use std::fmt::Debug;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, Error, Result};
use http::{header, uri::Scheme, HeaderMap, HeaderName, Uri};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::rt::ReadBufCursor;
use hyper_util::client::legacy::connect::{Connect, HttpConnector};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use pin_project::pin_project;
use tokio::net::TcpStream;
use tower_service::Service;
use tracing::{debug, trace, warn};

use reactor::gcore::fastedge::http::Headers;
use reactor::gcore::fastedge::{
    http::{Error as HttpError, Method, Request, Response},
    http_client::Host,
};

type HeaderNameList = Vec<HeaderName>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendStrategy {
    Direct,
    FastEdge,
}

#[pin_project]
pub struct Connection {
    #[pin]
    inner: TcpStream,
}

/// A custom Hyper client connector, which is needed to override Hyper's default behavior of
/// connecting to host specified by the request's URI; we instead want to connect to the host
/// specified by our backend configuration, regardless of what the URI says.
#[derive(Clone, Debug)]
pub struct FastEdgeConnector {
    inner: HttpConnector,
    backend: Uri,
}

#[derive(Clone, Debug)]
pub struct Backend<C> {
    client: Client<C, Full<Bytes>>,
    uri: Uri,
    propagate_headers: HeaderMap,
    propagate_header_names: HeaderNameList,
    max_sub_requests: usize,
    pub strategy: BackendStrategy,
}

pub struct Builder {
    uri: Uri,
    propagate_header_names: HeaderNameList,
    max_sub_requests: usize,
    strategy: BackendStrategy,
}

impl Builder {
    pub fn uri(&mut self, uri: Uri) -> &mut Self {
        self.uri = uri;
        self
    }
    pub fn propagate_headers_names(&mut self, propagate: HeaderNameList) -> &mut Self {
        self.propagate_header_names = propagate;
        self
    }
    pub fn max_sub_requests(&mut self, max_sub_requests: usize) -> &mut Self {
        self.max_sub_requests = max_sub_requests;
        self
    }

    pub fn build<C>(&self, connector: C) -> Backend<C>
    where
        C: Connect + Clone,
    {
        let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
            .set_host(false)
            .pool_idle_timeout(Duration::from_secs(30))
            .build(connector);

        Backend {
            client,
            uri: self.uri.to_owned(),
            propagate_headers: HeaderMap::new(),
            propagate_header_names: self.propagate_header_names.to_owned(),
            max_sub_requests: self.max_sub_requests,
            strategy: self.strategy,
        }
    }
}

impl<C> Backend<C> {
    pub fn builder(strategy: BackendStrategy) -> Builder {
        Builder {
            uri: Uri::default(),
            propagate_header_names: vec![],
            max_sub_requests: usize::MAX,
            strategy,
        }
    }

    pub fn uri(&self) -> Uri {
        self.uri.to_owned()
    }

    pub fn propagate_header_names(&self) -> HeaderNameList {
        self.propagate_header_names.clone()
    }

    /// Propagate filtered headers from original requests
    pub fn propagate_headers(&mut self, headers: HeaderMap) -> Result<()> {
        self.propagate_headers.clear();

        if self.strategy == BackendStrategy::FastEdge {
            let server_name = headers
                .get("server_name")
                .and_then(|v| v.to_str().ok())
                .ok_or(anyhow!("header Server_name is missing"))?;
            self.propagate_headers.insert(
                HeaderName::from_static("host"),
                be_base_domain(server_name).parse()?,
            );
        }
        let headers = headers.into_iter().filter(|(k, _)| {
            if let Some(name) = k {
                self.propagate_header_names.contains(name)
            } else {
                false
            }
        });
        self.propagate_headers.extend(headers);

        Ok(())
    }

    fn propagate_headers_vec(&self) -> Vec<(String, String)> {
        self.propagate_headers
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
            .collect::<Vec<(String, String)>>()
    }

    fn make_request(&self, req: Request) -> Result<http::Request<Full<Bytes>>> {
        trace!("strategy: {:?}", self.strategy);
        let builder = match self.strategy {
            BackendStrategy::Direct => {
                let mut headers = req.headers.into_iter().collect::<Vec<(String, String)>>();
                headers.extend(self.propagate_headers_vec());
                // CLI has to set Host header from URL, if it is not set already by the request
                if !headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case(header::HOST.as_str()))
                {
                    if let Ok(uri) = req.uri.parse::<Uri>() {
                        if let Some(host) = uri.authority().map(|a| {
                            if let Some(port) = a.port() {
                                format!("{}:{}", a.host(), port)
                            } else {
                                a.host().to_string()
                            }
                        }) {
                            headers.push((header::HOST.as_str().to_string(), host))
                        }
                    }
                }

                let builder = http::Request::builder().uri(req.uri);

                let builder = match req.method {
                    Method::Get => builder.method(http::Method::GET),
                    Method::Post => builder.method(http::Method::POST),
                    Method::Put => builder.method(http::Method::PUT),
                    Method::Delete => builder.method(http::Method::DELETE),
                    Method::Head => builder.method(http::Method::HEAD),
                    Method::Patch => builder.method(http::Method::PATCH),
                    Method::Options => builder.method(http::Method::OPTIONS),
                };
                headers
                    .into_iter()
                    .fold(builder, |builder, (k, v)| builder.header(k, v))
            }
            BackendStrategy::FastEdge => {
                let original_url = req.uri.parse::<Uri>()?;
                trace!("send request original url: {:?}", original_url);
                let original_host = original_url.authority().map(|a| {
                    if let Some(port) = a.port() {
                        format!("{}:{}", a.host(), port)
                    } else {
                        a.host().to_string()
                    }
                });
                let request_host_header = req.headers.iter().find_map(|(k, v)| {
                    if k.eq_ignore_ascii_case("host") {
                        Some(v.to_owned())
                    } else {
                        None
                    }
                });

                let original_host = original_host
                    .or_else(|| request_host_header.clone())
                    .unwrap_or_default();

                anyhow::ensure!(
                    is_public_host(&original_host),
                    "private host not allowed: {}",
                    original_host
                );

                // filter headers
                let mut headers = req
                    .headers
                    .into_iter()
                    .map(|(k, v)| (k.to_lowercase(), v))
                    .filter(|(k, _)| {
                        !matches!(
                            k.as_str(),
                            "host"
                                | "content-length"
                                | "transfer-encoding"
                                | "fastedge-hostname"
                                | "fastedge-scheme"
                        )
                    })
                    .filter(|(k, _)| {
                        !self
                            .propagate_header_names
                            .iter()
                            .any(|name| name.eq(k.as_str()))
                    })
                    .collect::<Vec<(String, String)>>();

                headers.push(("fastedge-hostname".to_string(), original_host));
                headers.push((
                    "fastedge-scheme".to_string(),
                    original_url.scheme_str().unwrap_or("http").to_string(),
                ));
                //When HTTP app sets Host header, Fastegde needs to set Fastedge_Header_Hostname header for BE.
                if let Some(request_host_header) = request_host_header {
                    headers.push(("Fastedge_Header_Hostname".to_string(), request_host_header));
                }
                headers.extend(self.propagate_headers_vec());

                let host = canonical_host_name(&headers, &original_url)?;
                let url = canonical_url(&original_url, &host, self.uri.path())?;

                let builder = http::Request::builder().uri(url);

                let builder = match req.method {
                    Method::Get => builder.method(http::Method::GET),
                    Method::Post => builder.method(http::Method::POST),
                    Method::Put => builder.method(http::Method::PUT),
                    Method::Delete => builder.method(http::Method::DELETE),
                    Method::Head => builder.method(http::Method::HEAD),
                    Method::Patch => builder.method(http::Method::PATCH),
                    Method::Options => builder.method(http::Method::OPTIONS),
                };
                headers
                    .into_iter()
                    .fold(builder, |builder, (k, v)| builder.header(k, v))
            }
        };
        debug!("request builder: {:?}", builder);
        let body = req.body.unwrap_or_default();
        Ok(builder.body(Full::new(Bytes::from(body)))?)
    }
}

impl<C> Host for Backend<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    async fn send_request(&mut self, req: Request) -> Result<Response, HttpError> {
        // check the limit of sub requests
        if self.max_sub_requests == 0 {
            return Err(HttpError::TooManyRequests);
        } else {
            self.max_sub_requests -= 1;
        }

        let request = self.make_request(req).map_err(|error| {
            warn!(cause=?error, "making request to backend");
            HttpError::RequestError
        })?;
        let res = self.client.request(request).await.map_err(|error| {
            warn!(cause=?error, "sending request to backend");
            HttpError::RequestError
        })?;

        let status = res.status().as_u16();
        let (parts, body) = res.into_parts();
        let headers = if !parts.headers.is_empty() {
            Some(
                parts
                    .headers
                    .iter()
                    .filter_map(|(name, value)| match value.to_str() {
                        Ok(value) => Some((name.to_string(), value.to_string())),
                        Err(error) => {
                            warn!(cause=?error, "invalid value: {:?}", value);
                            None
                        }
                    })
                    .collect::<Vec<(String, String)>>(),
            )
        } else {
            None
        };

        let body_bytes = body
            .collect()
            .await
            .map_err(|error| {
                warn!(cause=?error, "receiving body from backend");
                HttpError::RequestError
            })?
            .to_bytes();
        let body = Some(body_bytes.to_vec());

        trace!(?status, ?headers, len = body_bytes.len(), "reply");

        Ok(Response {
            status,
            headers,
            body,
        })
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

// extract canonical host name
fn canonical_host_name(headers: &Headers, original_uri: &Uri) -> Result<String> {
    let host = headers.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("host") {
            Some(v.to_owned())
        } else {
            None
        }
    });
    host.or_else(|| original_uri.host().map(|h| h.to_string()))
        .ok_or(anyhow!("Could determine a Host header"))
}

// make canonical uri for backend
fn canonical_url(original_url: &Uri, canonical_host: &str, backend_path: &str) -> Result<Uri> {
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

impl FastEdgeConnector {
    pub fn new(backend: Uri) -> Self {
        let mut inner = HttpConnector::new();
        inner.enforce_http(false);

        Self { inner, backend }
    }
}

impl Service<Uri> for FastEdgeConnector {
    type Response = Connection;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, _req: Uri) -> Self::Future {
        trace!("connecting to: {}", self.backend);
        let connect_fut = self.inner.call(self.backend.clone());
        Box::pin(async move {
            let conn = connect_fut
                .await
                .map(|inner| Connection {
                    inner: inner.into_inner(),
                })
                .map_err(Box::new)?;
            Ok(conn)
        })
    }
}

impl hyper::rt::Read for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<std::result::Result<(), std::io::Error>> {
        let n = unsafe {
            let mut tbuf = tokio::io::ReadBuf::uninit(buf.as_mut());
            match tokio::io::AsyncRead::poll_read(self.project().inner, cx, &mut tbuf) {
                Poll::Ready(Ok(())) => tbuf.filled().len(),
                other => return other,
            }
        };

        unsafe {
            buf.advance(n);
        }
        Poll::Ready(Ok(()))
    }
}

impl hyper::rt::Write for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::result::Result<usize, std::io::Error>> {
        tokio::io::AsyncWrite::poll_write(self.project().inner, cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), std::io::Error>> {
        tokio::io::AsyncWrite::poll_flush(self.project().inner, cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), std::io::Error>> {
        tokio::io::AsyncWrite::poll_shutdown(self.project().inner, cx)
    }
}

impl hyper_util::client::legacy::connect::Connection for Connection {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        hyper_util::client::legacy::connect::Connected::new()
    }
}

fn extract_host(addr: &str) -> &str {
    if addr.starts_with('[') {
        // IPv6 with port: [::1]:8080
        addr.split(']')
            .next()
            .unwrap_or(addr)
            .trim_start_matches('[')
    } else {
        // IPv4 with port or just host
        addr.rsplit_once(':').map(|(host, _)| host).unwrap_or(addr)
    }
}

pub fn is_public_host(host: &str) -> bool {
    let host = extract_host(host);
    // Try to parse as IP address
    match host.parse::<IpAddr>() {
        Ok(ip) => !is_private_ip(&ip),
        Err(_) => true, // Not an IP address, assume it's a hostname
    }
}

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_private_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_private_ipv6(ipv6),
    }
}

/// Check if an IPv4 address is private
fn is_private_ipv4(ip: &Ipv4Addr) -> bool {
    ip.octets()[0] == 0 // "This network"
        || ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || (
        ip.octets()[0] == 192 && ip.octets()[1] == 0 && ip.octets()[2] == 0
            && ip.octets()[3] != 9 && ip.octets()[3] != 10
    )
        || ip.is_documentation()
        || ip.is_broadcast()
}

/// Check if an IPv6 address is private
fn is_private_ipv6(ip: &Ipv6Addr) -> bool {
    ip.is_unspecified()
        || ip.is_loopback()
        || matches!(ip.segments(), [0, 0, 0, 0, 0, 0xffff, _, _])
        || matches!(ip.segments(), [0x64, 0xff9b, 1, _, _, _, _, _])
        || matches!(ip.segments(), [0x100, 0, 0, 0, _, _, _, _])
        || (matches!(ip.segments(), [0x2001, b, _, _, _, _, _, _] if b < 0x200)
            && !(u128::from_be_bytes(ip.octets()) == 0x2001_0001_0000_0000_0000_0000_0000_0001
                || u128::from_be_bytes(ip.octets()) == 0x2001_0001_0000_0000_0000_0000_0000_0002
                || matches!(ip.segments(), [0x2001, 3, _, _, _, _, _, _])
                || matches!(ip.segments(), [0x2001, 4, 0x112, _, _, _, _, _])
                || matches!(ip.segments(), [0x2001, b, _, _, _, _, _, _] if (0x20..=0x3F).contains(&b))))
        || matches!(ip.segments(), [0x2002, _, _, _, _, _, _, _])
        || matches!(ip.segments(), [0x5f00, ..])
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
}

#[cfg(test)]
mod tests {
    use super::*;
    use claims::assert_ok;
    use http::StatusCode;

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn simple_http_request() {
        let mut builder = mock_http_connector::Connector::builder();
        builder
            .expect()
            .times(1)
            .with_method(http::Method::GET)
            .with_uri("http://be.server/path")
            .with_header("fastedge-hostname", "example.com")
            .with_header("fastedge-scheme", "http")
            .with_header("host", "be.server")
            .with_header("header01", "01")
            .returning("OK")
            .unwrap();
        let connector = builder.build();
        let mut backend =
            Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
                .build(connector);
        let mut headers = HeaderMap::new();
        headers.insert("Server_name", claims::assert_ok!("server".try_into()));
        claims::assert_ok!(backend.propagate_headers(headers));
        let req = Request {
            method: Method::Get,
            uri: "http://example.com/path".to_string(),
            headers: vec![("header01".to_string(), "01".to_string())],
            body: None,
        };
        let res = claims::assert_ok!(backend.send_request(req).await);
        assert_eq!(http::StatusCode::OK, res.status);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn error_http_request() {
        let mut builder = mock_http_connector::Connector::builder();
        builder
            .expect()
            .times(1)
            .with_method(http::Method::GET)
            .with_uri("http://be.server/path")
            .with_header("fastedge-hostname", "example.com")
            .with_header("fastedge-scheme", "http")
            .with_header("host", "be.server")
            .with_header("header01", "01")
            .returning(StatusCode::REQUEST_TIMEOUT)
            .unwrap();
        let connector = builder.build();
        let mut backend =
            Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
                .build(connector);
        let mut headers = HeaderMap::new();
        headers.insert("Server_name", claims::assert_ok!("server".try_into()));
        claims::assert_ok!(backend.propagate_headers(headers));
        let req = Request {
            method: Method::Get,
            uri: "http://example.com/path".to_string(),
            headers: vec![("header01".to_string(), "01".to_string())],
            body: None,
        };
        let res = claims::assert_ok!(backend.send_request(req).await);
        assert_eq!(http::StatusCode::REQUEST_TIMEOUT, res.status);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn simple_http_request_with_host() {
        let mut builder = mock_http_connector::Connector::builder();
        builder
            .expect()
            .times(1)
            .with_method(http::Method::GET)
            .with_uri("http://be.server/path")
            .with_header("fastedge-hostname", "example.com")
            .with_header("fastedge-scheme", "http")
            .with_header("host", "be.server")
            .with_header("header01", "01")
            .returning("OK")
            .unwrap();
        let connector = builder.build();
        let mut backend =
            Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
                .build(connector);
        let mut headers = HeaderMap::new();
        headers.insert("Server_name", claims::assert_ok!("server".try_into()));
        claims::assert_ok!(backend.propagate_headers(headers));
        let req = Request {
            method: Method::Get,
            uri: "/path".to_string(),
            headers: vec![
                ("header01".to_string(), "01".to_string()),
                ("host".to_string(), "example.com".to_string()),
            ],
            body: None,
        };
        let res = claims::assert_ok!(backend.send_request(req).await);
        assert_eq!(http::StatusCode::OK, res.status);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn simple_https_request() {
        let mut builder = mock_http_connector::Connector::builder();
        builder
            .expect()
            .times(1)
            .with_method(http::Method::GET)
            .with_uri("http://rust-lang.org")
            .with_header("fastedge-hostname", "rust-lang.org")
            .with_header("fastedge-scheme", "https")
            .returning("OK")
            .unwrap();
        let connector = builder.build();
        let mut backend =
            Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
                .build(connector);
        let req = Request {
            method: Method::Get,
            uri: "https://rust-lang.org".to_string(),
            headers: vec![],
            body: None,
        };
        let res = claims::assert_ok!(backend.send_request(req).await);
        assert_eq!(http::StatusCode::OK, res.status);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn filter_headers() {
        let mut builder = mock_http_connector::Connector::builder();
        builder
            .expect()
            .times(1)
            .with_method(http::Method::GET)
            .with_uri("http://be.server")
            .with_header("fastedge-hostname", "rust-lang.org")
            .with_header("fastedge-scheme", "http")
            .with_header("header01", "01")
            .returning("OK")
            .unwrap();
        let connector = builder.build();
        let mut backend =
            Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
                .build(connector);
        let mut headers = HeaderMap::new();
        headers.insert("Server_name", claims::assert_ok!("server".try_into()));
        claims::assert_ok!(backend.propagate_headers(headers));
        let req = Request {
            method: Method::Get,
            uri: "http://rust-lang.org".to_string(),
            headers: vec![
                ("host".to_string(), "example.com".to_string()),
                ("header01".to_string(), "01".to_string()),
                ("content-lenght".to_string(), "unexpected".to_string()),
                ("Transfer-Encoding".to_string(), "unexpected".to_string()),
                ("fastedge-hostname".to_string(), "unexpected".to_string()),
                ("fastedge-scheme".to_string(), "unexpected".to_string()),
            ],
            body: None,
        };
        let res = claims::assert_ok!(backend.send_request(req).await);
        assert_eq!(http::StatusCode::OK, res.status);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn propagate_headers() {
        let mut builder = mock_http_connector::Connector::builder();
        builder
            .expect()
            .times(1)
            .with_method(http::Method::GET)
            .with_uri("http://be.server")
            .with_header("fastedge-hostname", "example.com")
            .with_header("fastedge-scheme", "http")
            .with_header("header01", "01")
            .with_header("host", "be.server")
            .with_header("propagate-header", "VALUE")
            .returning("OK")
            .unwrap();
        let connector = builder.build();
        let mut backend =
            Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
                .propagate_headers_names(vec!["Propagate-Header".parse().unwrap()])
                .build(connector);
        let mut headers = HeaderMap::new();
        headers.insert("Server_name", claims::assert_ok!("server".try_into()));
        headers.insert(
            "No-Propagate-Header",
            claims::assert_ok!("VALUE".try_into()),
        );
        headers.insert("Propagate-Header", claims::assert_ok!("VALUE".try_into()));
        claims::assert_ok!(backend.propagate_headers(headers));
        let req = Request {
            method: Method::Get,
            uri: "http://example.com".to_string(),
            headers: vec![("header01".to_string(), "01".to_string())],
            body: None,
        };
        let res = claims::assert_ok!(backend.send_request(req).await);
        assert_eq!(http::StatusCode::OK, res.status);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn backend_path() {
        let mut builder = mock_http_connector::Connector::builder();
        builder
            .expect()
            .times(1)
            .with_method(http::Method::GET)
            .with_uri("http://be.server/backend_path/path/")
            .with_header("fastedge-hostname", "example.com")
            .with_header("fastedge-scheme", "http")
            .with_header("header01", "01")
            .with_header("host", "be.server")
            .returning("OK")
            .unwrap();
        let connector = builder.build();
        let mut backend =
            Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
                .propagate_headers_names(vec!["Propagate-Header".parse().unwrap()])
                .uri(assert_ok!("http://be.server/backend_path/".parse()))
                .build(connector);
        let mut headers = HeaderMap::new();
        headers.insert("Server_name", claims::assert_ok!("server".try_into()));

        claims::assert_ok!(backend.propagate_headers(headers));
        let req = Request {
            method: Method::Get,
            uri: "http://example.com/path/".to_string(),
            headers: vec![("header01".to_string(), "01".to_string())],
            body: None,
        };
        let res = claims::assert_ok!(backend.send_request(req).await);
        assert_eq!(http::StatusCode::OK, res.status);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn max_sub_requests() {
        let mut builder = mock_http_connector::Connector::builder();
        builder
            .expect()
            .times(2)
            .with_method(http::Method::GET)
            .with_uri("http://be.server/")
            .with_header("fastedge-hostname", "example.com")
            .with_header("fastedge-scheme", "http")
            .with_header("header01", "01")
            .with_header("host", "be.server")
            .returning("OK")
            .unwrap();
        let connector = builder.build();
        let mut backend =
            Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
                .propagate_headers_names(vec!["Propagate-Header".parse().unwrap()])
                .max_sub_requests(2)
                .build(connector);
        let mut headers = HeaderMap::new();
        headers.insert("Server_name", claims::assert_ok!("server".try_into()));

        claims::assert_ok!(backend.propagate_headers(headers));
        let req = Request {
            method: Method::Get,
            uri: "http://example.com/".to_string(),
            headers: vec![("header01".to_string(), "01".to_string())],
            body: None,
        };
        let res = claims::assert_ok!(backend.send_request(req.clone()).await);
        assert_eq!(http::StatusCode::OK, res.status);

        let res = claims::assert_ok!(backend.send_request(req.clone()).await);
        assert_eq!(http::StatusCode::OK, res.status);

        let error = claims::assert_err!(backend.send_request(req).await);
        assert_eq!("too-many-requests", error.name());
    }

    #[test]
    fn test_is_public_host_private_ipv4() {
        // Loopback
        assert!(!is_public_host("127.0.0.1"));
        assert!(!is_public_host("127.0.0.1:8080"));

        // Private networks
        assert!(!is_public_host("10.0.0.1"));
        assert!(!is_public_host("10.255.255.255:3000"));
        assert!(!is_public_host("172.16.0.1"));
        assert!(!is_public_host("172.31.255.254:8080"));
        assert!(!is_public_host("192.168.1.1"));
        assert!(!is_public_host("192.168.0.1:9000"));

        // Link-local
        assert!(!is_public_host("169.254.0.1"));
        assert!(!is_public_host("169.254.169.254:80"));

        // Broadcast
        assert!(!is_public_host("255.255.255.255"));

        // This network (0.0.0.0/8)
        assert!(!is_public_host("0.0.0.0"));
        assert!(!is_public_host("0.1.2.3:8080"));

        // Documentation addresses
        assert!(!is_public_host("192.0.2.1"));
        assert!(!is_public_host("198.51.100.1"));
        assert!(!is_public_host("203.0.113.1"));
    }

    #[test]
    fn test_is_public_host_public_ipv4() {
        // Public IP addresses
        assert!(is_public_host("8.8.8.8"));
        assert!(is_public_host("8.8.8.8:53"));
        assert!(is_public_host("1.1.1.1"));
        assert!(is_public_host("1.1.1.1:443"));
        assert!(is_public_host("93.184.216.34"));
        assert!(is_public_host("93.184.216.34:80"));
    }

    #[test]
    fn test_is_public_host_private_ipv6() {
        // Loopback
        assert!(!is_public_host("[::1]"));
        assert!(!is_public_host("[::1]:8080"));

        // Unspecified
        assert!(!is_public_host("[::]"));
        assert!(!is_public_host("[::]:8080"));

        // Link-local
        assert!(!is_public_host("[fe80::1]"));
        assert!(!is_public_host("[fe80::1]:8080"));

        // Unique local
        assert!(!is_public_host("[fc00::1]"));
        assert!(!is_public_host("[fd00::1]"));
        assert!(!is_public_host("[fd00::1]:9000"));

        // IPv4-mapped IPv6
        assert!(!is_public_host("[::ffff:127.0.0.1]"));
        assert!(!is_public_host("[::ffff:192.168.1.1]"));
    }

    #[test]
    fn test_is_public_host_public_ipv6() {
        // Public IPv6 addresses
        assert!(is_public_host("[2001:4860:4860::8888]"));
        assert!(is_public_host("[2001:4860:4860::8888]:443"));
        assert!(is_public_host("[2606:4700:4700::1111]"));
        assert!(is_public_host("[2606:4700:4700::1111]:80"));
    }

    #[test]
    fn test_is_public_host_domain_names() {
        // Domain names should be considered public
        assert!(is_public_host("example.com"));
        assert!(is_public_host("example.com:8080"));
        assert!(is_public_host("www.example.com"));
        assert!(is_public_host("api.example.com:443"));
        assert!(is_public_host("subdomain.example.co.uk"));
        assert!(is_public_host("localhost")); // hostname, not IP
    }

    #[test]
    fn test_extract_host() {
        // IPv4 with port
        assert_eq!(extract_host("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(extract_host("192.168.1.1:3000"), "192.168.1.1");

        // IPv4 without port
        assert_eq!(extract_host("127.0.0.1"), "127.0.0.1");

        // IPv6 with port
        assert_eq!(extract_host("[::1]:8080"), "::1");
        assert_eq!(extract_host("[2001:4860:4860::8888]:443"), "2001:4860:4860::8888");

        // IPv6 without port
        assert_eq!(extract_host("[::1]"), "::1");

        // Domain names
        assert_eq!(extract_host("example.com:8080"), "example.com");
        assert_eq!(extract_host("example.com"), "example.com");
        assert_eq!(extract_host("sub.example.com:443"), "sub.example.com");
    }

    #[test]
    fn test_make_request_rejects_private_ipv4() {
        let connector = mock_http_connector::Connector::builder().build();
        let backend = Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
            .build(connector);

        // Test loopback
        let req = Request {
            method: Method::Get,
            uri: "http://127.0.0.1/path".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private host not allowed"));

        // Test private network
        let req = Request {
            method: Method::Get,
            uri: "http://192.168.1.1:8080/path".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private host not allowed"));

        // Test another private network
        let req = Request {
            method: Method::Get,
            uri: "http://10.0.0.1/path".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private host not allowed"));

        // Test link-local
        let req = Request {
            method: Method::Get,
            uri: "http://169.254.169.254/metadata".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private host not allowed"));
    }

    #[test]
    fn test_make_request_rejects_private_ipv4_from_host_header() {
        let connector = mock_http_connector::Connector::builder().build();
        let backend = Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
            .build(connector);

        // Test with Host header containing private IP
        let req = Request {
            method: Method::Get,
            uri: "/path".to_string(),
            headers: vec![("host".to_string(), "192.168.1.1".to_string())],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private host not allowed"));

        // Test with Host header containing private IP with port
        let req = Request {
            method: Method::Get,
            uri: "/path".to_string(),
            headers: vec![("host".to_string(), "127.0.0.1:8080".to_string())],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private host not allowed"));
    }

    #[test]
    fn test_make_request_rejects_private_ipv6() {
        let connector = mock_http_connector::Connector::builder().build();
        let backend = Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
            .build(connector);

        // Test loopback
        let req = Request {
            method: Method::Get,
            uri: "http://[::1]/path".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private host not allowed"));

        // Test unique local
        let req = Request {
            method: Method::Get,
            uri: "http://[fc00::1]:8080/path".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private host not allowed"));

        // Test link-local
        let req = Request {
            method: Method::Get,
            uri: "http://[fe80::1]/path".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private host not allowed"));
    }

    #[test]
    fn test_make_request_accepts_public_ip() {
        let connector = mock_http_connector::Connector::builder().build();
        let backend = Backend::<mock_http_connector::Connector>::builder(BackendStrategy::FastEdge)
            .build(connector);

        // Test public IPv4
        let req = Request {
            method: Method::Get,
            uri: "http://8.8.8.8/path".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_ok());

        // Test public IPv6
        let req = Request {
            method: Method::Get,
            uri: "http://[2001:4860:4860::8888]/path".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_ok());

        // Test domain name
        let req = Request {
            method: Method::Get,
            uri: "http://example.com/path".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_ok());
    }

    #[test]
    fn test_make_request_direct_strategy_allows_private_ip() {
        let connector = mock_http_connector::Connector::builder().build();
        let backend = Backend::<mock_http_connector::Connector>::builder(BackendStrategy::Direct)
            .build(connector);

        // Direct strategy should allow private IPs
        let req = Request {
            method: Method::Get,
            uri: "http://127.0.0.1/path".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_ok());

        let req = Request {
            method: Method::Get,
            uri: "http://192.168.1.1/path".to_string(),
            headers: vec![],
            body: None,
        };
        let result = backend.make_request(req);
        assert!(result.is_ok());
    }


}
