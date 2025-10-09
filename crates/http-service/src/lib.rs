use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use wasmtime::component::HasSelf;
use wasmtime_wasi_nn::wit::WasiNnView;

pub use crate::executor::ExecutorFactory;
use crate::executor::HttpExecutor;
use anyhow::{bail, Error, Result};
use bytes::Bytes;
use bytesize::ByteSize;
use http::{
    header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL},
    HeaderMap, HeaderName, HeaderValue, StatusCode,
};
use http_body_util::{BodyExt, Empty, Full};
use hyper::{body::Body, server::conn::http1, service::service_fn};
use hyper_util::{client::legacy::connect::Connect, rt::TokioIo};
#[cfg(feature = "metrics")]
use runtime::util::metrics;
#[cfg(feature = "stats")]
use runtime::util::stats::StatRow;
use runtime::{
    app::Status, service::Service, util::stats::StatsWriter, App, AppResult, ContextT, Router,
    WasmEngine, WasmEngineBuilder,
};
use smol_str::SmolStr;
#[cfg(feature = "stats")]
use smol_str::ToSmolStr;
use state::HttpState;
use tokio::{net::TcpListener, time::error::Elapsed};
pub use wasmtime_wasi_http::body::HyperOutgoingBody;

pub mod executor;
pub mod state;

pub(crate) static TRACEPARENT: &str = "traceparent";

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

#[derive(Default)]
pub struct HttpConfig {
    pub all_interfaces: bool,
    pub port: u16,
    #[cfg(target_family = "unix")]
    pub cancel: std::sync::Weak<shellflip::ShutdownHandle>,
    pub listen_fd: Option<OwnedFd>,
    pub backoff: u64,
}

pub struct HttpService<T: ContextT> {
    engine: WasmEngine<HttpState<T::BackendConnector>>,
    context: T,
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
        + Clone
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
                                        .handle_request(&request_id, req)
                                        .instrument(tracing::debug_span!("http", request_id))
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
        // Allow re-importing of `wasi:clocks/wall-clock@0.2.0`
        wasmtime_wasi::p2::add_to_linker_async(linker)?;
        linker.allow_shadowing(true);
        wasmtime_wasi_http::add_to_linker_async(linker)?;

        wasmtime_wasi_nn::wit::add_to_linker(linker, |data: &mut runtime::Data<Self::State>| {
            WasiNnView::new(&mut data.table, &mut data.wasi_nn)
        })?;

        reactor::gcore::fastedge::http_client::add_to_linker::<_, HasSelf<_>>(linker, |data| {
            &mut data.as_mut().http_backend
        })?;

        reactor::gcore::fastedge::dictionary::add_to_linker::<_, HasSelf<_>>(linker, |data| {
            &mut data.dictionary
        })?;

        reactor::gcore::fastedge::secret::add_to_linker::<_, HasSelf<_>>(linker, |data| {
            &mut data.secret_store
        })?;

        reactor::gcore::fastedge::key_value::add_to_linker::<_, HasSelf<_>>(linker, |data| {
            &mut data.key_value_store
        })?;

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
        + 'static
        + Clone,
    T::BackendConnector: Clone + Send + Sync + 'static,
    T::Executor: HttpExecutor + Send + Sync,
{
    /// handle HTTP request.
    async fn handle_request<B>(
        &self,
        request_id: &str,
        mut request: hyper::Request<B>,
    ) -> Result<hyper::Response<HyperOutgoingBody>>
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

        let response_handler = {
            let app_name = app_name.clone();
            #[cfg(feature = "stats")]
            let billing_plan = cfg.plan.clone();
            #[cfg(feature = "stats")]
            let request_id = request_id.to_smolstr();
            #[cfg(feature = "stats")]
            let context = self.context.clone();

            move |status_code: StatusCode, mem_used: ByteSize, time_elapsed: Duration| {
                tracing::info!(
                    "'{}' completed with status code: '{}' in {:.0?} using {} of WebAssembly heap",
                    app_name,
                    status_code,
                    time_elapsed,
                    mem_used
                );
                #[cfg(feature = "stats")]
                {
                    let stat_row = StatRow {
                        app_id: cfg.app_id,
                        client_id: cfg.client_id,
                        timestamp: timestamp as u32,
                        app_name,
                        status_code: status_code.as_u16() as u32,
                        fail_reason: 0, // TODO: use AppResult
                        billing_plan,
                        time_elapsed: time_elapsed.as_micros() as u64,
                        memory_used: mem_used.as_u64(),
                        request_id,
                    };
                    context.write_stats(stat_row);
                }
                #[cfg(feature = "metrics")]
                metrics::metrics(
                    AppResult::SUCCESS,
                    &["http"],
                    Some(time_elapsed.as_micros() as u64),
                    Some(mem_used.as_u64()),
                );
            }
        };

        let response = match executor.execute(request, response_handler).await {
            Ok(mut response) => {
                response.headers_mut().extend(app_res_headers(cfg));
                response
            }
            Err(error) => {
                tracing::warn!(cause=?error, "execute");
                let time_elapsed = std::time::Instant::now().duration_since(start_);

                let (status_code, fail_reason, msg) = map_err(error);
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
                        app_name: app_name,
                        status_code: status_code as u32,
                        fail_reason: fail_reason as u32,
                        billing_plan: cfg.plan.clone(),
                        time_elapsed: time_elapsed.as_micros() as u64,
                        memory_used: 0,
                        request_id: request_id.to_smolstr(),
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

                let builder = hyper::Response::builder().status(status_code);
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

fn map_err(error: Error) -> (u16, AppResult, HyperOutgoingBody) {
    let root_cause = error.root_cause();
    let (status_code, fail_reason, msg) =
        if let Some(exit) = root_cause.downcast_ref::<wasi_common::I32Exit>() {
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
        } else if let Some(trap) = root_cause.downcast_ref::<wasmtime::Trap>() {
            match trap {
                wasmtime::Trap::Interrupt => (
                    FASTEDGE_EXECUTION_TIMEOUT,
                    AppResult::TIMEOUT,
                    Full::new(Bytes::from("fastedge: Execution timeout"))
                        .map_err(|never| match never {})
                        .boxed(),
                ),
                wasmtime::Trap::UnreachableCodeReached => (
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
    (status_code, fail_reason, msg)
}

fn remote_traceparent(req: &hyper::Request<hyper::body::Incoming>) -> String {
    req.headers()
        .get(TRACEPARENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or(nanoid::nanoid!())
}

/// Creates an HTTP 500 response.
fn internal_fastedge_error(msg: &'static str) -> Result<hyper::Response<HyperOutgoingBody>> {
    Ok(hyper::Response::builder()
        .status(FASTEDGE_INTERNAL_ERROR)
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

/// borrows the request and returns the apps name
/// app name can be either as sub-domain in a format '<app_name>.<domain>' (from `Server_name` header)
/// or '<domain>/<app_name>' (from URL)
fn app_name_from_request(req: &hyper::Request<impl Body>) -> Result<SmolStr> {
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

    use crate::app_name_from_request;
    use bytes::Bytes;
    use claims::assert_ok;
    use http_body_util::{BodyExt, Empty};

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
}
