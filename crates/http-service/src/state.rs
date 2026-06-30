use anyhow::Error;
use http::request::Parts;
use http::uri::Scheme;
use http::{HeaderMap, HeaderName, HeaderValue, Uri, header};
use http_backend::Backend;
use http_backend::{is_backend_host, is_fastedge_internal_header, is_public_host};
use runtime::BackendRequest;
use runtime::store::HasStats;
use runtime::util::stats::StatsVisitor;
use std::sync::Arc;
use tracing::instrument;

pub struct HttpState<C> {
    pub(super) http_backend: Backend<C>,
    pub(super) uri: Uri,
    pub(super) propagate_headers: HeaderMap,
    pub(super) propagate_header_names: Vec<HeaderName>,
    pub(super) stats: Arc<dyn StatsVisitor>,
}

// `HeaderName::from_static` is `const fn`, so the routing-header names can be
// declared once at module scope and reused everywhere without re-parsing.
const FASTEDGE_HOSTNAME: HeaderName = HeaderName::from_static("fastedge-hostname");
const FASTEDGE_SCHEME: HeaderName = HeaderName::from_static("fastedge-scheme");
// NOTE: `HeaderName::from_static` requires the input to be lowercase; the
// previous code preserved the mixed-case `Fastedge_Header_Hostname` purely as
// the display form, but `HeaderMap` lookups are case-insensitive so the
// lowercase form is equivalent on the wire.
const FASTEDGE_HEADER_HOSTNAME: HeaderName = HeaderName::from_static("fastedge_header_hostname");
const FASTEDGE_HOSTNAME_VALUE: HeaderValue = HeaderValue::from_static("127.0.0.1");
const FASTEDGE_APP_ID: HeaderName = HeaderName::from_static("fastedge-app-id");

impl<C> BackendRequest for HttpState<C> {
    #[instrument(skip(self, head), level = "debug", ret)]
    fn backend_request(&mut self, mut head: Parts) -> anyhow::Result<Parts> {
        match self.http_backend.strategy {
            http_backend::BackendStrategy::Direct => {
                tracing::trace!("direct send request original url: {:?}", head.uri);
                Ok(head)
            }
            http_backend::BackendStrategy::FastEdge => {
                let original_url = head.uri;
                tracing::trace!("backend send request original url: {:?}", original_url);
                let original_host = original_url.authority().map(|a| {
                    match (original_url.scheme_str(), a.port().map(|p| p.as_u16())) {
                        (None, Some(80))
                        | (Some("http"), Some(80))
                        | (Some("https"), Some(443))
                        | (_, None) => a.host().to_string(),
                        (_, Some(port)) => format!("{}:{}", a.host(), port),
                    }
                });
                let request_host_header_value = head
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq(&header::HOST))
                    .map(|(_, v)| v.to_owned());

                let original_host = original_host.or_else(|| {
                    request_host_header_value
                        .as_ref()
                        .and_then(|v| v.to_str().map(|v| v.to_string()).ok())
                });

                anyhow::ensure!(
                    original_host.is_some(),
                    "host is required for backend request"
                );
                let original_host = original_host.unwrap();

                anyhow::ensure!(
                    is_public_host(&original_host),
                    "private host not allowed: {}",
                    original_host
                );

                if let Some(ref hostname) = self.http_backend.hostname() {
                    tracing::debug!(
                        "backend request original url: {:?}, original host: {}",
                        hostname,
                        original_host
                    );
                    anyhow::ensure!(
                        !is_backend_host(&original_host, hostname.as_str()),
                        "direct access to backend resource is not allowed: {}",
                        original_host
                    );
                }

                // When the outbound call targets the CDN "real host" (the host of the
                // original end-user request, carried in the `X-Cdn-Real-Host` header) on a
                // default port, the backend resource is the app itself. In that case route
                // the request back to `localhost`, expose the real host via
                // `Fastedge_Header_Hostname`, and ignore any `Host` header the app set.
                let self_binding = self
                    .http_backend
                    .cdn_real_host()
                    .zip(original_url.host())
                    .is_some_and(|(cdn_real_host, url_host)| {
                        let port_ok =
                            matches!(original_url.port_u16(), None | Some(80) | Some(443));
                        port_ok && cdn_real_host.eq_ignore_ascii_case(url_host)
                    });

                // Strip internal routing headers and all headers with the
                // reserved `fastedge` prefix so guests cannot spoof internal metadata.
                static FILTER_HEADERS: [HeaderName; 4] = [
                    header::HOST,
                    header::CONTENT_LENGTH,
                    header::TRANSFER_ENCODING,
                    header::UPGRADE,
                ];

                // filter headers
                let mut headers = head
                    .headers
                    .into_iter()
                    .filter_map(|(k, v)| k.map(|k| (k, v)))
                    .filter(|(k, _)| {
                        !FILTER_HEADERS.contains(k)
                            && !self.propagate_header_names.contains(k)
                            && !is_fastedge_internal_header(k.as_str())
                    })
                    .collect::<HeaderMap>();

                if self_binding {
                    tracing::debug!(
                        "found backend request original url host matches cdn real host, applying self-binding logic"
                    );
                    // URL host is guaranteed present here (checked above).
                    let url_host = original_url.host().unwrap_or_default();
                    headers.insert(FASTEDGE_HOSTNAME, FASTEDGE_HOSTNAME_VALUE);
                    headers.insert(
                        FASTEDGE_SCHEME,
                        original_url.scheme_str().unwrap_or("http").parse()?,
                    );
                    headers.insert(FASTEDGE_HEADER_HOSTNAME, url_host.parse()?);
                } else {
                    headers.insert(FASTEDGE_HOSTNAME, original_host.parse()?);
                    headers.insert(
                        FASTEDGE_SCHEME,
                        original_url.scheme_str().unwrap_or("http").parse()?,
                    );
                    //When HTTP app sets Host header, Fastegde needs to set Fastedge_Header_Hostname header for BE.
                    if let Some(request_host_header) = request_host_header_value {
                        headers.insert(FASTEDGE_HEADER_HOSTNAME, request_host_header);
                    }
                }

                headers.extend(self.propagate_headers.clone());

                // Inject the application identifier so the backend can
                // correlate the request with the originating FastEdge app.
                if let Some(app_id) = self.http_backend.app_id() {
                    headers.insert(FASTEDGE_APP_ID, app_id.to_string().parse()?);
                }

                let authority = self
                    .http_backend
                    .uri()
                    .authority()
                    .map(|a| a.as_str().to_string())
                    .unwrap_or("localhost:10080".to_string());
                let uri = canonical_url(&original_url, &authority, self.uri.path())?;

                head.uri = uri;
                head.headers = headers;

                Ok(head)
            }
        }
    }
}

// make canonical uri for backend
fn canonical_url(
    original_url: &Uri,
    canonical_host: &str,
    backend_path: &str,
) -> anyhow::Result<Uri> {
    let original_path_and_query = original_url.path_and_query().map(|p| p.as_str());
    let mut canonical_path = String::new();
    let canonical_uri = Uri::builder().scheme(Scheme::HTTP);

    // We get the authority from the canonical host. In some cases that might actually come
    // from the `original_uri`, but usually it's from an explicit `Host` header.
    let canonical_uri = canonical_uri.authority(canonical_host);

    // The path begins with the "path prefix" present in the backend's URI. This is often just
    // an empty path or `/`.
    canonical_path.push_str(backend_path);
    if !canonical_path.ends_with('/') {
        canonical_path.push('/');
    }

    // Finally we incorporate the requested path, taking care not to introduce extra `/`
    // separators when gluing things together.
    if let Some(original_path_and_query) = original_path_and_query {
        if let Some(stripped) = original_path_and_query.strip_prefix('/') {
            canonical_path.push_str(stripped)
        } else {
            canonical_path.push_str(original_path_and_query)
        }
    }

    canonical_uri
        .path_and_query(canonical_path)
        .build()
        .map_err(Error::msg)
}

impl<T> HasStats for HttpState<T> {
    fn get_stats(&self) -> Arc<dyn StatsVisitor> {
        self.stats.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Request;
    use http_backend::stats::ExtRequestStats;
    use http_backend::{BackendStrategy, FastEdgeConnector};
    use key_value_store::ReadStats;
    use runtime::util::stats::CdnPhase;
    use std::time::Duration;
    use utils::UserDiagStats;

    #[derive(Clone)]
    struct TestStats;
    impl ReadStats for TestStats {
        fn count_kv_read(&self, _value: i32) {}
        fn count_kv_byod_read(&self, _value: i32) {}
    }
    impl UserDiagStats for TestStats {
        fn set_user_diag(&self, _diag: &str) {}
    }
    impl ExtRequestStats for TestStats {
        fn observe_ext(&self, _elapsed: Duration) {}
    }
    impl StatsVisitor for TestStats {
        fn status_code(&self, _status_code: u16) {}
        fn memory_used(&self, _memory_used: u64) {}
        fn fail_reason(&self, _fail_reason: i32) {}
        fn observe(&self, _elapsed: Duration) {}
        fn get_time_elapsed(&self) -> u64 {
            0
        }
        fn get_memory_used(&self) -> u64 {
            0
        }
        fn cdn_phase(&self, _phase: CdnPhase) {}
    }

    fn make_state(cdn_real_host: Option<&str>) -> HttpState<FastEdgeConnector> {
        let connector = FastEdgeConnector::new("http://be.server/".parse().unwrap());
        let mut http_backend = Backend::<FastEdgeConnector>::builder(BackendStrategy::FastEdge)
            .hostname("be.server")
            .uri("http://be.server/".parse().unwrap())
            .build(connector);
        if let Some(cdn_real_host) = cdn_real_host {
            http_backend.set_cdn_real_host(cdn_real_host.into());
        }
        let backend_uri = http_backend.uri();
        HttpState {
            http_backend,
            uri: backend_uri,
            propagate_headers: HeaderMap::new(),
            propagate_header_names: vec![],
            stats: Arc::new(TestStats),
        }
    }

    fn parts(uri: &str, headers: &[(&str, &str)]) -> Parts {
        let mut builder = Request::builder().uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(()).unwrap().into_parts().0
    }

    fn header<'a>(parts: &'a Parts, name: &str) -> Option<&'a str> {
        parts.headers.get(name).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn self_binding_rewrites_routing_headers() {
        let mut state = make_state(Some("example.com"));
        let out = state
            .backend_request(parts("http://example.com/path", &[]))
            .unwrap();
        assert_eq!(header(&out, "fastedge-hostname"), Some("127.0.0.1"));
        assert_eq!(header(&out, "fastedge-scheme"), Some("http"));
        assert_eq!(
            header(&out, "fastedge_header_hostname"),
            Some("example.com")
        );
    }

    #[test]
    fn self_binding_ignores_app_host_header() {
        let mut state = make_state(Some("example.com"));
        let out = state
            .backend_request(parts(
                "https://example.com:443/path",
                &[("host", "app-set-host.com")],
            ))
            .unwrap();
        assert_eq!(header(&out, "fastedge-hostname"), Some("127.0.0.1"));
        assert_eq!(header(&out, "fastedge-scheme"), Some("https"));
        // real host comes from the URL, not the app-provided Host header
        assert_eq!(
            header(&out, "fastedge_header_hostname"),
            Some("example.com")
        );
    }

    #[test]
    fn non_default_port_skips_self_binding() {
        let mut state = make_state(Some("example.com"));
        let out = state
            .backend_request(parts("http://example.com:8080/path", &[]))
            .unwrap();
        assert_eq!(header(&out, "fastedge-hostname"), Some("example.com:8080"));
        assert_eq!(header(&out, "fastedge_header_hostname"), None);
    }

    #[test]
    fn mismatch_skips_self_binding() {
        let mut state = make_state(Some("other.com"));
        let out = state
            .backend_request(parts("http://example.com/path", &[]))
            .unwrap();
        assert_eq!(header(&out, "fastedge-hostname"), Some("example.com"));
        assert_eq!(header(&out, "fastedge_header_hostname"), None);
    }

    #[test]
    fn no_cdn_real_host_keeps_default_behavior() {
        let mut state = make_state(None);
        let out = state
            .backend_request(parts("http://example.com/path", &[("host", "app.com")]))
            .unwrap();
        assert_eq!(header(&out, "fastedge-hostname"), Some("example.com"));
        // app Host header is surfaced via Fastedge_Header_Hostname in the default path
        assert_eq!(header(&out, "fastedge_header_hostname"), Some("app.com"));
    }

    #[test]
    fn app_provided_fastedge_header_hostname_is_stripped() {
        // Without self-binding and without a Host header, FastEdge does not set
        // its own `fastedge_header_hostname`. Any value the app supplied must
        // still be filtered out so it cannot spoof the backend's real-host view.
        let mut state = make_state(None);
        let out = state
            .backend_request(parts(
                "http://example.com/path",
                &[("fastedge_header_hostname", "evil.example.com")],
            ))
            .unwrap();
        assert_eq!(header(&out, "fastedge_header_hostname"), None);
    }

    #[test]
    fn app_cannot_override_fastedge_header_hostname_in_self_binding() {
        let mut state = make_state(Some("example.com"));
        let out = state
            .backend_request(parts(
                "http://example.com/path",
                &[("fastedge_header_hostname", "evil.example.com")],
            ))
            .unwrap();
        // FastEdge's own value wins; the app-provided one is filtered first.
        assert_eq!(
            header(&out, "fastedge_header_hostname"),
            Some("example.com")
        );
    }

    #[test]
    fn direct_access_to_backend_host_is_rejected() {
        let mut state = make_state(None);
        let result = state.backend_request(parts("http://be.server/path", &[]));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("direct access to backend resource is not allowed"),
            "unexpected error: {}",
            err_msg
        );
    }

    #[test]
    fn fastedge_prefixed_headers_are_stripped() {
        let mut state = make_state(None);
        let out = state
            .backend_request(parts(
                "http://example.com/path",
                &[
                    ("fastedge_custom_header", "should-be-removed"),
                    ("fastedge-secret", "should-be-removed"),
                    ("x-custom-header", "should-remain"),
                ],
            ))
            .unwrap();
        // All fastedge-prefixed headers from the guest are stripped
        assert_eq!(header(&out, "fastedge_custom_header"), None);
        assert_eq!(header(&out, "fastedge-secret"), None);
        // Non-fastedge headers pass through
        assert_eq!(header(&out, "x-custom-header"), Some("should-remain"));
    }

    #[test]
    fn fastedge_app_id_is_injected_when_set() {
        let connector = FastEdgeConnector::new("http://be.server/".parse().unwrap());
        let mut http_backend = Backend::<FastEdgeConnector>::builder(BackendStrategy::FastEdge)
            .hostname("be.server")
            .uri("http://be.server/".parse().unwrap())
            .build(connector);
        http_backend.set_app_id(42);
        let backend_uri = http_backend.uri();
        let mut state = HttpState {
            http_backend,
            uri: backend_uri,
            propagate_headers: HeaderMap::new(),
            propagate_header_names: vec![],
            stats: Arc::new(TestStats),
        };
        let out = state
            .backend_request(parts("http://example.com/path", &[]))
            .unwrap();
        assert_eq!(header(&out, "fastedge-app-id"), Some("42"));
    }

    #[test]
    fn fastedge_app_id_not_present_when_unset() {
        let mut state = make_state(None);
        let out = state
            .backend_request(parts("http://example.com/path", &[]))
            .unwrap();
        assert_eq!(header(&out, "fastedge-app-id"), None);
    }
}
