use crate::app::KvStoreOption;
use crate::store::HasStats;
use http_backend::access_log::ExtRequestLogHandle;
use http_backend::stats::ExtStatsTimer;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use std::{fmt::Debug, ops::Deref};
use utils::{Dictionary, Utils};
use wasmtime_wasi::ResourceTable;
use wasmtime_wasi::WasiCtxView;
use wasmtime_wasi_http::{
    DEFAULT_FORBIDDEN_HEADERS, RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView,
    WasiHttpHooks, WasiHttpView, default_send_request,
};
use wasmtime_wasi_io::IoView;

use crate::store::StoreBuilder;
use http_backend::Backend;
use limiter::ProxyLimiter;
use wasmtime::component::Component;
use wasmtime::{
    Engine, InstanceAllocationStrategy, Module, PoolingAllocationConfig, ProfilingStrategy,
    WasmBacktraceDetails,
};
use wit_component::ComponentEncoder;

pub mod access_log;
pub mod app;
pub mod instances;
mod limiter;
pub mod logger;
mod registry;
pub mod service;
pub mod store;
pub mod stub;
pub mod trace;
pub mod util;

pub use trace::new_traceparent;

use crate::access_log::{AccessLogSender, NoopAccessLogSender};
use crate::app::SecretOption;
use crate::logger::Logger;
use crate::util::stats::StatsVisitor;
use anyhow::{anyhow, bail};
pub use app::{App, SecretValue, SecretValues};
use bytes::Bytes;
use http::request::Parts;
use http::{HeaderName, Request, Response, header};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use secret::SecretStore;
use smol_str::SmolStr;
use std::borrow::Cow;
use std::future::Future;
use wasmtime_environ::wasmparser::{Encoding, Parser, Payload};
use wasmtime_wasi_nn::wit::WasiNnCtx;

pub const DEFAULT_EPOCH_TICK_INTERVAL: u64 = 10;

const PREVIEW1_ADAPTER: &[u8] = include_bytes!("adapters/wasi_snapshot_preview1.reactor.wasm");

/// Outcome of one app call.
///
/// Reported as the `outcome` label of `fastedge_calls_total` (see the server's
/// metrics sink) and, **as its discriminant**, as `fail_reason Int32` in the
/// ClickHouse stats table. Variants are therefore append-only: never reorder
/// or remove one.
#[allow(non_camel_case_types)]
#[derive(PartialEq, Copy, Clone, Debug)]
pub enum AppResult {
    SUCCESS,
    UNKNOWN,
    TIMEOUT,
    OOM,
    OTHER,
    /// Request shed by admission control (server overloaded).
    OVERLOADED,
    /// Request rejected because the app is rate limited.
    RATE_LIMITED,
    /// Request rejected because the app is disabled or in draft.
    DISABLED,
    /// Request rejected because the app is suspended.
    SUSPENDED,
}

pub type InstancePre<T> = wasmtime::component::InstancePre<Data<T>>;
pub type ModuleInstancePre<T> = wasmtime::InstancePre<Data<T>>;

/// The version of Wasi being used
#[allow(dead_code)]
#[derive(Clone, Debug, Copy)]
pub enum WasiVersion {
    /// Preview 1
    Preview1,
    /// Preview 2
    Preview2,
}

/// Wrapper for the Preview 1 and Preview 2 versions of `WasiCtx`.
pub enum Wasi {
    /// Preview 1 `WasiCtx`
    Preview1(wasmtime_wasi::p1::WasiP1Ctx),
    /// Preview 2 `WasiCtx`
    Preview2(wasmtime_wasi::WasiCtx),
}

/// The embedder-side `wasi:http` overrides, bundled with the guest state `T`.
///
/// wasmtime 48 split the old `WasiHttpView` in two: the view now only hands out
/// the HTTP context, the resource table and a `&mut dyn WasiHttpHooks`, while
/// the overrides themselves (`send_request`, `is_forbidden_header`) moved onto
/// [`WasiHttpHooks`]. Because the hooks are borrowed out of the store data as a
/// single trait object, everything an override needs has to live in one struct.
/// That is this struct: it owns the guest state `T` (reached from the rest of
/// the runtime through `AsRef`/`AsMut` on [`Data`]) alongside the epoch
/// bookkeeping that `send_request` updates.
pub struct HttpHooks<T> {
    inner: T,
    /// Milliseconds of host I/O that should not count against the epoch
    /// deadline. Shared with the `epoch_deadline_callback` installed on the
    /// Store, which drains this counter to extend the deadline.
    epoch_pause_ms: Arc<AtomicU64>,
    /// Whether elapsed time of external HTTP should refund epoch ticks.
    pause_epoch_timeout_for_external_http: bool,
}

impl<T> HttpHooks<T> {
    pub fn new(
        inner: T,
        epoch_pause_ms: Arc<AtomicU64>,
        pause_epoch_timeout_for_external_http: bool,
    ) -> Self {
        Self {
            inner,
            epoch_pause_ms,
            pause_epoch_timeout_for_external_http,
        }
    }
}

/// Host state data associated with individual [Store]s and [Instance]s.
pub struct Data<T: 'static> {
    hooks: HttpHooks<T>,
    wasi: Wasi,
    pub wasi_nn: WasiNnCtx,
    // memory usage limiter
    store_limits: ProxyLimiter,
    pub timeout: u64,
    pub table: ResourceTable,
    pub logger: Option<Logger>,
    http: WasiHttpCtx,
    pub secret_store: SecretStore,
    pub key_value_store: key_value_store::StoreImpl,
    pub dictionary: Dictionary,
    pub utils: Utils,
    pub cache: cache::CacheImpl,
    /// Counts this instance in `fastedge_wasm_instances_live` for as long as the store —
    /// and therefore its pooling-allocator slots — is alive. Held only for its `Drop`.
    _live_instance: crate::instances::LiveInstanceGuard,
}

pub trait BackendRequest {
    fn backend_request(&mut self, head: Parts) -> anyhow::Result<Parts>;

    /// Snapshot the outbound-request access-log handle, if configured. Default
    /// `None` (no outbound-request logging). Implemented by embedders that carry
    /// a sink; used by the WASI-HTTP send path.
    fn ext_request_log_handle(&self) -> Option<ExtRequestLogHandle> {
        None
    }
}

impl<T> AsRef<T> for Data<T> {
    fn as_ref(&self) -> &T {
        &self.hooks.inner
    }
}

impl<T> AsMut<T> for Data<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.hooks.inner
    }
}

impl<T: Send> IoView for Data<T> {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

/// Boxed future used by [`WasiHttpHooks::send_request`] to report an error
/// raised while the request or response body was being processed.
type HttpIoFuture = Box<dyn Future<Output = wasmtime_wasi_http::Result<()>> + Send>;

/// Deposits host I/O wait time into the shared epoch-pause counter, carrying
/// sub-millisecond remainders across deposits so that bodies streamed in many
/// small frames do not have their wait time truncated away frame by frame.
struct EpochRefund {
    epoch_pause_ms: Arc<AtomicU64>,
    carry: Duration,
}

impl EpochRefund {
    fn new(epoch_pause_ms: Arc<AtomicU64>) -> Self {
        Self {
            epoch_pause_ms,
            carry: Duration::ZERO,
        }
    }

    fn deposit(&mut self, elapsed: Duration) {
        let total = self.carry + elapsed;
        let ms = total.as_millis() as u64;
        self.carry = total - Duration::from_millis(ms);
        if ms > 0 {
            self.epoch_pause_ms.fetch_add(ms, Ordering::Relaxed);
        }
    }
}

/// Response-body wrapper that refunds epoch ticks for time spent waiting on
/// the network. `default_send_request` resolves as soon as the response
/// *headers* are in; the body then streams lazily through the returned
/// `Response` and is only polled when the guest reads it. Any time a frame
/// poll stays `Pending` the guest is blocked on host I/O, so that time is
/// deposited into `epoch_pause_ms`, mirroring the send-phase refund in
/// [`WasiHttpHooks::send_request`].
struct ResponseBodyEpochRefund {
    inner: WasiBody,
    refund: EpochRefund,
    /// When the in-progress frame poll first returned `Pending`.
    pending_since: Option<Instant>,
}

impl Body for ResponseBodyEpochRefund {
    type Data = Bytes;
    type Error = wasmtime_wasi_http::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_frame(cx);
        match &result {
            Poll::Pending => {
                this.pending_since.get_or_insert_with(Instant::now);
            }
            Poll::Ready(_) => {
                if let Some(started) = this.pending_since.take() {
                    this.refund.deposit(started.elapsed());
                }
            }
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for ResponseBodyEpochRefund {
    fn drop(&mut self) {
        // Body dropped mid-wait (e.g. the request ended early): flush the tail.
        if let Some(started) = self.pending_since.take() {
            self.refund.deposit(started.elapsed());
        }
    }
}

/// Request-body wrapper covering the tail of the request-body lifetime.
///
/// Until the response headers arrive, [`WasiHttpHooks::send_request`] refunds
/// the whole wall clock of `default_send_request`, which already includes
/// request-body streaming, so no extra accounting is needed there (and any
/// would double count). But hyper may still be sending the request body
/// *after* the headers arrived — driven by the background `io` future — while
/// the guest blocks in a body write waiting for backpressure to clear. That
/// wait shows up as the gap between this body yielding a frame and hyper
/// polling for the next one; a `Pending` poll, by contrast, means the guest
/// itself has not produced data yet and must stay on the clock. Gaps are
/// refunded only for their portion past `headers_at`.
struct RequestBodyEpochRefund {
    inner: WasiBody,
    refund: EpochRefund,
    /// Set once the response headers are in (send-phase refund window closed).
    headers_at: Arc<OnceLock<Instant>>,
    /// When the previous poll yielded a frame.
    ready_since: Option<Instant>,
}

impl RequestBodyEpochRefund {
    /// Refund the elapsed part of the current inter-poll gap that falls
    /// outside the send-phase refund window.
    fn settle_gap(&mut self) {
        let Some(ready) = self.ready_since.take() else {
            return;
        };
        let Some(headers) = self.headers_at.get() else {
            return;
        };
        self.refund.deposit(ready.max(*headers).elapsed());
    }
}

impl Body for RequestBodyEpochRefund {
    type Data = Bytes;
    type Error = wasmtime_wasi_http::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        this.settle_gap();
        let result = Pin::new(&mut this.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(_))) = &result {
            this.ready_since = Some(Instant::now());
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for RequestBodyEpochRefund {
    fn drop(&mut self) {
        // hyper drops the body when it is done sending it (or the connection
        // failed); settle the wait between the last yielded frame and now.
        self.settle_gap();
    }
}

impl<T: Send + BackendRequest + HasStats> WasiHttpView for Data<T> {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

impl<T: Send + BackendRequest + HasStats> WasiHttpHooks for HttpHooks<T> {
    fn is_forbidden_header(&mut self, name: &HeaderName) -> bool {
        // We want to allow the host header to be set.
        if name.eq(&header::HOST) {
            return false;
        }
        // Block all headers with the reserved `fastedge` prefix — these are
        // internal routing headers that guest modules must not set.
        let name_str = name.as_str();
        if name_str.starts_with("fastedge-") || name_str.starts_with("fastedge_") {
            return true;
        }
        // Fall back to wasmtime's default forbidden-header policy.
        DEFAULT_FORBIDDEN_HEADERS.contains(name)
    }

    fn send_request(
        &mut self,
        request: Request<WasiBody>,
        options: Option<RequestOptions>,
        fut: HttpIoFuture,
    ) -> Box<
        dyn Future<Output = wasmtime_wasi_http::Result<(Response<WasiBody>, HttpIoFuture)>> + Send,
    > {
        // The request-side error channel is unused: a request rejected by
        // `backend_request` fails the whole call below instead.
        let _ = fut;

        // Capture the outbound method/target and access-log handle before the
        // request is consumed. The URI here is the original external target,
        // before `backend_request` rewrites it to the internal backend
        // authority.
        let log_method = SmolStr::new(request.method().as_str());
        let log_uri = SmolStr::new(request.uri().to_string());
        let log_handle = self.inner.ext_request_log_handle();

        let (head, body) = request.into_parts();
        let head = match self.inner.backend_request(head) {
            Ok(head) => head,
            Err(e) => {
                tracing::warn!(cause=?e, "backend request");
                let cause = e.to_string();
                return Box::new(async move {
                    Err(wasmtime_wasi_http::Error::InternalError(Some(cause)))
                });
            }
        };
        // `default_send_request` derives TLS usage from the rewritten URI's
        // scheme, so the backend rewrite above decides it.
        let request = Request::from_parts(head, body);
        // start external request stats timer
        let stats = self.inner.get_stats();
        let epoch_pause_ms = self.epoch_pause_ms.clone();
        let epoch_exclude_http_wait = self.pause_epoch_timeout_for_external_http;

        Box::new(async move {
            let _stats_timer = ExtStatsTimer::new(stats); // keep timer alive until response head is in
            // Set once the response headers are in; the request-body wrapper
            // only refunds waits that fall outside the send-phase window below.
            let headers_at = Arc::new(OnceLock::new());
            let request = if epoch_exclude_http_wait {
                request.map(|body| {
                    RequestBodyEpochRefund {
                        inner: body,
                        refund: EpochRefund::new(epoch_pause_ms.clone()),
                        headers_at: headers_at.clone(),
                        ready_since: None,
                    }
                    .boxed_unsync()
                })
            } else {
                request
            };
            let started = Instant::now();
            let sent = default_send_request(request, options).await;
            let elapsed = started.elapsed();
            if epoch_exclude_http_wait {
                epoch_pause_ms.fetch_add(elapsed.as_millis() as u64, Ordering::Relaxed);
                let _ = headers_at.set(Instant::now());
            }
            // One access-log record per outbound request (status 0 = no response
            // received, e.g. connection refused / timeout).
            if let Some(handle) = log_handle {
                let status = match &sent {
                    Ok((incoming, _)) => incoming.status().as_u16(),
                    Err(_) => 0,
                };
                handle.log(&log_method, &log_uri, status, elapsed);
            }
            let (response, io) = sent?;
            // The send-phase refund above stops at the response headers; the
            // body wrappers keep refunding network wait past that point.
            let response = if epoch_exclude_http_wait {
                response.map(|body| {
                    ResponseBodyEpochRefund {
                        inner: body.boxed_unsync(),
                        refund: EpochRefund::new(epoch_pause_ms.clone()),
                        pending_since: None,
                    }
                    .boxed_unsync()
                })
            } else {
                response.map(BodyExt::boxed_unsync)
            };
            Ok((response, Box::new(io) as HttpIoFuture))
        })
    }
}

impl<T> Data<T> {
    pub fn preview1_wasi_ctx_mut(&mut self) -> &mut wasmtime_wasi::p1::WasiP1Ctx {
        match &mut self.wasi {
            Wasi::Preview1(ctx) => ctx,
            Wasi::Preview2(_) => unreachable!("using WASI Preview 2 functions with Preview 1 ctx"),
        }
    }

    pub fn preview2_wasi_ctx_mut(&mut self) -> &mut wasmtime_wasi::WasiCtx {
        match &mut self.wasi {
            Wasi::Preview1(_) => unreachable!("using WASI Preview 1 functions with Preview 2 ctx"),
            Wasi::Preview2(ctx) => ctx,
        }
    }
}

/// Global Engine configuration used to build a [`wasmtime::Engine`].
pub struct WasmConfig {
    inner: wasmtime::Config,
}

impl Deref for WasmConfig {
    type Target = wasmtime::Config;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl AsRef<wasmtime::Config> for WasmConfig {
    fn as_ref(&self) -> &wasmtime::Config {
        &self.inner
    }
}

impl<T: Send> wasmtime_wasi::WasiView for Data<T> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        match &mut self.wasi {
            Wasi::Preview1(_) => {
                unreachable!("using WASI Preview 1 functions with Preview 2 store")
            }
            Wasi::Preview2(ctx) => WasiCtxView {
                ctx,
                table: &mut self.table,
            },
        }
    }
}

/// Impl default Fastedge wasm config
impl Default for WasmConfig {
    fn default() -> Self {
        let mut inner = wasmtime::Config::new();
        inner.debug_info(false); // Keep this disabled - wasmtime will hang if enabled

        // Standalone, non-concurrent: spend compile time once to get the fastest
        // generated code, since each module is compiled then executed hot.
        inner.cranelift_opt_level(wasmtime::OptLevel::Speed);

        // Debug build: keep full, symbolized guest backtraces. We are optimizing
        // execution CPU, not trap-path cost, and this is the standalone debug
        // runner — detailed backtraces are the whole point.
        inner.wasm_backtrace_details(WasmBacktraceDetails::Enable);

        inner.consume_fuel(false); // this is custom Gcore setting
        inner.profiler(ProfilingStrategy::None);
        inner.epoch_interruption(true); // required by store.rs timeout mechanism
        inner.wasm_component_model(true);

        // Fast instantiation: map the initialized image copy-on-write instead of
        // memcpy'ing it on every instantiation.
        inner.memory_init_cow(true);

        const MB: usize = 1 << 20;
        let mut pooling_allocation_config = PoolingAllocationConfig::default();

        // This number matches C@E production
        pooling_allocation_config.max_core_instance_size(MB);

        // Core wasm programs have 1 memory
        //pooling_allocation_config.total_memories(1000);
        //pooling_allocation_config.max_memories_per_module(1);

        // allow for up to 128MiB of linear memory. Wasm pages are 64k
        //pooling_allocation_config.memory_pages(128 * (MB as u64) / (64 * 1024));

        // Core wasm programs have 1 table
        pooling_allocation_config.max_tables_per_module(1);

        // Some applications create a large number of functions, in particular
        // when compiled in debug mode or applications written in swift. Every
        // function can end up in the table
        pooling_allocation_config.table_elements(98765);

        // No concurrency: at most one instance is ever live, so don't keep extra
        // slots warm (was 10, tuned for a multi-tenant server).
        pooling_allocation_config.max_unused_warm_slots(1);

        inner.allocation_strategy(InstanceAllocationStrategy::Pooling(
            pooling_allocation_config,
        ));

        WasmConfig { inner }
    }
}

/// An alias for [`wasmtime::component::Linker`]
pub type ComponentLinker<T> = wasmtime::component::Linker<Data<T>>;

/// An alias for [`wasmtime::Linker`]
pub type ModuleLinker<T> = wasmtime::Linker<Data<T>>;

/// An `WasmEngine` is a global context for the initialization and execution of WASM application.
pub struct WasmEngine<T: 'static> {
    inner: Engine,
    component_linker: ComponentLinker<T>,
    module_linker: ModuleLinker<T>,
}

// Manual impl: `derive(Clone)` would incorrectly require `T: Clone`, but the
// engine and linkers are internally reference-counted and clone cheaply for
// any `T`. Needed so executor factories can move an engine handle into
// `spawn_blocking` closures.
impl<T> Clone for WasmEngine<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            component_linker: self.component_linker.clone(),
            module_linker: self.module_linker.clone(),
        }
    }
}

/// A builder interface for configuring a new [`WasmEngine`].
///
/// A new [`WasmEngineBuilder`] can be obtained with [`WasmEngine::builder`].
pub struct WasmEngineBuilder<T: 'static> {
    engine: Engine,
    component_linker: ComponentLinker<T>,
    module_linker: ModuleLinker<T>,
}

impl<T: Send + Sync> WasmEngine<T> {
    /// Creates a new [`WasmEngineBuilder`] with the given [`wasmtime::Engine`].
    pub fn builder(engine: &Engine) -> anyhow::Result<WasmEngineBuilder<T>> {
        WasmEngineBuilder::new(engine)
    }

    pub fn store_builder(&self, version: WasiVersion) -> StoreBuilder {
        StoreBuilder::new(self.inner.clone(), version)
    }

    /// Creates a new [`InstancePre`] for the given [`Component`].
    pub fn component_instantiate_pre(
        &self,
        component: &Component,
    ) -> anyhow::Result<InstancePre<T>> {
        Ok(self.component_linker.instantiate_pre(component)?)
    }

    /// Creates a new [`InstancePre`] for the given [`Module`].
    pub fn module_instantiate_pre(&self, module: &Module) -> anyhow::Result<ModuleInstancePre<T>> {
        Ok(self.module_linker.instantiate_pre(module)?)
    }
}

impl<T: Send + Sync> WasmEngineBuilder<T> {
    fn new(engine: &Engine) -> anyhow::Result<Self> {
        let module_linker: ModuleLinker<T> = ModuleLinker::new(engine);
        let component_linker: ComponentLinker<T> = ComponentLinker::new(engine);

        Ok(Self {
            engine: Engine::clone(engine),
            component_linker,
            module_linker,
        })
    }

    pub fn component_linker_ref(&mut self) -> &mut ComponentLinker<T> {
        &mut self.component_linker
    }

    pub fn module_linker_ref(&mut self) -> &mut ModuleLinker<T> {
        &mut self.module_linker
    }

    /// Builds an [`WasmEngine`] from this builder.
    pub fn build(self) -> WasmEngine<T> {
        WasmEngine {
            inner: self.engine,
            component_linker: self.component_linker,
            module_linker: self.module_linker,
        }
    }
}

impl<T> Deref for WasmEngine<T> {
    type Target = Engine;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

pub trait PreCompiledLoader<K> {
    fn load_component(&self, id: K) -> anyhow::Result<Component>;
    fn load_module(&self, id: K) -> anyhow::Result<Module>;
}

pub trait ContextT {
    type BackendConnector: 'static;

    fn make_logger(&self, app_name: SmolStr, wrk: &App) -> Logger;

    fn backend(&self) -> Backend<Self::BackendConnector>;

    fn loader(&self) -> &dyn PreCompiledLoader<u64>;

    fn engine_ref(&self) -> &Engine;

    fn make_secret_store(&self, secrets: &Vec<SecretOption>) -> anyhow::Result<SecretStore>;

    fn make_key_value_store(&self, stores: &Vec<KvStoreOption>) -> key_value_store::Builder;

    fn new_stats_row(
        &self,
        request_id: &SmolStr,
        app: &SmolStr,
        caller_ip: Ipv4Addr,
        cfg: &App,
    ) -> Arc<dyn StatsVisitor>;

    /// Exact `User-Agent` value whose requests are not persisted to the stats
    /// store (synthetic probes such as a security scanner). The comparison is
    /// byte-exact and case-sensitive; `None` disables the filter.
    fn stats_excluded_user_agent(&self) -> Option<&str> {
        None
    }

    /// Sink for the per-request system log (see [`access_log`]).
    ///
    /// Distinct from the guest application log produced by [`make_logger`].
    /// Defaults to a no-op until a concrete appender (e.g. UDP syslog) is wired
    /// in, so existing embedders need not implement it.
    ///
    /// [`make_logger`]: ContextT::make_logger
    fn access_log_sender(&self) -> &dyn AccessLogSender {
        static NOOP: NoopAccessLogSender = NoopAccessLogSender;
        &NOOP
    }
}

pub trait ExecutorCache {
    /// Invalidate the cached executor for a single app.
    fn remove(&self, name: &str) -> impl std::future::Future<Output = ()> + Send;
    /// Invalidate all cached executors. Prefer per-app [`ExecutorCache::remove`]:
    /// a full flush forces a cold start (load + instantiate) for every active
    /// app on the next request, causing a latency burst.
    fn remove_all(&self);
}

pub trait Router: Send + Sync {
    fn lookup_by_name(&self, name: &str) -> impl std::future::Future<Output = Option<App>> + Send;
    fn lookup_by_id(
        &self,
        id: u64,
    ) -> impl std::future::Future<Output = Option<(SmolStr, App)>> + Send;
}

pub fn componentize_if_necessary<'a>(buffer: &'a [u8]) -> anyhow::Result<Cow<'a, [u8]>> {
    for payload in Parser::new(0).parse_all(buffer) {
        match payload {
            Ok(Payload::Version { encoding, .. }) => {
                return match encoding {
                    Encoding::Component => Ok(Cow::Borrowed(buffer)),
                    Encoding::Module => componentize(buffer).map(Cow::Owned),
                };
            }
            Err(error) => bail!("parse error: {}", error),
            _ => (),
        }
    }
    Err(anyhow!("unable to determine wasm binary encoding"))
}

fn componentize(module: &[u8]) -> anyhow::Result<Vec<u8>> {
    ComponentEncoder::default()
        .validate(true)
        .module(&module)?
        .adapter("wasi_snapshot_preview1", PREVIEW1_ADAPTER)?
        .encode()
}
