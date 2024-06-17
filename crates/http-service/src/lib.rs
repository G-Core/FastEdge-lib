use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Error, Result};
use http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL};
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use hyper::client::connect::Connect;
use hyper::server::conn::{AddrIncoming, AddrStream};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Server};
use smol_str::SmolStr;
use tokio_rustls::rustls;
use tokio_rustls::rustls::ServerConfig;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error_span, info, info_span, trace, warn, Instrument};
use wasi_common::I32Exit;
use wasmtime::Trap;
use wasmtime_wasi_nn::WasiNnCtx;

use http_backend::Backend;
use runtime::app::Status;
use runtime::service::Service;
use runtime::{App, AppResult, ContextT, Router, WasmEngine, WasmEngineBuilder};
#[cfg(feature = "metrics")]
use runtime::util::metrics;

use crate::executor::{ HttpExecutor};
#[cfg(feature = "stats")]
use runtime::util::stats::StatRow;
use runtime::util::stats::StatsWriter;

use crate::tls::{load_certs, load_private_key, TlsAcceptor, TlsStream};
pub mod executor;

mod tls;

pub use crate::executor::ExecutorFactory;

pub(crate) static TRACEPARENT: &str = "traceparent";

const FASTEDGE_INTERNAL_ERROR: u16 = 530;
const FASTEDGE_OUT_OF_MEMORY: u16 = 531;
const FASTEDGE_EXECUTION_TIMEOUT: u16 = 532;
const FASTEDGE_EXECUTION_PANIC: u16 = 533;

pub struct HttpsConfig {
    pub ssl_certs: &'static str,
    pub ssl_pkey: &'static str,
}

pub struct HttpConfig {
    pub all_interfaces: bool,
    pub port: u16,
    pub https: Option<HttpsConfig>,
    pub cancel: CancellationToken,
}

pub struct HttpService<T: ContextT> {
    engine: WasmEngine<HttpState<T::BackendConnector>>,
    context: T,
}

pub struct HttpState<C> {
    wasi_nn: WasiNnCtx,
    http_backend: Backend<C>,
}

pub trait ContextHeaders {
    fn append_headers(&self) -> impl Iterator<Item = (SmolStr, SmolStr)>;
}

impl<T> Service for HttpService<T>
where
    T: ContextT
        + StatsWriter
        + Router
        + ContextHeaders
        + ExecutorFactory<HttpState<T::BackendConnector>>
        + Sync
        + Send
        + 'static,
    T::BackendConnector: Connect + Clone + Send + Sync + 'static,
    T::Executor: HttpExecutor + Send + Sync,
{
    type State = HttpState<T::BackendConnector>;
    type Config = HttpConfig;
    type Context = T;

    fn new(engine: WasmEngine<Self::State>, context: Self::Context) -> Result<Self> {
        Ok(Self { engine, context })
    }

    /// Run hyper http service
    async fn run(self, config: Self::Config) -> Result<()> {
        let interface: [u8; 4] = if config.all_interfaces {
            [0, 0, 0, 0]
        } else {
            [127, 0, 0, 1]
        };
        let listen_addr = (interface, config.port).into();

        if let Some(https) = config.https {
            let tls = {
                // Load public certificate.
                let certs = load_certs(https.ssl_certs)?;
                // Load private key.
                let key = load_private_key(https.ssl_pkey)?;
                // Do not use client certificate authentication.
                let mut cfg = rustls::ServerConfig::builder()
                    .with_safe_defaults()
                    .with_no_client_auth()
                    .with_single_cert(certs, key)
                    .map_err(|e| anyhow!(format!("{}", e)))?;
                // Configure ALPN to accept HTTP/2, HTTP/1.1 in that order.
                cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
                Arc::new(cfg)
            };
            self.serve_tls(listen_addr, tls).await?
        } else {
            self.serve(listen_addr).await?
        };

        Ok(())
    }

    fn configure_engine(builder: &mut WasmEngineBuilder<Self::State>) -> Result<()> {
        wasmtime_wasi::add_to_linker_async(builder.component_linker_ref())?;
        wasmtime_wasi_nn::wit::ML::add_to_linker(builder.component_linker_ref(), |data| {
            &mut data.as_mut().wasi_nn
        })?;

        reactor::gcore::fastedge::http_client::add_to_linker(
            builder.component_linker_ref(),
            |data| &mut data.as_mut().http_backend,
        )?;
        Ok(())
    }
}

impl<T> HttpService<T>
where
    T: ContextT
        + StatsWriter
        + Router
        + ContextHeaders
        + ExecutorFactory<HttpState<T::BackendConnector>>
        + Sync
        + Send
        + 'static,
    T::BackendConnector: Clone + Send + Sync + 'static,
    T::Executor: HttpExecutor + Send + Sync,
{
    async fn serve(self, listen_addr: SocketAddr) -> Result<()> {
        let service = Arc::new(self);
        let make_service = make_service_fn(|_conn: &AddrStream| {
            let service = service.clone();
            async move {
                let service = service_fn(move |req| {
                    let self_ = service.clone();
                    let request_id = remote_traceparent(&req);
                    async move {
                        self_
                            .handle_request(req)
                            .instrument(info_span!("http_handler", request_id))
                            .await
                    }
                });
                Ok::<_, Error>(service)
            }
        });
        info!("Listening on http://{}", listen_addr);
        Server::try_bind(&listen_addr)
            .with_context(|| format!("Unable to listen on {}", listen_addr))?
            .serve(make_service)
            .await?;
        Ok(())
    }

    async fn serve_tls(self, listen_addr: SocketAddr, tls: Arc<ServerConfig>) -> Result<()> {
        let service = Arc::new(self);
        let make_service = make_service_fn(|_conn: &TlsStream| {
            let service = service.clone();
            async move {
                let service = service_fn(move |req| {
                    let service = service.clone();
                    let request_id = remote_traceparent(&req);
                    async move {
                        service
                            .handle_request(req)
                            .instrument(error_span!("https_handler", request_id))
                            .await
                    }
                });
                Ok::<_, Error>(service)
            }
        });

        let incoming = AddrIncoming::bind(&listen_addr)
            .with_context(|| format!("Unable to bind on {}", listen_addr))?;
        info!("Listening on https://{}", listen_addr);
        Server::builder(TlsAcceptor::new(tls, incoming))
            .serve(make_service)
            .await?;
        Ok(())
    }

    /// handle HTTP request.
    async fn handle_request(&self, mut request: Request<Body>) -> Result<Response<Body>> {
        #[cfg(feature = "stats")]
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("current time")
            .as_secs();

        debug!(?request, "process");
        request
            .headers_mut()
            .extend(app_req_headers(self.context.append_headers()));

        // get application name from request URL
        let app_name = match app_name_from_request(&request) {
            Err(error) => {
                #[cfg(feature = "metrics")]
                metrics::metrics(AppResult::UNKNOWN);
                info!(cause=?error, "App name not provided");
                return not_found();
            }
            Ok(app_name) => app_name,
        };
        // lookup for application config and binary_id
        info!(
            "Processing request for application '{}' on URL: {}",
            app_name,
            request.uri()
        );
        let cfg = match self.context.lookup_by_name(&app_name).await {
            None => {
                #[cfg(feature = "metrics")]
                metrics::metrics(AppResult::UNKNOWN);
                info!(
                    "Request for unknown application '{}' on URL: {}",
                    app_name,
                    request.uri()
                );
                return not_found();
            }
            Some(cfg) if cfg.status == Status::Draft || cfg.status == Status::Disabled => {
                info!(
                    "Request for disabled application '{}' on URL: {}",
                    app_name,
                    request.uri()
                );
                return not_found();
            }
            Some(cfg) if cfg.status == Status::RateLimited => {
                info!(
                    "Request for rate limited application '{}' on URL: {}",
                    app_name,
                    request.uri()
                );
                return too_many_requests();
            }
            Some(app_cfg) if app_cfg.status == Status::Suspended => {
                info!(
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
                metrics::metrics(AppResult::UNKNOWN);
                warn!(cause=?error,
                    "failure on getting context"
                );
                return internal_fastedge_error("context error");
            }
        };

        let start_ = std::time::Instant::now();

        let response = match executor.execute(request).await {
            Ok((mut response, time_elapsed, memory_used)) => {
                info!(
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
                        app_name,
                        status_code: response.status().as_u16() as u32,
                        fail_reason: 0, // TODO: use AppResult
                        billing_plan: cfg.plan.clone(),
                        time_elapsed: time_elapsed.as_micros() as u64,
                        memory_used: memory_used.as_u64(),
                        .. Default::default()
                    };
                    self.context.write_stats(stat_row).await;
                }
                #[cfg(feature = "metrics")]
                metrics::metrics(AppResult::SUCCESS);

                response.headers_mut().extend(app_res_headers(cfg));
                response
            }
            Err(error) => {
                let root_cause = error.root_cause();
                let time_elapsed = std::time::Instant::now().duration_since(start_);
                let (status_code, fail_reason, msg) =
                    if let Some(exit) = root_cause.downcast_ref::<I32Exit>() {
                        if exit.0 == 0 {
                            (StatusCode::OK.as_u16(), AppResult::SUCCESS, Body::empty())
                        } else {
                            (
                                FASTEDGE_EXECUTION_PANIC,
                                AppResult::OTHER,
                                Body::from("fastedge: App failed"),
                            )
                        }
                    } else if let Some(trap) = root_cause.downcast_ref::<Trap>() {
                        match trap {
                            Trap::Interrupt => (
                                FASTEDGE_EXECUTION_TIMEOUT,
                                AppResult::TIMEOUT,
                                Body::from("fastedge: Execution timeout"),
                            ),
                            Trap::UnreachableCodeReached => (
                                FASTEDGE_OUT_OF_MEMORY,
                                AppResult::OOM,
                                Body::from("fastedge: Out of memory"),
                            ),
                            _ => (
                                FASTEDGE_EXECUTION_PANIC,
                                AppResult::OTHER,
                                Body::from("fastedge: App failed"),
                            ),
                        }
                    } else {
                        (
                            FASTEDGE_INTERNAL_ERROR,
                            AppResult::OTHER,
                            Body::from("fastedge: Execute error"),
                        )
                    };
                info!(
                    "'{}' failed with status code: '{}' in {:.0?}",
                    app_name, status_code, time_elapsed
                );

                #[cfg(feature = "stats")]
                {
                    let stat_row = StatRow {
                        app_id: cfg.app_id,
                        client_id: cfg.client_id,
                        timestamp: timestamp as u32,
                        app_name,
                        status_code: status_code as u32,
                        fail_reason: fail_reason as u32,
                        billing_plan: cfg.plan.clone(),
                        time_elapsed: time_elapsed.as_micros() as u64,
                        memory_used: 0,
                        .. Default::default()
                    };
                    self.context.write_stats(stat_row).await;
                }
                #[cfg(not(feature = "stats"))]
                debug!(?fail_reason, "stats");

                #[cfg(feature = "metrics")]
                metrics::metrics(fail_reason);

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

fn remote_traceparent(req: &Request<Body>) -> String {
    req.headers()
        .get(TRACEPARENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .unwrap_or(nanoid::nanoid!())
}

/// Creates an HTTP 500 response.
fn internal_fastedge_error(msg: &'static str) -> Result<Response<Body>> {
    Ok(Response::builder()
        .status(FASTEDGE_INTERNAL_ERROR)
        .body(Body::from(format!("fastedge: {}", msg)))?)
}

/// Creates an HTTP 404 response.
fn not_found() -> Result<Response<Body>> {
    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("fastedge: Unknown app"))?)
}

/// Creates an HTTP 429 response.
fn too_many_requests() -> Result<Response<Body>> {
    Ok(Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .body(Body::empty())?)
}

/// Creates an HTTP 406 response.
fn not_acceptable() -> Result<Response<Body>> {
    Ok(Response::builder()
        .status(StatusCode::NOT_ACCEPTABLE)
        .body(Body::empty())?)
}

/// borrows the request and returns the apps name
/// app name can be either as sub-domain in a format '<app_name>.<domain>' (from `Server_name` header)
/// or '<domain>/<app_name>' (from URL)
fn app_name_from_request(req: &Request<Body>) -> Result<SmolStr> {
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

    return match path.find('/') {
        None => Ok(SmolStr::from(path)),
        Some(i) => {
            let (prefix, _) = path.split_at(i);
            if prefix.contains('-') {
                Ok(SmolStr::from(prefix.replace('-', "_")))
            } else {
                Ok(SmolStr::from(prefix))
            }
        }
    };
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
                debug!("Unable to parse header name: {}", name);
                continue;
            };
            let Ok(value) = val.parse::<HeaderValue>() else {
                debug!("Unable to parse header value: {}", val);
                continue;
            };
            headers.insert(key, value);
        }
    }
    headers
}

fn app_req_headers<'a>(geo: impl Iterator<Item = (SmolStr, SmolStr)>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (key, value) in geo {
        trace!("append new request header {}={}", key, value);
        match key.parse::<HeaderName>() {
            Ok(name) => match value.parse::<HeaderValue>() {
                Ok(value) => {
                    headers.insert(name, value);
                }
                Err(error) => warn!(cause=?error, "could not parse http value: {}", value),
            },
            Err(error) => warn!(cause=?error, "could not parse http header: {}", key),
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::stats::StatRow;
    use claims::*;
    use test_case::test_case;
    use wasmtime::component::Component;
    use wasmtime::{Engine, Module};

    use crate::executor::HttpExecutorImpl;
    use http_backend::{Backend, BackendStrategy, FastEdgeConnector};
    use runtime::logger::{Logger, NullAppender};
    use runtime::service::ServiceBuilder;
    use runtime::{componentize_if_necessary, PreCompiledLoader, WasiVersion, WasmConfig};

    use super::*;

    #[test_case("app.server.com", "server.com", "app"; "get app name from server_name header")]
    fn test_app_name_from_request(server_name: &str, uri: &str, expected: &str) {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri(uri)
            .header("server_name", server_name)
            .body(hyper::Body::from("")));

        let app_name = assert_ok!(app_name_from_request(&req));
        assert_eq!(expected, app_name);
    }

    #[derive(Clone)]
    struct TestContext {
        geo: HashMap<Cow<'static, str>, Cow<'static, str>>,
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
        async fn write_stats(&self, _stat: StatRow) {}
    }

    impl Router for TestContext {
        async fn lookup_by_name(&self, _name: &str) -> Option<App> {
            self.app.clone()
        }
    }

    impl ContextHeaders for TestContext {
        fn append_headers(&self) -> impl Iterator<Item = (Cow<str>, Cow<str>)> {
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
            let env = cfg
                .env
                .iter()
                .map(|(k, v)| (k, v))
                .collect::<Vec<(&String, &String)>>();

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
                ("RES_HEADER_01".to_string(), "01".to_string()),
                ("RES_HEADER_02".to_string(), "02".to_string()),
            ]),
            log: Default::default(),
            app_id: 12345,
            client_id: 23456,
            plan: "test_plan".to_string(),
            status,
            debug_until: None,
        })
    }

    fn make_engine() -> Engine {
        let config = WasmConfig::default();
        let engine = assert_ok!(Engine::new(&config));
        engine
    }

    fn load_geo_info() -> HashMap<Cow<'static, str>, Cow<'static, str>> {
        let mut ret = HashMap::new();
        ret.insert(Cow::Borrowed("PoP-Lat"), Cow::Borrowed("47.00420"));
        ret.insert(Cow::Borrowed("PoP-Long"), Cow::Borrowed("28.85740"));
        ret.insert(Cow::Borrowed("PoP-Reg"), Cow::Borrowed("CU"));
        ret.insert(Cow::Borrowed("PoP-City"), Cow::Borrowed("Bucharest"));
        ret.insert(Cow::Borrowed("PoP-Continent"), Cow::Borrowed("EU"));
        ret.insert(Cow::Borrowed("PoP-Country-Code"), Cow::Borrowed("RO"));
        ret.insert(Cow::Borrowed("PoP-Country-Name"), Cow::Borrowed("Romania"));
        ret
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_success() {
        let req = assert_ok!(http::Request::builder()
            .method("GET")
            .uri("http://www.rust-lang.org/")
            .header("server_name", "success.test.com")
            .body(hyper::Body::from("")));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Enabled),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());

        let res = assert_ok!(http_service.handle_request(req).await);
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
            .body(hyper::Body::from("")));

        let app = Some(App {
            binary_id: 1,
            max_duration: 0,
            mem_limit: 10000000,
            env: Default::default(),
            rsp_headers: HashMap::from([("RES_HEADER_03".to_string(), "03".to_string())]),
            log: Default::default(),
            app_id: 12345,
            client_id: 23456,
            plan: "test_plan".to_string(),
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

        let res = assert_ok!(http_service.handle_request(req).await);
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
            .body(hyper::Body::from("")));

        let app = Some(App {
            binary_id: 100,
            max_duration: 10,
            mem_limit: 1500000,
            env: Default::default(),
            rsp_headers: HashMap::from([("RES_HEADER_03".to_string(), "03".to_string())]),
            log: Default::default(),
            app_id: 12345,
            client_id: 23456,
            plan: "test_plan".to_string(),
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

        let res = assert_ok!(http_service.handle_request(req).await);
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
            .body(hyper::Body::from("")));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Draft),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request(req).await);
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
            .body(hyper::Body::from("")));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Disabled),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request(req).await);
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
            .body(hyper::Body::from("")));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::RateLimited),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request(req).await);
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
            .body(hyper::Body::from("")));

        let context = TestContext {
            geo: load_geo_info(),
            app: default_test_app(Status::Suspended),
            engine: make_engine(),
        };

        let http_service: HttpService<TestContext> =
            assert_ok!(ServiceBuilder::new(context).build());
        let res = assert_ok!(http_service.handle_request(req).await);
        assert_eq!(StatusCode::NOT_ACCEPTABLE, res.status());
        assert_eq!(0, res.headers().len());
    }
}