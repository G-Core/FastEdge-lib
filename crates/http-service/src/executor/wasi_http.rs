use std::time::{Duration, Instant};

use crate::executor::HttpExecutor;
use crate::state::HttpState;
use ::http::{header, uri::Scheme, HeaderMap, Request, Response, StatusCode, Uri};
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use bytesize::ByteSize;
use dictionary::Dictionary;
use http_backend::Backend;
use http_body_util::{BodyExt, Full};
use hyper::body::Body;
use runtime::{store::StoreBuilder, InstancePre};
use secret::{Secret, SecretStrategy};
use smol_str::ToSmolStr;
use wasmtime_wasi_http::{body::HyperOutgoingBody, WasiHttpView};

/// Execute context used by ['HttpService']
#[derive(Clone)]
pub struct WasiHttpExecutorImpl<C, T: SecretStrategy> {
    instance_pre: InstancePre<HttpState<C, T>>,
    store_builder: StoreBuilder,
    backend: Backend<C>,
    dictionary: Dictionary,
    secret: Secret<T>,
}

#[async_trait]
impl<C, T> HttpExecutor for WasiHttpExecutorImpl<C, T>
where
    C: Clone + Send + Sync + 'static,
    T: SecretStrategy + Clone + Send + Sync + 'static,
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
            uparts.scheme = Some(Scheme::HTTP);
            if uparts.authority.is_none() {
                uparts.authority = server_name.parse().ok()
            }
            parts.uri = Uri::from_parts(uparts)?;
        }

        //FIXME send streamed request body
        let body = body
            .collect()
            .await
            .map_err(|_| anyhow!("body read error"))?
            .to_bytes();
        let body = Full::new(body).map_err(|never| match never {});
        let body = body.boxed();

        let properties = crate::executor::get_properties(&parts.headers);
        let store_builder = self.store_builder.to_owned().with_properties(properties);
        let wasi_nn = self
            .store_builder
            .make_wasi_nn_ctx()
            .context("make_wasi_nn_ctx")?;
        let mut http_backend = self.backend.to_owned();

        http_backend
            .propagate_headers(parts.headers.clone())
            .context("propagate headers")?;

        let propagate_header_names = http_backend.propagate_header_names();
        let mut propagate_headers: HeaderMap = parts
            .headers
            .iter()
            .filter_map(|(k, v)| {
                if propagate_header_names.contains(&k.to_smolstr()) {
                    Some((k.to_owned(), v.to_owned()))
                } else {
                    None
                }
            })
            .collect();

        propagate_headers.insert(header::HOST, be_base_domain(server_name).parse()?);

        let backend_uri = http_backend.uri();
        let state = HttpState {
            wasi_nn,
            http_backend,
            uri: backend_uri,
            propagate_headers,
            propagate_header_names,
            dictionary: self.dictionary.clone(),
            secret: self.secret.clone(),
        };

        let mut store = store_builder.build(state).context("store build")?;
        let instance_pre = self.instance_pre.clone();

        let request = hyper::Request::from_parts(parts, body);
        let req = store
            .data_mut()
            .new_incoming_request(request)
            .context("new incoming request")?;
        let out = store
            .data_mut()
            .new_response_outparam(sender)
            .context("new response outparam")?;

        let (proxy, _inst) =
            wasmtime_wasi_http::proxy::Proxy::instantiate_pre(&mut store, instance_pre.as_ref())
                .await?;

        let duration = Duration::from_millis(store.data().timeout);

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
            on_response(
                StatusCode::default(),
                ByteSize::b(store.memory_used() as u64),
                elapsed,
            );
            Ok(())
        });

        match receiver.await {
            Ok(Ok(response)) => Ok(response),
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

impl<C, T> WasiHttpExecutorImpl<C, T>
where
    C: Clone + Send + Sync + 'static,
    T: SecretStrategy + Clone + Send + 'static,
{
    pub fn new(
        instance_pre: InstancePre<HttpState<C, T>>,
        store_builder: StoreBuilder,
        backend: Backend<C>,
        dictionary: Dictionary,
        secret: Secret<T>,
    ) -> Self {
        Self {
            instance_pre,
            store_builder,
            backend,
            dictionary,
            secret,
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
