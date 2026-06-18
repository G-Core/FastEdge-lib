use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use crate::executor;
use crate::executor::{HttpExecutor, X_CDN_REAL_HOST};
use crate::state::HttpState;
use ::http::{HeaderMap, Request, Response, Uri, header};
use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use http_backend::Backend;
use http_body_util::{BodyExt, Full};
use hyper::body::Body;
use runtime::util::stats::{StatsTimer, StatsVisitor};
use runtime::{InstancePre, store::StoreBuilder};
use smol_str::SmolStr;
use tracing::Instrument;
use wasmtime_wasi_http::bindings::ProxyPre;
use wasmtime_wasi_http::bindings::http::types::Scheme;
use wasmtime_wasi_http::{WasiHttpView, body::HyperOutgoingBody};

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
        // Start timing for stats
        let stats_timer = StatsTimer::new(stats.clone());

        let (sender, receiver) = tokio::sync::oneshot::channel();
        let (mut parts, body) = req.into_parts();

        let backend_hostname = self
            .backend
            .hostname()
            .context("backend hostname must be set")?;
        let backend_host_header = backend_hostname.parse().context("invalid hostname")?;
        let hostname = resolve_authority(&parts.headers, backend_hostname);

        // Promote a relative request URI (origin-form from hyper, e.g. `/foo`)
        // to an absolute URI so the wasi-http `incoming-request` resource
        // exposes a valid scheme + authority to the guest. `Uri::from_parts`
        // also requires authority once a scheme is set, so both fields must be
        // populated together.
        if parts.uri.scheme().is_none() {
            let mut uparts = parts.uri.clone().into_parts();
            uparts.scheme = Some(::http::uri::Scheme::HTTP);
            if uparts.authority.is_none() {
                uparts.authority = hostname.parse().ok()
            }
            parts.uri = Uri::from_parts(uparts)?;
        }

        //TODO: send streamed request body
        let body = body
            .collect()
            .await
            .map_err(|_| anyhow!("body read error"))?
            .to_bytes();
        let body = Full::new(body).map_err(|never| match never {});
        let body = body.boxed();

        let properties = executor::get_properties(&parts.headers);
        // Shared counter so wasi-http outbound calls (handled by
        // `WasiHttpView::send_request` in the runtime) refund epoch ticks
        // via the Store's `epoch_deadline_callback`.
        let epoch_pause_ms = Arc::new(AtomicU64::new(0));
        let store_builder = self
            .store_builder
            .with_properties(properties)
            .epoch_pause_ms(epoch_pause_ms.clone());
        let mut http_backend = self.backend;
        http_backend.set_epoch_pause_ms(epoch_pause_ms);

        if let Some(cdn_real_host) = parts
            .headers
            .get(X_CDN_REAL_HOST)
            .and_then(|v| v.to_str().ok())
        {
            http_backend.set_cdn_real_host(cdn_real_host.into());
        }

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

        propagate_headers.insert(header::HOST, backend_host_header);

        let backend_uri = http_backend.uri();
        let state = HttpState {
            http_backend,
            uri: backend_uri,
            propagate_headers,
            propagate_header_names,
            stats: stats.clone(),
        };

        let mut store = store_builder.build(state).context("store build")?;
        let instance_pre = self.instance_pre.clone();

        let request = Request::from_parts(parts, body);
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
        let task = tokio::task::spawn(
            async move {
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
                    // log to application logger  error
                    if let Some(ref logger) = store.data().logger {
                        logger.write_msg(format!("Execution error: {}", e)).await;
                    }
                    return Err(e);
                };

                drop(stats_timer); // Stop timing for stats
                task_stats.memory_used(store.memory_used() as u64);

                Ok(())
            }
            .in_current_span(),
        );

        match receiver.await {
            Ok(Ok(response)) => {
                stats.status_code(response.status().as_u16());
                Ok(response)
            }
            Ok(Err(error)) => Err(error.into()),
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

/// Resolve the authority used to absolutize the incoming request URI.
///
/// Precedence:
/// 1. `x-cdn-real-host` request header (if set and valid UTF-8).
/// 2. The backend's configured hostname stripped to its parent domain
///    (e.g. `app.example.com` -> `example.com`).
/// 3. The backend's configured hostname as-is when it contains no dot.
fn resolve_authority(headers: &HeaderMap, backend_hostname: SmolStr) -> SmolStr {
    headers
        .get(X_CDN_REAL_HOST)
        .and_then(|v| v.to_str().ok())
        .map(SmolStr::from)
        .unwrap_or_else(|| match backend_hostname.find('.') {
            None => backend_hostname,
            Some(i) => {
                let (_, domain) = backend_hostname.split_at(i + 1);
                SmolStr::from(domain)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::http::HeaderValue;

    fn headers_with_real_host(host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(X_CDN_REAL_HOST, host.parse().unwrap());
        h
    }

    /// `x-cdn-real-host` wins over the backend hostname.
    #[test]
    fn resolve_authority_prefers_real_host_header() {
        let headers = headers_with_real_host("override.test.com");
        let authority = resolve_authority(&headers, SmolStr::from("backend.example.com"));
        assert_eq!(authority, "override.test.com");
    }

    /// Without the header, the backend hostname is stripped to its parent domain.
    #[test]
    fn resolve_authority_falls_back_to_parent_domain() {
        let headers = HeaderMap::new();
        let authority = resolve_authority(&headers, SmolStr::from("app.example.com"));
        assert_eq!(authority, "example.com");
    }

    /// Multi-level subdomains: only the leftmost label is removed.
    #[test]
    fn resolve_authority_strips_only_leftmost_label() {
        let headers = HeaderMap::new();
        let authority = resolve_authority(&headers, SmolStr::from("a.b.c.example.com"));
        assert_eq!(authority, "b.c.example.com");
    }

    /// Single-label hostnames have no dot, so they're returned unchanged.
    #[test]
    fn resolve_authority_keeps_single_label_host() {
        let headers = HeaderMap::new();
        let authority = resolve_authority(&headers, SmolStr::from("localhost"));
        assert_eq!(authority, "localhost");
    }

    /// Non-UTF-8 header value is silently ignored: behaves like a missing header.
    #[test]
    fn resolve_authority_ignores_non_utf8_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            X_CDN_REAL_HOST,
            HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        let authority = resolve_authority(&headers, SmolStr::from("backend.example.com"));
        assert_eq!(authority, "example.com");
    }
}
