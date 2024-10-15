use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Weak};
use std::time::Duration;

pub use crate::executor::ExecutorFactory;
use crate::executor::HttpExecutor;
use anyhow::{bail, Result};
use bytes::Bytes;
use http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL};
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Body;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::Connect;
use hyper_util::rt::TokioIo;
use runtime::app::Status;
use runtime::service::Service;
#[cfg(feature = "metrics")]
use runtime::util::metrics;
#[cfg(feature = "stats")]
use runtime::util::stats::StatRow;
use runtime::util::stats::StatsWriter;
use runtime::{App, AppResult, ContextT, Router, WasmEngine, WasmEngineBuilder};
use secret::SecretStrategy;
use shellflip::ShutdownHandle;
use smol_str::SmolStr;
use state::HttpState;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::time::error::Elapsed;
use tracing::Instrument;
use wasi_common::I32Exit;
use wasmtime::Trap;

pub mod executor;
pub mod state;

pub(crate) static TRACEPARENT: &str = "traceparent";

#[cfg(feature = "metrics")]
const HTTP_LABEL: &[&str; 1] = &["http"];

const FASTEDGE_INTERNAL_ERROR: u16 = 530;
const FASTEDGE_OUT_OF_MEMORY: u16 = 531;
const FASTEDGE_EXECUTION_TIMEOUT: u16 = 532;
const FASTEDGE_EXECUTION_PANIC: u16 = 533;

#[derive(Default)]
pub struct HttpConfig {
    pub all_interfaces: bool,
    pub port: u16,
    pub cancel: Weak<ShutdownHandle>,
    pub listen_fd: Option<OwnedFd>,
    pub backoff: u64,
}

pub struct HttpService<T: ContextT, S: SecretStrategy> {
    engine: WasmEngine<HttpState<T::BackendConnector, S>>,
    context: T,
}

pub trait ContextHeaders {
    fn append_headers(&self) -> impl Iterator<Item = (SmolStr, SmolStr)>;
}

impl<T, S> Service for HttpService<T, S>
where
    T: ContextT
        + StatsWriter
        + Router
        + ContextHeaders
        + ExecutorFactory<HttpState<T::BackendConnector, S>>
        + Sync
        + Send
        + 'static,
    T::BackendConnector: Connect + Clone + Send + Sync + 'static,
    T::Executor: HttpExecutor + Send + Sync,
    S: SecretStrategy + Send + Sync + 'static,
{
    type State = HttpState<T::BackendConnector, S>;
    type Config = HttpConfig;
    type Context = T;

    fn new(engine: WasmEngine<Self::State>, context: Self::Context) -> Result<Self> {
        Ok(Self { engine, context })
    }

    /// Run hyper http service
    async fn run(self, config: Self::Config) -> Result<()> {
        let listener = if let Some(fd) = config.listen_fd {
            let listener = std::net::TcpListener::from(fd);
            listener.set_nonblocking(true)?;
            TcpListener::from_std(listener)?
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
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tracing::trace!("new http connection");
                    let connection = self_.clone();
                    if let Some(cancel) = config.cancel.upgrade() {
                        tokio::spawn(connection.serve(stream, cancel));
                        backoff = 1;
                    } else {
                        tracing::debug!("weak cancel handler");
                        backoff *= 2;
                    }
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
        }
    }

    fn configure_engine(builder: &mut WasmEngineBuilder<Self::State>) -> Result<()> {
        let linker = builder.component_linker_ref();
        wasmtime_wasi_nn::wit::ML::add_to_linker(linker, |data| &mut data.as_mut().wasi_nn)?;
        // Allow re-importing of `wasi:clocks/wall-clock@0.2.0`
        wasmtime_wasi::add_to_linker_async(linker)?;
        linker.allow_shadowing(true);
        wasmtime_wasi_http::proxy::add_to_linker(linker)?;

        reactor::gcore::fastedge::http_client::add_to_linker(linker, |data| {
            &mut data.as_mut().http_backend
        })?;

        reactor::gcore::fastedge::dictionary::add_to_linker(linker, |data| {
            &mut data.as_mut().dictionary
        })?;

        reactor::gcore::fastedge::secret::add_to_linker(linker, |data| &mut data.as_mut().secret)?;

        Ok(())
    }
}

impl<T, U> HttpService<T, U>
where
    T: ContextT
        + StatsWriter
        + Router
        + ContextHeaders
        + ExecutorFactory<HttpState<T::BackendConnector, U>>
        + Sync
        + Send
        + 'static,
    T::BackendConnector: Clone + Send + Sync + 'static,
    T::Executor: HttpExecutor + Send + Sync,
    U: SecretStrategy,
{
    async fn serve<S>(self: Arc<Self>, stream: S, _cancel: Arc<ShutdownHandle>)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let io = TokioIo::new(stream);

        let service = service_fn(move |req| {
            let self_ = self.clone();
            let request_id = remote_traceparent(&req);
            async move {
                self_
                    .handle_request(&request_id, req)
                    .instrument(tracing::info_span!("http_handler", request_id))
                    .await
            }
        });
        if let Err(error) = http1::Builder::new().serve_connection(io, service).await {
            tracing::warn!(cause=?error, "Error serving connection");
        }
    }

    /// handle HTTP request.
    async fn handle_request<B>(
        &self,
        request_id: &str,
        mut request: Request<B>,
    ) -> Result<Response<BoxBody<Bytes, anyhow::Error>>>
    where
        B: BodyExt + Send,
        <B as Body>::Data: Send,
    {
        #[cfg(feature = "stats")]
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("current time")
            .as_secs();

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
        // lookup for application config and binary_id
        tracing::info!(
            "Processing request for application '{}' on URL: {}",
            app_name,
            request.uri()
        );
        let cfg = match self.context.lookup_by_name(&app_name).await {
            None => {
                #[cfg(feature = "metrics")]
                metrics::metrics(AppResult::UNKNOWN, HTTP_LABEL, None, None);
                tracing::info!(
                    "Request for unknown application '{}' on URL: {}",
                    app_name,
                    request.uri()
                );
                return not_found();
            }
            Some(cfg) if cfg.status == Status::Draft || cfg.status == Status::Disabled => {
                tracing::info!(
                    "Request for disabled application '{}' on URL: {}",
                    app_name,
                    request.uri()
                );
                return not_found();
            }
            Some(cfg) if cfg.status == Status::RateLimited => {
                tracing::info!(
                    "Request for rate limited application '{}' on URL: {}",
                    app_name,
                    request.uri()
                );
                return too_many_requests();
            }
            Some(app_cfg) if app_cfg.status == Status::Suspended => {
                tracing::info!(
                    "Request for suspended application '{}' on URL: {}",
                    app_name,
                    request.uri()
                );
                return not_acceptable();
            }

            Some(cfg) => cfg,
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
                tracing::warn!(cause=?error,
                    "failure on getting context"
                );
                return internal_fastedge_error("context error");
            }
        };

        let start_ = std::time::Instant::now();

        let response = match executor.execute(request).await {
            Ok((mut response, time_elapsed, memory_used)) => {
                tracing::info!(
                    "'{}' completed with status code: '{}' in {:.0?} using {} of WebAssembly heap",
                    app_name,
                    response.status(),
                    time_elapsed,
                    memory_used
                );

                #[cfg(feature = "stats")]
                {
                    let stat_row = StatRow {
                        app_id: cfg.app_id,
                        client_id: cfg.client_id,
                        timestamp: timestamp as u32,
                        app_name: app_name.to_string(),
                        status_code: response.status().as_u16() as u32,
                        fail_reason: 0, // TODO: use AppResult
                        billing_plan: cfg.plan.to_string(),
                        time_elapsed: time_elapsed.as_micros() as u64,
                        memory_used: memory_used.as_u64(),
                        request_id: request_id.to_string(),
                        ..Default::default()
                    };
                    self.context.write_stats(stat_row);
                }
                #[cfg(feature = "metrics")]
                metrics::metrics(
                    AppResult::SUCCESS,
                    &["http"],
                    Some(time_elapsed.as_micros() as u64),
                    Some(memory_used.as_u64()),
                );

                response.headers_mut().extend(app_res_headers(cfg));
                response
            }
            Err(error) => {
                tracing::warn!(cause=?error, "execute");
                let root_cause = error.root_cause();
                let time_elapsed = std::time::Instant::now().duration_since(start_);
                let (status_code, fail_reason, msg) =
                    if let Some(exit) = root_cause.downcast_ref::<I32Exit>() {
                        if exit.0 == 0 {
                            (
                                StatusCode::OK.as_u16(),
                                AppResult::SUCCESS,
                                Empty::new().map_err(|never| match never {}).boxed(),
                            )
                        } else {
                            (
                                FASTEDGE_EXECUTION_PANIC,
                                AppResult::OTHER,
                                Full::new(Bytes::from("fastedge: App failed"))
                                    .map_err(|never| match never {})
                                    .boxed(),
                            )
                        }
                    } else if let Some(trap) = root_cause.downcast_ref::<Trap>() {
                        match trap {
                            Trap::Interrupt => (
                                FASTEDGE_EXECUTION_TIMEOUT,
                                AppResult::TIMEOUT,
                                Full::new(Bytes::from("fastedge: Execution timeout"))
                                    .map_err(|never| match never {})
                                    .boxed(),
                            ),
                            Trap::UnreachableCodeReached => (
                                FASTEDGE_OUT_OF_MEMORY,
                                AppResult::OOM,
                                Full::new(Bytes::from("fastedge: Out of memory"))
                                    .map_err(|never| match never {})
                                    .boxed(),
                            ),
                            _ => (
                                FASTEDGE_EXECUTION_PANIC,
                                AppResult::OTHER,
                                Full::new(Bytes::from("fastedge: App failed"))
                                    .map_err(|never| match never {})
                                    .boxed(),
                            ),
                        }
                    } else if let Some(_elapsed) = root_cause.downcast_ref::<Elapsed>() {
                        (
                            FASTEDGE_EXECUTION_TIMEOUT,
                            AppResult::TIMEOUT,
                            Full::new(Bytes::from("fastedge: Execution timeout"))
                                .map_err(|never| match never {})
                                .boxed(),
                        )
                    } else if root_cause.to_string().ends_with("deadline has elapsed") {
                        (
                            FASTEDGE_EXECUTION_TIMEOUT,
                            AppResult::TIMEOUT,
                            Full::new(Bytes::from("fastedge: Execution timeout"))
                                .map_err(|never| match never {})
                                .boxed(),
                        )
                    } else {
                        (
                            FASTEDGE_INTERNAL_ERROR,
                            AppResult::OTHER,
                            Full::new(Bytes::from("fastedge: Execute error"))
                                .map_err(|never| match never {})
                                .boxed(),
                        )
                    };
                tracing::info!(
                    "'{}' failed with status code: '{}' in {:.0?}",
                    app_name,
                    status_code,
                    time_elapsed
                );

                #[cfg(feature = "stats")]
                {
                    let stat_row = StatRow {
                        app_id: cfg.app_id,
                        client_id: cfg.client_id,
                        timestamp: timestamp as u32,
                        app_name: app_name.to_string(),
                        status_code: status_code as u32,
                        fail_reason: fail_reason as u32,
                        billing_plan: cfg.plan.to_string(),
                        time_elapsed: time_elapsed.as_micros() as u64,
                        memory_used: 0,
                        request_id: request_id.to_string(),
                        ..Default::default()
                    };
                    self.context.write_stats(stat_row);
                }
                #[cfg(not(feature = "stats"))]
                tracing::debug!(?fail_reason, request_id, "stats");

                #[cfg(feature = "metrics")]
                metrics::metrics(
                    fail_reason,
                    HTTP_LABEL,
                    Some(time_elapsed.as_micros() as u64),
                    None,
                );

                let builder = Response::builder().status(status_code);
                let res_headers = app_res_headers(cfg);
                let builder = res_headers
                    .iter()
                    .fold(builder, |builder, (k, v)| builder.header(k, v));

                builder.body(msg)?
            }
        };
        Ok(response)
    }
}

fn remote_traceparent(req: &Request<hyper::body::Incoming>) -> String {
    req.headers()
        .get(TRACEPARENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or(nanoid::nanoid!())
}

/// Creates an HTTP 500 response.
fn internal_fastedge_error(msg: &'static str) -> Result<Response<BoxBody<Bytes, anyhow::Error>>> {
    Ok(Response::builder().status(FASTEDGE_INTERNAL_ERROR).body(
        Full::new(Bytes::from(format!("fastedge: {}", msg)))
            .map_err(|never| match never {})
            .boxed(),
    )?)
}

/// Creates an HTTP 404 response.
fn not_found() -> Result<Response<BoxBody<Bytes, anyhow::Error>>> {
    Ok(Response::builder().status(StatusCode::NOT_FOUND).body(
        Full::new(Bytes::from("fastedge: Unknown app"))
            .map_err(|never| match never {})
            .boxed(),
    )?)
}

/// Creates an HTTP 429 response.
fn too_many_requests() -> Result<Response<BoxBody<Bytes, anyhow::Error>>> {
    Ok(Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .body(Empty::new().map_err(|never| match never {}).boxed())?)
}

/// Creates an HTTP 406 response.
fn not_acceptable() -> Result<Response<BoxBody<Bytes, anyhow::Error>>> {
    Ok(Response::builder()
        .status(StatusCode::NOT_ACCEPTABLE)
        .body(Empty::new().map_err(|never| match never {}).boxed())?)
}

/// borrows the request and returns the apps name
/// app name can be either as sub-domain in a format '<app_name>.<domain>' (from `Server_name` header)
/// or '<domain>/<app_name>' (from URL)
fn app_name_from_request(req: &Request<impl Body>) -> Result<SmolStr> {
    match req.headers().get("server_name") {
        None => {}
        Some(h) => {
            let full_hostname = h.to_str().unwrap();
            match full_hostname.find('.') {
                None => {}
                Some(i) => {
                    let (prefix, _) = full_hostname.split_at(i);
                    if prefix != "www" {
                        return Ok(SmolStr::from(prefix));
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
        None => Ok(SmolStr::from(path)),
        Some(i) => {
            let (prefix, _) = path.split_at(i);
            if prefix.contains('-') {
                Ok(SmolStr::from(prefix.replace('-', "_")))
            } else {
                Ok(SmolStr::from(prefix))
            }
        }
    }
}

fn app_res_headers(app_cfg: App) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.append(
        ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_str("*").unwrap(),
    );
    headers.append(CACHE_CONTROL, HeaderValue::from_str("no-store").unwrap());
    /* if specified, add/remove/overwrite response headers */
    for (name, val) in app_cfg.rsp_headers {
        if !val.is_empty() {
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
    headers
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

#[cfg(test)]
mod tests {
    use claims::*;
    use dictionary::Dictionary;
    use http_backend::{Backend, BackendStrategy, FastEdgeConnector};
    use runtime::logger::{Logger, NullAppender};
    use runtime::service::ServiceBuilder;
    use runtime::{componentize_if_necessary, PreCompiledLoader, WasiVersion, WasmConfig};
    use smol_str::ToSmolStr;
    use std::collections::HashMap;
    use test_case::test_case;
    use wasmtime::component::Component;
    use wasmtime::{Engine, Module};

    use crate::executor::HttpExecutorImpl;
    use runtime::util::stats::StatRow;

    use super::*;

    #[test_case("app.server.com", "server.com", "app"; "get app name from server_name header")]
    fn test_app_name_from_request(server_name: &str, uri: &str, expected: &str) {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri(uri)
            .header("server_name", server_name)
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let app_name = assert_ok!(app_name_from_request(&req));
        assert_eq!(expected, app_name);
    }

    #[derive(Clone)]
    struct TestContext {
        geo: HashMap<SmolStr, SmolStr>,
        app: Option<App>,
        engine: Engine,
    }

    impl ContextT for TestContext {
        type BackendConnector = FastEdgeConnector;

        fn make_logger(&self, _app_name: SmolStr, _wrk: &App) -> Logger {
            Logger::new(NullAppender)
        }

        fn backend(&self) -> Backend<FastEdgeConnector> {
            let backend = Backend::<FastEdgeConnector>::builder(BackendStrategy::FastEdge).build(
                FastEdgeConnector::new(assert_ok!("http://localhost:8080/".parse())),
            );
            backend
        }

        fn loader(&self) -> &dyn PreCompiledLoader<u64> {
            self
        }

        fn engine_ref(&self) -> &Engine {
            &self.engine
        }
    }

    static DUMMY_SAMPLE: &[u8] = include_bytes!("fixtures/dummy.wasm");
    static ALLOC_SAMPLE: &[u8] = include_bytes!("fixtures/alloc.wasm");

    impl PreCompiledLoader<u64> for TestContext {
        fn load_component(&self, id: u64) -> Result<Component> {
            let bytes = if id == 100 {
                ALLOC_SAMPLE
            } else {
                DUMMY_SAMPLE
            };
            let wasm_sample = componentize_if_necessary(bytes)?;
            Component::new(&self.engine, wasm_sample)
        }

        fn load_module(&self, _id: u64) -> Result<Module> {
            Module::new(&self.engine, DUMMY_SAMPLE)
        }
    }

    impl StatsWriter for TestContext {
        fn write_stats(&self, _stat: StatRow) {}
    }

    impl Router for TestContext {
        async fn lookup_by_name(&self, _name: &str) -> Option<App> {
            self.app.clone()
        }

        async fn lookup_by_id(&self, _id: u64) -> Option<(SmolStr, App)> {
            todo!()
        }
    }

    impl ContextHeaders for TestContext {
        fn append_headers(&self) -> impl Iterator<Item = (SmolStr, SmolStr)> {
            self.geo.iter().map(|(k, v)| (k.to_owned(), v.to_owned()))
        }
    }

    impl ExecutorFactory<HttpState<FastEdgeConnector>> for TestContext {
        type Executor = HttpExecutorImpl<FastEdgeConnector>;

        fn get_executor(
            &self,
            name: SmolStr,
            cfg: &App,
            engine: &WasmEngine<HttpState<FastEdgeConnector>>,
        ) -> Result<Self::Executor> {
            let mut dictionary = Dictionary::new();
            for (k, v) in cfg.env.iter() {
                dictionary.insert(k.to_string(), v.to_string());
            }

            let env = cfg.env.iter().collect::<Vec<(&SmolStr, &SmolStr)>>();

            let logger = self.make_logger(name.clone(), cfg);

            let version = WasiVersion::Preview2;
            let store_builder = engine
                .store_builder(version)
                .set_env(&env)
                .max_memory_size(cfg.mem_limit)
                .max_epoch_ticks(cfg.max_duration)
                .logger(logger);

            let component = self.loader().load_component(cfg.binary_id)?;
            let instance_pre = engine.component_instantiate_pre(&component)?;
            tracing::info!("Added '{}' to cache", name);
            Ok(HttpExecutorImpl::new(
                instance_pre,
                store_builder,
                self.backend(),
                dictionary,
            ))
        }
    }

    fn default_test_app(status: Status) -> Option<App> {
        Some(App {
            binary_id: 0,
            max_duration: 10,
            mem_limit: 1400000,
            env: Default::default(),
            rsp_headers: HashMap::from([
                ("RES_HEADER_01".to_smolstr(), "01".to_smolstr()),
                ("RES_HEADER_02".to_smolstr(), "02".to_smolstr()),
            ]),
            log: Default::default(),
            app_id: 12345,
            client_id: 23456,
            plan: "test_plan".to_smolstr(),
            status,
            debug_until: None,
        })
    }

    fn make_engine() -> Engine {
        let config = WasmConfig::default();
        let engine = assert_ok!(Engine::new(&config));
        engine
    }

    fn load_geo_info() -> HashMap<SmolStr, SmolStr> {
        let mut ret = HashMap::new();
        ret.insert("PoP-Lat".to_smolstr(), "47.00420".to_smolstr());
        ret.insert("PoP-Long".to_smolstr(), "28.85740".to_smolstr());
        ret.insert("PoP-Reg".to_smolstr(), "CU".to_smolstr());
        ret.insert("PoP-City".to_smolstr(), "Bucharest".to_smolstr());
        ret.insert("PoP-Continent".to_smolstr(), "EU".to_smolstr());
        ret.insert("PoP-Country-Code".to_smolstr(), "RO".to_smolstr());
        ret.insert("PoP-Country-Name".to_smolstr(), "Romania".to_smolstr());
        ret
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_success() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "success.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Enabled),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());

        let res = assert_ok!(http_service.handle_request("1", req).await);
        assert_eq!(StatusCode::OK, res.status());
        let headers = res.headers();
        assert_eq!(4, headers.len());
        assert_eq!(
            "*",
            assert_some!(headers.get("access-control-allow-origin"))
        );
        assert_eq!("no-store", assert_some!(headers.get("cache-control")));
        assert_eq!("01", assert_some!(headers.get("RES_HEADER_01")));
        assert_eq!("02", assert_some!(headers.get("RES_HEADER_02")));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_timeout() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "timeout.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let app = Some(App {
            binary_id: 1,
            max_duration: 0,
            mem_limit: 10000000,
            env: Default::default(),
            rsp_headers: HashMap::from([("RES_HEADER_03".to_smolstr(), "03".to_smolstr())]),
            log: Default::default(),
            app_id: 12345,
            client_id: 23456,
            plan: "test_plan".to_smolstr(),
            status: Status::Enabled,
            debug_until: None,
        });

        let context = TestContext {
            geo: load_geo_info(),
            app,
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());

        let res = assert_ok!(http_service.handle_request("2", req).await);
        assert_eq!(FASTEDGE_EXECUTION_TIMEOUT, res.status());
        let headers = res.headers();
        assert_eq!(3, headers.len());
        assert_eq!(
            "*",
            assert_some!(headers.get("access-control-allow-origin"))
        );
        assert_eq!("no-store", assert_some!(headers.get("cache-control")));
        assert_eq!("03", assert_some!(headers.get("RES_HEADER_03")));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_insufficient_memory() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/?size=200000")
            .header("server_name", "insufficient_memory.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let app = Some(App {
            binary_id: 100,
            max_duration: 10,
            mem_limit: 1500000,
            env: Default::default(),
            rsp_headers: HashMap::from([("RES_HEADER_03".to_smolstr(), "03".to_smolstr())]),
            log: Default::default(),
            app_id: 12345,
            client_id: 23456,
            plan: "test_plan".to_smolstr(),
            status: Status::Enabled,
            debug_until: None,
        });

        let context = TestContext {
            geo: load_geo_info(),
            app,
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());

        let res = assert_ok!(http_service.handle_request("3", req).await);
        assert_eq!(FASTEDGE_OUT_OF_MEMORY, res.status());
        let headers = res.headers();
        assert_eq!(3, headers.len());
        assert_eq!(
            "*",
            assert_some!(headers.get("access-control-allow-origin"))
        );
        assert_eq!("no-store", assert_some!(headers.get("cache-control")));
        assert_eq!("03", assert_some!(headers.get("RES_HEADER_03")));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn draft_app() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "draft.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Draft),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request("4", req).await);
        assert_eq!(StatusCode::NOT_FOUND, res.status());
        assert_eq!(0, res.headers().len());
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn disabled_app() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "draft.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Disabled),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request("5", req).await);
        assert_eq!(StatusCode::NOT_FOUND, res.status());
        assert_eq!(0, res.headers().len());
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn rate_limit_app() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "draft.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::RateLimited),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request("6", req).await);
        assert_eq!(StatusCode::TOO_MANY_REQUESTS, res.status());
        assert_eq!(0, res.headers().len());
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn suspended_app() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "draft.test.com")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            ));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Suspended),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request("7", req).await);
        assert_eq!(StatusCode::NOT_ACCEPTABLE, res.status());
        assert_eq!(0, res.headers().len());
    }
}
