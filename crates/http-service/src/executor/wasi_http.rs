use std::sync::Arc;
use std::time::Duration;

use crate::executor;
use crate::executor::HttpExecutor;
use crate::state::HttpState;
use ::http::{header, HeaderMap, Request, Response, Uri};
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use http_backend::Backend;
use http_body_util::{BodyExt, Full};
use hyper::body::Body;
use runtime::util::stats::{StatsTimer, StatsVisitor};
use runtime::{store::StoreBuilder, InstancePre};
use wasmtime_wasi_http::bindings::http::types::Scheme;
use wasmtime_wasi_http::bindings::ProxyPre;
use wasmtime_wasi_http::{body::HyperOutgoingBody, WasiHttpView};

/// Execute context used by ['HttpService']
#[derive(Clone)]
pub struct WasiHttpExecutorImpl<C: 'static> {
    instance_pre: InstancePre<HttpState<C>>,
    store_builder: StoreBuilder,
    backend: Backend<C>,
}

#[async_trait]
impl<C> HttpExecutor for WasiHttpExecutorImpl<C>
where
    C: Clone + Send + Sync + 'static,
{
    async fn execute<B>(
        self,
        req: Request<B>,
        stats: Arc<dyn StatsVisitor>,
    ) -> anyhow::Result<Response<HyperOutgoingBody>>
    where
        B: BodyExt + Send,
        <B as Body>::Data: Send,
    {
        tracing::trace!("start execute");
        // Start timing for stats
        let stats_timer = StatsTimer::new(stats.clone());

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

        //FIXME send streamed request body
        let body = body
            .collect()
            .await
            .map_err(|_| anyhow!("body read error"))?
            .to_bytes();
        let body = Full::new(body).map_err(|never| match never {});
        let body = body.boxed();

        let properties = executor::get_properties(&parts.headers);
        let store_builder = self.store_builder.with_properties(properties);
        let mut http_backend = self.backend;

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

        let task_stats = stats.clone();
        let task = tokio::task::spawn(async move {
            let duration = Duration::from_millis(store.data().timeout);
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

            drop(stats_timer); // Stop timing for stats
            task_stats.memory_used(store.memory_used() as u64);

            Ok(())
        });

        match receiver.await {
            Ok(Ok(response)) => {
                stats.status_code(response.status().as_u16());
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

impl<C> WasiHttpExecutorImpl<C>
where
    C: Clone + Send + Sync + 'static,
{
    pub fn new(
        instance_pre: InstancePre<HttpState<C>>,
        store_builder: StoreBuilder,
        backend: Backend<C>,
    ) -> Self {
        Self {
            instance_pre,
            store_builder,
            backend,
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
