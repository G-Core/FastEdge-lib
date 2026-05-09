use std::fmt::Display;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use wasmtime::component::HasSelf;
use wasmtime_wasi_nn::wit::WasiNnView;

pub use crate::executor::ExecutorFactory;
use crate::executor::HttpExecutor;
use anyhow::{Context, Error, Result, bail};
use bytes::Bytes;
use http::{
    HeaderMap, HeaderName, HeaderValue, StatusCode,
    header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL},
};
use http_backend::SERVER_NAME_HEADER;
use http_body_util::{BodyExt, Empty, Full};
use hyper::{body::Body, server::conn::http1, service::service_fn};
use hyper_util::{client::legacy::connect::Connect, rt::TokioIo};
#[cfg(feature = "metrics")]
use runtime::util::metrics;
use runtime::util::stats::StatsVisitor;
use runtime::{
    App, AppResult, ContextT, Router, WasmEngine, WasmEngineBuilder, app::Status, service::Service,
};
use smol_str::{SmolStr, ToSmolStr};
use state::HttpState;
use tokio::{net::TcpListener, time::error::Elapsed};
use tracing::Instrument;
pub use wasmtime_wasi_http::body::HyperOutgoingBody;

pub mod executor;
pub mod state;

pub(crate) static TRACEPARENT: &str = "traceparent";
pub(crate) static X_CDN_INTERNAL_STATUS: &str = "x-cdn-internal-status";

#[cfg(target_family = "unix")]
type OwnedFd = std::os::fd::OwnedFd;
#[cfg(not(target_family = "unix"))]
type OwnedFd = std::os::raw::c_int;

#[cfg(feature = "metrics")]
const HTTP_LABEL: &[&str; 1] = &["http"];

const FASTEDGE_INTERNAL_ERROR: u16 = 530;
const FASTEDGE_OUT_OF_MEMORY: u16 = 531;
const FASTEDGE_EXECUTION_TIMEOUT: u16 = 532;
const FASTEDGE_EXECUTION_PANIC: u16 = 533;

/// Internal status codes returned in `X-CDN-Internal-Status` response header (range 3000–3999).
pub(crate) const INTERNAL_STATUS_CONTEXT_ERROR: u16 = 3000;
pub(crate) const INTERNAL_STATUS_EXECUTE_ERROR: u16 = 3001;
pub(crate) const INTERNAL_STATUS_APP_EXIT_ERROR: u16 = 3002;
pub(crate) const INTERNAL_STATUS_WASM_TRAP_OTHER: u16 = 3003;
pub(crate) const INTERNAL_STATUS_TIMEOUT_INTERRUPT: u16 = 3010;
pub(crate) const INTERNAL_STATUS_TIMEOUT_ELAPSED: u16 = 3011;
pub(crate) const INTERNAL_STATUS_TIMEOUT_DEADLINE: u16 = 3012;
pub(crate) const INTERNAL_STATUS_OUT_OF_MEMORY: u16 = 3020;

#[derive(Default)]
pub struct HttpConfig {
    pub all_interfaces: bool,
    pub port: u16,
    #[cfg(target_family = "unix")]
    pub cancel: std::sync::Weak<shellflip::ShutdownHandle>,
    pub listen_fd: Option<OwnedFd>,
    pub backoff: u64,
}

pub struct HttpService<T: ContextT, S: StatsVisitor> {
    engine: WasmEngine<HttpState<T::BackendConnector>>,
    context: T,
    _stats: PhantomData<S>,
}

pub trait ContextHeaders {
    fn append_headers(&self) -> impl Iterator<Item = (SmolStr, SmolStr)>;
}

impl<T, S> Service for HttpService<T, S>
where
    T: ContextT
        + Router
        + ContextHeaders
        + ExecutorFactory<HttpState<T::BackendConnector>>
        + Clone
        + Sync
        + Send
        + 'static,
    T::BackendConnector: Connect + Clone + Send + Sync + 'static,
    T::Executor: HttpExecutor + Send + Sync,
    S: StatsVisitor + Send + Sync + 'static,
{
    type State = HttpState<T::BackendConnector>;
    type Config = HttpConfig;
    type Context = T;

    fn new(engine: WasmEngine<Self::State>, context: Self::Context) -> Result<Self> {
        Ok(Self {
            engine,
            context,
            _stats: Default::default(),
        })
    }

    /// Run hyper http service
    async fn run(self, config: Self::Config) -> Result<()> {
        #[allow(unused_variables)]
        let listener = if let Some(fd) = config.listen_fd {
            #[cfg(target_family = "unix")]
            {
                let listener = std::net::TcpListener::from(fd);
                listener.set_nonblocking(true)?;
                TcpListener::from_std(listener)?
            }

            #[cfg(not(target_family = "unix"))]
            panic!("listen_fd is not supported on this platform")
        } else {
            let interface: [u8; 4] = if config.all_interfaces {
                [0, 0, 0, 0]
            } else {
                [127, 0, 0, 1]
            };
            let listen_addr = SocketAddr::from((interface, config.port));
            TcpListener::bind(listen_addr).await?
        };

        let listen_addr = listener.local_addr()?;
        tracing::info!("Listening on http://{}", listen_addr);
        let mut backoff = 1;
        let self_ = Arc::new(self);
        let graceful = hyper_util::server::graceful::GracefulShutdown::new();
        #[cfg(target_family = "unix")]
        let mut signal = config
            .cancel
            .upgrade()
            .map(|s| shellflip::ShutdownSignal::from(s.as_ref()))
            .unwrap_or_default();

        #[cfg(not(target_family = "unix"))]
        let signal = signal::Signal {};

        loop {
            tokio::select! {
                conn = listener.accept() => {
                    match conn {
                        Ok((stream, _)) => {
                            tracing::debug!(remote=?stream.peer_addr(), "new http connection");
                            let connection = self_.clone();
                            use tracing::Instrument;
                            let io = TokioIo::new(stream);

                            let service = service_fn(move |req| {
                                let self_ = connection.clone();
                                let request_id = remote_traceparent(&req);
                                async move {
                                    self_
                                        .handle_request(request_id.clone(), req)
                                        .instrument(tracing::debug_span!("http", ?request_id))
                                        .await
                                }
                            });

                            let connection = http1::Builder::new().keep_alive(true).serve_connection(io, service);
                            let connection = graceful.watch(connection);
                            tokio::spawn(async move {
                                if let Err(error) = connection.await {
                                    tracing::warn!(cause=?error, "Error serving connection");
                                }
                            });
                        }
                        Err(error) => {
                            tracing::warn!(cause=?error, "http accept error");
                            tokio::time::sleep(Duration::from_millis(backoff * 100)).await;
                            if backoff > config.backoff {
                                backoff = 1;
                            } else {
                                backoff *= 2;
                            }
                        }
                    }
                },
                _ = signal.on_shutdown() => {
                    tracing::info!("Shutting down http service");
                    break;
                }
            }
        }

        graceful.shutdown().await;
        Ok(())
    }

    fn configure_engine(builder: &mut WasmEngineBuilder<Self::State>) -> Result<()> {
        let linker = builder.component_linker_ref();
        Self::add_wasi_imports(linker)?;
        Self::add_fastedge_imports(linker)?;
        Ok(())
    }
}

impl<T, S> HttpService<T, S>
where
    T: ContextT
        + Router
        + ContextHeaders
        + ExecutorFactory<HttpState<T::BackendConnector>>
        + Clone
        + Sync
        + Send
        + 'static,
    T::BackendConnector: Connect + Clone + Send + Sync + 'static,
    T::Executor: HttpExecutor + Send + Sync,
    S: StatsVisitor + Send + Sync + 'static,
{
    /// Wires up the standard WASI / WASI-HTTP / WASI-NN host imports.
    ///
    /// These come from upstream wasmtime crates and should normally not be
    /// changed without bumping wasmtime versions in lockstep.
    fn add_wasi_imports(
        linker: &mut wasmtime::component::Linker<runtime::Data<HttpState<T::BackendConnector>>>,
    ) -> Result<()> {
        // Allow re-importing of `wasi:clocks/wall-clock@0.2.0`
        wasmtime_wasi::p2::add_to_linker_async(linker)?;
        linker.allow_shadowing(true);
        wasmtime_wasi_http::add_to_linker_async(linker)?;
        wasmtime_wasi_nn::wit::add_to_linker(linker, |data: &mut runtime::Data<_>| {
            WasiNnView::new(&mut data.table, &mut data.wasi_nn)
        })?;
        Ok(())
    }

    /// Wires up FastEdge-specific host imports defined in the `reactor` WIT world.
    fn add_fastedge_imports(
        linker: &mut wasmtime::component::Linker<runtime::Data<HttpState<T::BackendConnector>>>,
    ) -> Result<()> {
        use reactor::gcore::fastedge as fe;

        fe::http_client::add_to_linker::<_, HasSelf<_>>(linker, |data| {
            &mut data.as_mut().http_backend
        })?;
        fe::dictionary::add_to_linker::<_, HasSelf<_>>(linker, |data| &mut data.dictionary)?;
        fe::secret::add_to_linker::<_, HasSelf<_>>(linker, |data| &mut data.secret_store)?;
        fe::key_value::add_to_linker::<_, HasSelf<_>>(linker, |data| &mut data.key_value_store)?;
        fe::utils::add_to_linker::<_, HasSelf<_>>(linker, |data| &mut data.utils)?;
        fe::cache_sync::add_to_linker::<_, HasSelf<_>>(linker, |data| &mut data.cache)?;

        Ok(())
    }
}

impl<T, S> HttpService<T, S>
where
    T: ContextT
        + Router
        + ContextHeaders
        + ExecutorFactory<HttpState<T::BackendConnector>>
        + Sync
        + Send
        + 'static
        + Clone,
    T::BackendConnector: Clone + Send + Sync + 'static,
    T::Executor: HttpExecutor + Send + Sync,
    S: StatsVisitor + Send + 'static,
{
    /// handle HTTP request.
    async fn handle_request<B>(
        &self,
        request_id: SmolStr,
        mut request: hyper::Request<B>,
    ) -> Result<hyper::Response<HyperOutgoingBody>>
    where
        B: BodyExt + Send,
        <B as Body>::Data: Send,
    {
        request
            .headers_mut()
            .extend(app_req_headers(self.context.append_headers()));

        // get application name from request URL
        let app_name = match app_name_from_request(&request) {
            Err(error) => {
                #[cfg(feature = "metrics")]
                metrics::metrics(AppResult::UNKNOWN, HTTP_LABEL, None, None);
                tracing::info!(cause=?error, "App name not provided");
                return not_found();
            }
            Ok(app_name) => app_name,
        };

        let span = tracing::info_span!("handle", app = %app_name);
        let _enter = span.enter();

        // lookup for application config and binary_id
        tracing::debug!("Processing request URL: {}", request.uri());
        let lookup = match app_name {
            AppName::Id(id) => self.context.lookup_by_id(id).instrument(span.clone()).await,
            AppName::Name(name) => self
                .context
                .lookup_by_name(&name)
                .instrument(span.clone())
                .await
                .map(|cfg| (name, cfg)),
        };

        let (app_name, cfg) = match lookup {
            None => {
                #[cfg(feature = "metrics")]
                metrics::metrics(AppResult::UNKNOWN, HTTP_LABEL, None, None);
                tracing::info!("Request for unknown application on URL: {}", request.uri());
                return not_found();
            }
            Some((app_name, cfg))
                if cfg.status == Status::Draft || cfg.status == Status::Disabled =>
            {
                tracing::info!(
                    "Request for disabled application '{}' on URL: {}",
                    app_name,
                    request.uri()
                );
                return not_found();
            }
            Some((app_name, cfg)) if cfg.status == Status::RateLimited => {
                tracing::info!(
                    "Request for rate limited application '{}' on URL: {}",
                    app_name,
                    request.uri()
                );
                return too_many_requests();
            }
            Some((app_name, cfg)) if cfg.status == Status::Suspended => {
                tracing::info!(
                    "Request for suspended application '{}' on URL: {}",
                    app_name,
                    request.uri()
                );
                return not_acceptable();
            }

            Some((app_name, cfg)) => (app_name, cfg),
        };

        // get cached execute context for this application
        let executor = match self
            .context
            .get_executor(app_name.clone(), &cfg, &self.engine)
        {
            Ok(executor) => executor,
            Err(error) => {
                #[cfg(feature = "metrics")]
                metrics::metrics(AppResult::UNKNOWN, HTTP_LABEL, None, None);
                tracing::warn!(cause=?error, app=%app_name,
                    "failure on getting context"
                );
                return internal_fastedge_error("context error", INTERNAL_STATUS_CONTEXT_ERROR);
            }
        };

        let stats = self.context.new_stats_row(&request_id, &app_name, &cfg);

        let response = match executor
            .execute(request, stats.clone())
            .instrument(span.clone())
            .await
        {
            Ok(mut response) => {
                #[cfg(feature = "metrics")]
                metrics::metrics(
                    AppResult::SUCCESS,
                    &["http"],
                    Some(stats.get_time_elapsed()),
                    Some(stats.get_memory_used()),
                );

                apply_default_and_app_headers(response.headers_mut(), &cfg);
                response
            }
            Err(error) => {
                tracing::warn!(cause=?error, "execute");
                let (status_code, fail_reason, msg, internal_code) = map_err(error);
                stats.status_code(status_code);
                stats.fail_reason(fail_reason as i32);
                tracing::debug!(?fail_reason, ?request_id, "stats");

                #[cfg(feature = "metrics")]
                metrics::metrics(
                    fail_reason,
                    HTTP_LABEL,
                    Some(stats.get_time_elapsed()),
                    None,
                );

                let mut builder = hyper::Response::builder().status(status_code);
                if let Some(headers) = builder.headers_mut() {
                    apply_default_and_app_headers(headers, &cfg);
                    // X_CDN_INTERNAL_STATUS is reserved — forcibly insert *after* the helper
                    // so per-app `rsp_headers` cannot spoof or stale-override the real error code.
                    headers.insert(X_CDN_INTERNAL_STATUS, HeaderValue::from(internal_code));
                }

                builder.body(msg)?
            }
        };
        Ok(response)
    }
}

fn map_err(error: Error) -> (u16, AppResult, HyperOutgoingBody, u16) {
    let root_cause = error.root_cause();
    let (status_code, fail_reason, msg, internal_code) =
        if let Some(exit) = root_cause.downcast_ref::<wasi_common::I32Exit>() {
            if exit.0 == 0 {
                (
                    StatusCode::OK.as_u16(),
                    AppResult::SUCCESS,
                    Empty::new().map_err(|never| match never {}).boxed(),
                    0,
                )
            } else {
                (
                    FASTEDGE_EXECUTION_PANIC,
                    AppResult::OTHER,
                    Full::new(Bytes::from("fastedge: App failed"))
                        .map_err(|never| match never {})
                        .boxed(),
                    INTERNAL_STATUS_APP_EXIT_ERROR,
                )
            }
        } else if let Some(trap) = root_cause.downcast_ref::<wasmtime::Trap>() {
            match trap {
                wasmtime::Trap::Interrupt => (
                    FASTEDGE_EXECUTION_TIMEOUT,
                    AppResult::TIMEOUT,
                    Full::new(Bytes::from("fastedge: Execution timeout"))
                        .map_err(|never| match never {})
                        .boxed(),
                    INTERNAL_STATUS_TIMEOUT_INTERRUPT,
                ),
                wasmtime::Trap::UnreachableCodeReached => (
                    FASTEDGE_OUT_OF_MEMORY,
                    AppResult::OOM,
                    Full::new(Bytes::from("fastedge: Out of memory"))
                        .map_err(|never| match never {})
                        .boxed(),
                    INTERNAL_STATUS_OUT_OF_MEMORY,
                ),
                _ => (
                    FASTEDGE_EXECUTION_PANIC,
                    AppResult::OTHER,
                    Full::new(Bytes::from("fastedge: App failed"))
                        .map_err(|never| match never {})
                        .boxed(),
                    INTERNAL_STATUS_WASM_TRAP_OTHER,
                ),
            }
        } else if let Some(_elapsed) = root_cause.downcast_ref::<Elapsed>() {
            (
                FASTEDGE_EXECUTION_TIMEOUT,
                AppResult::TIMEOUT,
                Full::new(Bytes::from("fastedge: Execution timeout"))
                    .map_err(|never| match never {})
                    .boxed(),
                INTERNAL_STATUS_TIMEOUT_ELAPSED,
            )
        } else if root_cause.to_string().ends_with("deadline has elapsed") {
            (
                FASTEDGE_EXECUTION_TIMEOUT,
                AppResult::TIMEOUT,
                Full::new(Bytes::from("fastedge: Execution timeout"))
                    .map_err(|never| match never {})
                    .boxed(),
                INTERNAL_STATUS_TIMEOUT_DEADLINE,
            )
        } else {
            (
                FASTEDGE_INTERNAL_ERROR,
                AppResult::OTHER,
                Full::new(Bytes::from("fastedge: Execute error"))
                    .map_err(|never| match never {})
                    .boxed(),
                INTERNAL_STATUS_EXECUTE_ERROR,
            )
        };
    (status_code, fail_reason, msg, internal_code)
}

fn remote_traceparent(req: &hyper::Request<hyper::body::Incoming>) -> SmolStr {
    req.headers()
        .get(TRACEPARENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_smolstr())
        .unwrap_or(nanoid::nanoid!().to_smolstr())
}

/// Creates an HTTP 530 response with an `X-CDN-Internal-Status` header.
fn internal_fastedge_error(
    msg: &'static str,
    internal_code: u16,
) -> Result<hyper::Response<HyperOutgoingBody>> {
    Ok(hyper::Response::builder()
        .status(FASTEDGE_INTERNAL_ERROR)
        .header(X_CDN_INTERNAL_STATUS, internal_code)
        .body(
            Full::new(Bytes::from(format!("fastedge: {}", msg)))
                .map_err(|never| match never {})
                .boxed(),
        )?)
}

/// Creates an HTTP 404 response.
fn not_found() -> Result<hyper::Response<HyperOutgoingBody>> {
    Ok(hyper::Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(
            Full::new(Bytes::from("fastedge: Unknown app"))
                .map_err(|never| match never {})
                .boxed(),
        )?)
}

/// Creates an HTTP 429 response.
fn too_many_requests() -> Result<hyper::Response<HyperOutgoingBody>> {
    Ok(hyper::Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .body(Empty::new().map_err(|never| match never {}).boxed())?)
}

/// Creates an HTTP 406 response.
fn not_acceptable() -> Result<hyper::Response<HyperOutgoingBody>> {
    Ok(hyper::Response::builder()
        .status(StatusCode::NOT_ACCEPTABLE)
        .body(Empty::new().map_err(|never| match never {}).boxed())?)
}

#[derive(Debug, Clone)]
pub(crate) enum AppName {
    Name(SmolStr),
    Id(u64),
}

impl<T> From<T> for AppName
where
    SmolStr: From<T>,
{
    fn from(s: T) -> Self {
        AppName::Name(SmolStr::from(s))
    }
}

impl Display for AppName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppName::Name(name) => write!(f, "{}", name),
            AppName::Id(id) => write!(f, "{}", id),
        }
    }
}

const FASTEDGE_APP_ID_HEADER: &str = "fastedge_app_id";

/// Extracts the application identifier from an incoming HTTP request.
///
/// Resolution order (first match wins):
/// 1. `fastedge_app_id` header — parsed as a `u64` → [`AppName::Id`]
/// 2. `server_name` header — the leftmost label of the hostname is used as the app name
///    (e.g. `app.example.com` → `"app"`), unless it is `"www"`.
/// 3. URL path — the first path segment is used as the app name
///    (e.g. `/my-app/route` → `"my_app"`; hyphens are normalised to underscores).
///
/// Returns an error if none of the above yields a non-empty identifier.
fn app_name_from_request(req: &hyper::Request<impl Body>) -> Result<AppName> {
    if let Some(app_id) = req.headers().get(FASTEDGE_APP_ID_HEADER) {
        let id = app_id.to_str().context("app_id header is not a string")?;
        return Ok(AppName::Id(
            id.parse::<u64>().context("app_id header is not a number")?,
        ));
    }

    match req.headers().get(SERVER_NAME_HEADER) {
        None => {}
        Some(h) => {
            let full_hostname = h.to_str().unwrap();
            match full_hostname.find('.') {
                None => {}
                Some(i) => {
                    let (prefix, _) = full_hostname.split_at(i);
                    if prefix != "www" {
                        return Ok(AppName::from(prefix));
                    }
                }
            }
        }
    }

    let path = req.uri().path().strip_prefix('/').unwrap();
    if path.is_empty() {
        bail!("app name not found in URL".to_string());
    }

    match path.find('/') {
        None => Ok(AppName::from(path)),
        Some(i) => {
            let (prefix, _) = path.split_at(i);
            if prefix.contains('-') {
                Ok(AppName::from(prefix.replace('-', "_")))
            } else {
                Ok(AppName::from(prefix))
            }
        }
    }
}

/// Apply runtime defaults and per-app `rsp_headers` config to a response's headers.
///
/// Precedence (lowest → highest):
///   1. Runtime defaults (`Access-Control-Allow-Origin: *`, `Cache-Control: no-store`)
///      — only filled when neither the wasm component nor app config set them.
///   2. Whatever the wasm component already wrote into `headers`.
///   3. Per-app static `rsp_headers` (operator override) — replaces whatever is there.
fn apply_default_and_app_headers(headers: &mut HeaderMap, app_cfg: &App) {
    headers
        .entry(ACCESS_CONTROL_ALLOW_ORIGIN)
        .or_insert_with(|| HeaderValue::from_static("*"));
    headers
        .entry(CACHE_CONTROL)
        .or_insert_with(|| HeaderValue::from_static("no-store"));

    /* if specified, add/overwrite response headers from app config */
    for (name, val) in &app_cfg.rsp_headers {
        if val.is_empty() {
            continue;
        }
        let Ok(key) = name.parse::<HeaderName>() else {
            tracing::debug!("Unable to parse header name: {}", name);
            continue;
        };
        let Ok(value) = val.parse::<HeaderValue>() else {
            tracing::debug!("Unable to parse header value: {}", val);
            continue;
        };
        headers.insert(key, value);
    }
}

fn app_req_headers(geo: impl Iterator<Item = (SmolStr, SmolStr)>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (key, value) in geo {
        tracing::trace!("append new request header {}={}", key, value);
        match key.parse::<HeaderName>() {
            Ok(name) => match value.parse::<HeaderValue>() {
                Ok(value) => {
                    headers.insert(name, value);
                }
                Err(error) => tracing::warn!(cause=?error, "could not parse http value: {}", value),
            },
            Err(error) => tracing::warn!(cause=?error, "could not parse http header: {}", key),
        }
    }
    headers
}

#[cfg(not(target_family = "unix"))]
pub(crate) mod signal {
    pub(crate) struct Signal;

    impl Signal {
        pub(crate) async fn on_shutdown(&self) {
            tokio::signal::ctrl_c().await.expect("ctrl-c");
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use crate::AppName;
    use crate::app_name_from_request;
    use bytes::Bytes;
    use claims::{assert_err, assert_ok};
    use http_backend::SERVER_NAME_HEADER;
    use http_body_util::{BodyExt, Empty};

    fn empty_body_request() -> http::request::Builder {
        http::Request::builder().method("GET")
    }

    // ── Name variant: server_name header ──────────────────────────────────

    #[test_case("app.server.com",  "/",        "app";      "server_name: normal subdomain")]
    #[test_case("foo.example.org", "/ignored", "foo";      "server_name: path is ignored")]
    fn test_app_name_from_server_name(server_name: &str, uri: &str, expected: &str) {
        let req = assert_ok!(
            empty_body_request()
                .uri(uri)
                .header(SERVER_NAME_HEADER, server_name)
                .body(
                    Empty::<Bytes>::new()
                        .map_err(|never| match never {})
                        .boxed()
                )
        );
        let app_name = assert_ok!(app_name_from_request(&req));
        assert!(matches!(&app_name, AppName::Name(n) if n.as_str() == expected));
    }

    #[test]
    fn test_app_name_server_name_www_falls_through_to_path() {
        // "www" subdomain must be ignored and resolution must fall through to URL path
        let req = assert_ok!(
            empty_body_request()
                .uri("/myapp/route")
                .header(SERVER_NAME_HEADER, "www.example.com")
                .body(
                    Empty::<Bytes>::new()
                        .map_err(|never| match never {})
                        .boxed()
                )
        );
        let app_name = assert_ok!(app_name_from_request(&req));
        assert!(matches!(&app_name, AppName::Name(n) if n.as_str() == "myapp"));
    }

    #[test]
    fn test_app_name_server_name_no_dot_falls_through_to_path() {
        // hostname without a dot must fall through to URL path
        let req = assert_ok!(
            empty_body_request()
                .uri("/myapp")
                .header(SERVER_NAME_HEADER, "localhost")
                .body(
                    Empty::<Bytes>::new()
                        .map_err(|never| match never {})
                        .boxed()
                )
        );
        let app_name = assert_ok!(app_name_from_request(&req));
        assert!(matches!(&app_name, AppName::Name(n) if n.as_str() == "myapp"));
    }

    // ── Name variant: URL path ────────────────────────────────────────────

    #[test_case("/myapp",         "myapp";   "path only, no subpath")]
    #[test_case("/myapp/route",   "myapp";   "path with subpath")]
    #[test_case("/my-app/route",  "my_app";  "hyphens normalised to underscores")]
    fn test_app_name_from_path(uri: &str, expected: &str) {
        let req = assert_ok!(
            empty_body_request().uri(uri).body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            )
        );
        let app_name = assert_ok!(app_name_from_request(&req));
        assert!(matches!(&app_name, AppName::Name(n) if n.as_str() == expected));
    }

    // ── Id variant: fastedge_app_id header ───────────────────────────────

    #[test]
    fn test_app_name_from_app_id_header() {
        let req = assert_ok!(
            empty_body_request()
                .uri("/")
                .header("fastedge_app_id", "42")
                .body(
                    Empty::<Bytes>::new()
                        .map_err(|never| match never {})
                        .boxed()
                )
        );
        let app_name = assert_ok!(app_name_from_request(&req));
        assert!(matches!(app_name, AppName::Id(42)));
    }

    #[test]
    fn test_app_name_app_id_takes_priority_over_server_name() {
        // fastedge_app_id must win over server_name
        let req = assert_ok!(
            empty_body_request()
                .uri("/")
                .header("fastedge_app_id", "99")
                .header(SERVER_NAME_HEADER, "other.example.com")
                .body(
                    Empty::<Bytes>::new()
                        .map_err(|never| match never {})
                        .boxed()
                )
        );
        let app_name = assert_ok!(app_name_from_request(&req));
        assert!(matches!(app_name, AppName::Id(99)));
    }

    #[test]
    fn test_app_name_app_id_not_a_number_returns_error() {
        let req = assert_ok!(
            empty_body_request()
                .uri("/")
                .header("fastedge_app_id", "not-a-number")
                .body(
                    Empty::<Bytes>::new()
                        .map_err(|never| match never {})
                        .boxed()
                )
        );
        assert_err!(app_name_from_request(&req));
    }

    // ── Error cases ───────────────────────────────────────────────────────

    #[test]
    fn test_app_name_empty_path_returns_error() {
        let req = assert_ok!(
            empty_body_request().uri("/").body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            )
        );
        assert_err!(app_name_from_request(&req));
    }

    // ── Display impl ─────────────────────────────────────────────────────

    #[test]
    fn test_app_name_display_name() {
        let name = AppName::Name("myapp".into());
        assert_eq!("myapp", name.to_string());
    }

    #[test]
    fn test_app_name_display_id() {
        let id = AppName::Id(1234);
        assert_eq!("1234", id.to_string());
    }

    // ── apply_default_and_app_headers ────────────────────────────────────
    //
    // Precedence: per-app `rsp_headers` (operator) > wasm response (developer)
    //             > runtime defaults (`*`, `no-store`).

    mod apply_headers {
        use crate::apply_default_and_app_headers;
        use http::{
            HeaderMap, HeaderValue,
            header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL},
        };
        use runtime::{App, app::Status};
        use smol_str::ToSmolStr;
        use std::collections::HashMap;

        fn test_app(rsp_headers: HashMap<smol_str::SmolStr, smol_str::SmolStr>) -> App {
            App {
                binary_id: 0,
                max_duration: 10,
                mem_limit: 1_400_000,
                env: Default::default(),
                rsp_headers,
                log: Default::default(),
                app_id: 12345,
                client_id: 23456,
                plan: "test_plan".to_smolstr(),
                status: Status::Enabled,
                debug_until: None,
                secrets: vec![],
                kv_stores: vec![],
                plan_id: 0,
                cache_mode: Default::default(),
            }
        }

        #[test]
        fn defaults_fill_when_wasm_silent() {
            let mut headers = HeaderMap::new();
            apply_default_and_app_headers(&mut headers, &test_app(HashMap::new()));

            assert_eq!(
                Some("*"),
                headers
                    .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                    .and_then(|v| v.to_str().ok())
            );
            assert_eq!(
                Some("no-store"),
                headers.get(CACHE_CONTROL).and_then(|v| v.to_str().ok())
            );
        }

        #[test]
        fn wasm_set_cache_control_is_preserved() {
            let mut headers = HeaderMap::new();
            headers.insert(
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=60"),
            );

            apply_default_and_app_headers(&mut headers, &test_app(HashMap::new()));

            // Wasm value preserved, not appended-to or replaced.
            assert_eq!(1, headers.get_all(CACHE_CONTROL).iter().count());
            assert_eq!(
                Some("public, max-age=60"),
                headers.get(CACHE_CONTROL).and_then(|v| v.to_str().ok())
            );
            // CORS default still kicks in since wasm didn't set it.
            assert_eq!(
                Some("*"),
                headers
                    .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                    .and_then(|v| v.to_str().ok())
            );
        }

        #[test]
        fn wasm_set_cors_is_preserved() {
            let mut headers = HeaderMap::new();
            headers.insert(
                ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("https://example.com"),
            );

            apply_default_and_app_headers(&mut headers, &test_app(HashMap::new()));

            assert_eq!(
                1,
                headers.get_all(ACCESS_CONTROL_ALLOW_ORIGIN).iter().count()
            );
            assert_eq!(
                Some("https://example.com"),
                headers
                    .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                    .and_then(|v| v.to_str().ok())
            );
            // Cache-control default still applied.
            assert_eq!(
                Some("no-store"),
                headers.get(CACHE_CONTROL).and_then(|v| v.to_str().ok())
            );
        }

        #[test]
        fn app_config_replaces_wasm_value() {
            let mut headers = HeaderMap::new();
            headers.insert(
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=60"),
            );

            let cfg = test_app(HashMap::from([(
                "Cache-Control".to_smolstr(),
                "private, max-age=0".to_smolstr(),
            )]));
            apply_default_and_app_headers(&mut headers, &cfg);

            // Per-app config wins over both wasm and default.
            assert_eq!(1, headers.get_all(CACHE_CONTROL).iter().count());
            assert_eq!(
                Some("private, max-age=0"),
                headers.get(CACHE_CONTROL).and_then(|v| v.to_str().ok())
            );
        }

        #[test]
        fn app_config_adds_custom_header() {
            let mut headers = HeaderMap::new();
            let cfg = test_app(HashMap::from([(
                "X-Custom".to_smolstr(),
                "yes".to_smolstr(),
            )]));
            apply_default_and_app_headers(&mut headers, &cfg);

            assert_eq!(
                Some("yes"),
                headers.get("x-custom").and_then(|v| v.to_str().ok())
            );
            // Defaults still present.
            assert_eq!(
                Some("*"),
                headers
                    .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                    .and_then(|v| v.to_str().ok())
            );
            assert_eq!(
                Some("no-store"),
                headers.get(CACHE_CONTROL).and_then(|v| v.to_str().ok())
            );
        }

        #[test]
        fn reserved_internal_status_header_cannot_be_spoofed_by_app_config() {
            // Mirrors the error-path order in lib.rs: apply defaults+app config first,
            // then forcibly insert X_CDN_INTERNAL_STATUS so per-app rsp_headers can't
            // override a reserved runtime header.
            use crate::X_CDN_INTERNAL_STATUS;

            let mut headers = HeaderMap::new();
            let cfg = test_app(HashMap::from([(
                "x-cdn-internal-status".to_smolstr(),
                "9999".to_smolstr(),
            )]));
            apply_default_and_app_headers(&mut headers, &cfg);
            // Real error code wins (here: 3010 = INTERNAL_STATUS_TIMEOUT_INTERRUPT).
            headers.insert(X_CDN_INTERNAL_STATUS, HeaderValue::from(3010_u16));

            assert_eq!(1, headers.get_all(X_CDN_INTERNAL_STATUS).iter().count());
            assert_eq!(
                Some("3010"),
                headers
                    .get(X_CDN_INTERNAL_STATUS)
                    .and_then(|v| v.to_str().ok())
            );
        }

        #[test]
        fn app_config_empty_value_is_skipped() {
            let mut headers = HeaderMap::new();
            headers.insert(
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=60"),
            );

            let cfg = test_app(HashMap::from([(
                "Cache-Control".to_smolstr(),
                "".to_smolstr(),
            )]));
            apply_default_and_app_headers(&mut headers, &cfg);

            // Empty value in config is a no-op; wasm value survives.
            assert_eq!(
                Some("public, max-age=60"),
                headers.get(CACHE_CONTROL).and_then(|v| v.to_str().ok())
            );
        }
    }
}
