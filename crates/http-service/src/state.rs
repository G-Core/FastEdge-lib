use anyhow::Error;
use http::request::Parts;
use http::uri::Scheme;
use http::{header, HeaderMap, HeaderName, Uri};
use http_backend::Backend;
use runtime::store::HasStats;
use runtime::util::stats::StatsVisitor;
use runtime::BackendRequest;
use std::sync::Arc;
use tracing::instrument;

pub struct HttpState<C> {
    pub(super) http_backend: Backend<C>,
    pub(super) uri: Uri,
    pub(super) propagate_headers: HeaderMap,
    pub(super) propagate_header_names: Vec<HeaderName>,
    pub(super) stats: Arc<dyn StatsVisitor>,
}

impl<C> BackendRequest for HttpState<C> {
    #[instrument(skip(self), ret, err)]
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
                let original_host = original_host
                    .or_else(|| {
                        head.headers.iter().find_map(|(k, v)| {
                            if k.as_str().eq_ignore_ascii_case("host") {
                                v.to_str().ok().map(|c| c.to_string())
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or_default();

                static FILTER_HEADERS: [HeaderName; 6] = [
                    header::HOST,
                    header::CONTENT_LENGTH,
                    header::TRANSFER_ENCODING,
                    header::UPGRADE,
                    HeaderName::from_static("fastedge-hostname"),
                    HeaderName::from_static("fastedge-scheme"),
                ];

                // filter headers
                let mut headers = head
                    .headers
                    .into_iter()
                    .filter_map(|(k, v)| k.map(|k| (k, v)))
                    .filter(|(k, _)| {
                        !FILTER_HEADERS.contains(k) && !self.propagate_header_names.contains(k)
                    })
                    .collect::<HeaderMap>();

                headers.insert(
                    HeaderName::from_static("fastedge-hostname"),
                    original_host.parse()?,
                );
                headers.insert(
                    HeaderName::from_static("fastedge-scheme"),
                    original_url.scheme_str().unwrap_or("http").parse()?,
                );

                headers.extend(self.propagate_headers.clone());

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
