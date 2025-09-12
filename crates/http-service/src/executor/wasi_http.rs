use std::time::{Duration, Instant};

use crate::executor::HttpExecutor;
use crate::state::HttpState;
use ::http::{header, HeaderMap, Request, Response, StatusCode, Uri};
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use bytesize::ByteSize;
use dictionary::Dictionary;
use http_backend::Backend;
use http_body_util::{BodyExt, Full};
use hyper::body::Body;
use runtime::{store::StoreBuilder, InstancePre};
use wasmtime_wasi_http::bindings::http::types::Scheme;
use wasmtime_wasi_http::bindings::ProxyPre;
use wasmtime_wasi_http::{body::HyperOutgoingBody, WasiHttpView};
use key_value_store::StoreManager;

/// Execute context used by ['HttpService']
#[derive(Clone)]
pub struct WasiHttpExecutorImpl<C: 'static, M: StoreManager +'static> {
    instance_pre: InstancePre<HttpState<C, M>>,
    store_builder: StoreBuilder,
    backend: Backend<C>,
    dictionary: Dictionary,
}

#[async_trait]
impl<C, M> HttpExecutor for WasiHttpExecutorImpl<C, M>
where
    C: Clone + Send + Sync + 'static,
    M: StoreManager + Default
{
    async fn execute<B, R>(
        &self,
        req: Request<B>,
        on_response: R,
    ) -> anyhow::Result<Response<HyperOutgoingBody>>
    where
        R: FnOnce(StatusCode, ByteSize, Duration) + Send + 'static,
        B: BodyExt + Send,
        <B as Body>::Data: Send,
    {
        tracing::trace!("start execute");
        let start_ = Instant::now();

        let (sender, receiver) = tokio::sync::oneshot::channel();
        let (mut parts, body) = req.into_parts();

        let server_name = parts
            .headers
            .get("server_name")
            .and_then(|v| v.to_str().ok())
            .ok_or(anyhow!("header Server_name is missing"))?;

        // fix relative uri to absolute
        if parts.uri.scheme().is_none() {
            let mut uparts = parts.uri.clone().into_parts();
            uparts.scheme = Some(::http::uri::Scheme::HTTP);
            if uparts.authority.is_none() {
                uparts.authority = server_name.parse().ok()
            }
            parts.uri = Uri::from_parts(uparts)?;
        }

        //TODO send streamed request body
        let body = body
            .collect()
            .await
            .map_err(|_| anyhow!("body read error"))?
            .to_bytes();
        let body = Full::new(body).map_err(|never| match never {});
        let body = body.boxed();

        let properties = crate::executor::get_properties(&parts.headers);
        let store_builder = self.store_builder.to_owned().with_properties(properties);
        let mut http_backend = self.backend.to_owned();

        http_backend
            .propagate_headers(parts.headers.clone())
            .context("propagate headers")?;

        let propagate_header_names = http_backend.propagate_header_names();
        let mut propagate_headers: HeaderMap = parts
            .headers
            .iter()
            .filter_map(|(k, v)| {
                if propagate_header_names.contains(k) {
                    Some((k.to_owned(), v.to_owned()))
                } else {
                    None
                }
            })
            .collect();

        propagate_headers.insert(header::HOST, be_base_domain(server_name).parse()?);

        let backend_uri = http_backend.uri();
        let state = HttpState {
            http_backend,
            uri: backend_uri,
            propagate_headers,
            propagate_header_names,
            dictionary: self.dictionary.clone(),
            key_value_store: Default::default(),
        };

        let mut store = store_builder.build(state).context("store build")?;
        let instance_pre = self.instance_pre.clone();

        let request = hyper::Request::from_parts(parts, body);
        let req = store
            .data_mut()
            .new_incoming_request(Scheme::Http, request)
            .context("new incoming request")?;
        let out = store
            .data_mut()
            .new_response_outparam(sender)
            .context("new response outparam")?;
        let proxy_pre = ProxyPre::new(instance_pre)?;

        let proxy = proxy_pre.instantiate_async(&mut store).await?;

        let duration = Duration::from_millis(store.data().timeout);

        /*
            Channel to receive http status code for asynchronious response processing of WASI-HTTP.
        */
        let (status_code_tx, status_code_rx) = tokio::sync::oneshot::channel();

        let task = tokio::task::spawn(async move {
            if let Err(e) = tokio::time::timeout(
                duration,
                proxy
                    .wasi_http_incoming_handler()
                    .call_handle(&mut store, req, out),
            )
            .await?
            {
                tracing::warn!(cause=?e, "incoming handler");
                return Err(e);
            };
            let elapsed = Instant::now().duration_since(start_);
            /*
                Used by WASI-HTTP to send status code prior response processing.
                If there is no status code then default value is returned.
                For synchronious HTTP processing the status_code parameter is set and no value in the channel.
            */
            let status_code = status_code_rx.await.unwrap_or_else(|error| {
                tracing::trace!(cause=?error, "unknown status code");
                StatusCode::default()
            });

            on_response(
                status_code,
                ByteSize::b(store.memory_used() as u64),
                elapsed,
            );
            Ok(())
        });

        match receiver.await {
            Ok(Ok(response)) => {
                /*
                   Status code sender is closed if response handler processing was done.
                */
                if !status_code_tx.is_closed() {
                    if let Err(error) = status_code_tx.send(response.status()) {
                        tracing::warn!(cause=?error, "sending status code")
                    }
                    tracing::debug!("returned status code: '{}'", response.status(),);
                } else {
                    tracing::warn!("status code sender is closed");
                }

                Ok(response)
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                let e = match task.await {
                    Ok(r) => {
                        r.expect_err("if the receiver has an error, the task must have failed")
                    }
                    Err(e) => e.into(),
                };
                bail!("guest never invoked `response-outparam::set` method: {e:?}")
            }
        }
    }
}

impl<C, M> WasiHttpExecutorImpl<C, M>
where
    C: Clone + Send + Sync + 'static,
    M: StoreManager
{
    pub fn new(
        instance_pre: InstancePre<HttpState<C, M>>,
        store_builder: StoreBuilder,
        backend: Backend<C>,
        dictionary: Dictionary,
    ) -> Self {
        Self {
            instance_pre,
            store_builder,
            backend,
            dictionary,
        }
    }
}

fn be_base_domain(server_name: &str) -> String {
    let base_domain = match server_name.find('.') {
        None => server_name,
        Some(i) => {
            let (_, domain) = server_name.split_at(i + 1);
            domain
        }
    };
    format!("be.{}", base_domain)
}
