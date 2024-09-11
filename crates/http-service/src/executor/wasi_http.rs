use std::time::{Duration, Instant};

use crate::executor;
use crate::executor::HttpExecutor;
use crate::state::HttpState;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use bytesize::ByteSize;
use dictionary::Dictionary;
use http::uri::Scheme;
use http::{header, HeaderMap, Response, Uri};
use http_backend::Backend;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes};
use runtime::store::StoreBuilder;
use runtime::InstancePre;
use smol_str::ToSmolStr;
use tracing::error;
use wasmtime_wasi_http::WasiHttpView;

/// Execute context used by ['HttpService']
#[derive(Clone)]
pub struct WasiHttpExecutorImpl<C> {
    instance_pre: InstancePre<HttpState<C>>,
    store_builder: StoreBuilder,
    backend: Backend<C>,
    dictionary: Dictionary,
}

#[async_trait]
impl<C> HttpExecutor for WasiHttpExecutorImpl<C>
where
    C: Clone + Send + Sync + 'static,
{
    async fn execute<B>(
        &self,
        req: hyper::Request<B>,
    ) -> anyhow::Result<(Response<BoxBody<Bytes, anyhow::Error>>, Duration, ByteSize)>
    where
        B: BodyExt + Send,
        <B as Body>::Data: Send,
    {
        let start_ = Instant::now();
        let response = self.execute_impl(req).await;
        let elapsed = Instant::now().duration_since(start_);
        response.map(|(r, used)| (r, elapsed, used))
    }
}

impl<C> WasiHttpExecutorImpl<C>
where
    C: Clone + Send + Sync + 'static,
{
    pub fn new(
        instance_pre: InstancePre<HttpState<C>>,
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

    async fn execute_impl<B>(
        &self,
        req: hyper::Request<B>,
    ) -> anyhow::Result<(Response<BoxBody<Bytes, anyhow::Error>>, ByteSize)>
    where
        B: BodyExt,
    {
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

        let body = body
            .collect()
            .await
            .map_err(|_| anyhow!("body read error"))?
            .to_bytes();
        let body = Full::new(body).map_err(|never| match never {});
        let body = body.boxed();

        let properties = executor::get_properties(&parts.headers);
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
        };

        let mut store = store_builder.build(state).context("store build")?;
        let instance_pre = self.instance_pre.clone();

        let task = tokio::task::spawn(async move {
            let request = hyper::Request::from_parts(parts, body);
            let req = store
                .data_mut()
                .new_incoming_request(request)
                .context("new incoming request")?;
            let out = store
                .data_mut()
                .new_response_outparam(sender)
                .context("new response outparam")?;

            let (proxy, _inst) = wasmtime_wasi_http::proxy::Proxy::instantiate_pre(
                &mut store,
                instance_pre.as_ref(),
            )
            .await?;

            if let Err(e) = proxy
                .wasi_http_incoming_handler()
                .call_handle(&mut store, req, out)
                .await
            {
                error!(cause=?e, "incoming handler");
                return Err(e);
            };
            let used = ByteSize::b(store.memory_used() as u64);
            Ok(used)
        });

        match receiver.await {
            Ok(Ok(resp)) => {
                let (parts, body) = resp.into_parts();
                let body = body.map_err(anyhow::Error::msg).boxed();
                let used = task.await.context("task await")?.context("byte size")?;
                Ok((Response::from_parts(parts, body), used))
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
