use std::fmt::Debug;
use std::ops::Deref;
use std::sync::Arc;
use wasmtime_wasi_http::{HttpResult, WasiHttpCtx, WasiHttpView};

use crate::registry::CachedGraphRegistry;
use crate::store::StoreBuilder;
use http_backend::Backend;
use limiter::ProxyLimiter;
use std::time::Duration;
use wasmtime::component::{Component, Resource, ResourceTable};
use wasmtime::{
    Engine, InstanceAllocationStrategy, Module, PoolingAllocationConfig, ProfilingStrategy,
    WasmBacktraceDetails,
};
use wit_component::ComponentEncoder;

pub mod app;
mod limiter;
pub mod logger;
pub mod registry;
pub mod service;
pub mod store;
pub mod stub;
pub mod util;

use crate::logger::Logger;
use anyhow::{anyhow, bail};
pub use app::App;
use http::request::Parts;
use http::Request;
use smol_str::SmolStr;
use std::borrow::Cow;
use std::sync::mpsc::TryRecvError;
use std::thread;
use wasmtime_environ::wasmparser::{Encoding, Parser, Payload};
use wasmtime_wasi_http::bindings::http::types::ErrorCode;
use wasmtime_wasi_http::types::{
    default_send_request, HostFutureIncomingResponse, OutgoingRequest,
};

const PREVIEW1_ADAPTER: &[u8] = include_bytes!("adapters/wasi_snapshot_preview1.reactor.wasm");

#[derive(PartialEq, Copy, Clone, Debug)]
pub enum AppResult {
    SUCCESS,
    #[cfg(feature = "metrics")]
    UNKNOWN,
    TIMEOUT,
    OOM,
    OTHER,
}

pub type InstancePre<T> = Arc<wasmtime::component::InstancePre<Data<T>>>;
pub type ModuleInstancePre<T> = Arc<wasmtime::InstancePre<Data<T>>>;

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
    Preview1(wasmtime_wasi::preview1::WasiP1Ctx),
    /// Preview 2 `WasiCtx`
    Preview2(wasmtime_wasi::WasiCtx),
}

/// Host state data associated with individual [Store]s and [Instance]s.
pub struct Data<T> {
    inner: T,
    wasi: Wasi,
    // memory usage limiter
    store_limits: ProxyLimiter,
    table: ResourceTable,
    pub logger: Option<Logger>,
    http: WasiHttpCtx,
}

pub trait BackendRequest {
    fn backend_request(&mut self, head: Parts) -> anyhow::Result<(String, Parts)>;
}

impl<T> AsRef<T> for Data<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

impl<T> AsMut<T> for Data<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: Send + BackendRequest> WasiHttpView for Data<T> {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.http
    }

    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    fn send_request(
        &mut self,
        request: OutgoingRequest,
    ) -> HttpResult<Resource<HostFutureIncomingResponse>>
    where
        Self: Sized,
    {
        let default_timeout = Duration::from_millis(3000);
        let (head, body) = request.request.into_parts();
        let (authority, head) = self.inner.backend_request(head).map_err(|e| {
            tracing::warn!(cause=?e, "backend request");
            ErrorCode::InternalError(Some(e.to_string()))
        })?;
        let outgoing_request = OutgoingRequest {
            use_tls: false,
            authority,
            request: Request::from_parts(head, body),
            connect_timeout: default_timeout,
            first_byte_timeout: default_timeout,
            between_bytes_timeout: default_timeout,
        };
        default_send_request(self, outgoing_request)
    }
}

impl<T> Data<T> {
    pub fn preview1_wasi_ctx_mut(&mut self) -> &mut wasmtime_wasi::preview1::WasiP1Ctx {
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

/// Global Engine configuration for `WasmEngineBuilder`.
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
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    fn ctx(&mut self) -> &mut wasmtime_wasi::WasiCtx {
        match &mut self.wasi {
            Wasi::Preview1(_) => {
                unreachable!("using WASI Preview 1 functions with Preview 2 store")
            }
            Wasi::Preview2(ctx) => ctx,
        }
    }
}

/// Impl default Fastedge wasm config
impl Default for WasmConfig {
    fn default() -> Self {
        let mut inner = wasmtime::Config::new();
        inner.debug_info(false); // Keep this disabled - wasmtime will hang if enabled
        inner.wasm_backtrace_details(WasmBacktraceDetails::Enable);
        inner.async_support(true);
        inner.consume_fuel(false); // this is custom Gcore setting
        inner.profiler(ProfilingStrategy::None);
        inner.epoch_interruption(true); // this is custom Gcore setting
        inner.wasm_component_model(true);

        const MB: usize = 1 << 20;
        let mut pooling_allocation_config = PoolingAllocationConfig::default();

        // This number matches C@E production
        pooling_allocation_config.max_core_instance_size(MB);

        // Core wasm programs have 1 memory
        pooling_allocation_config.total_memories(100);
        pooling_allocation_config.max_memories_per_module(1);

        // allow for up to 128MiB of linear memory. Wasm pages are 64k
        pooling_allocation_config.memory_pages(128 * (MB as u64) / (64 * 1024));

        // Core wasm programs have 1 table
        pooling_allocation_config.max_tables_per_module(1);

        // Some applications create a large number of functions, in particular
        // when compiled in debug mode or applications written in swift. Every
        // function can end up in the table
        pooling_allocation_config.table_elements(98765);

        // Maximum number of slots in the pooling allocator to keep "warm", or those
        // to keep around to possibly satisfy an affine allocation request or an
        // instantiation of a module previously instantiated within the pool.
        pooling_allocation_config.max_unused_warm_slots(10);

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

pub const DEFAULT_EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(10);

/// An `WasmEngine` is a global context for the initialization and execution of WASM application.
pub struct WasmEngine<T> {
    inner: Engine,
    component_linker: ComponentLinker<T>,
    module_linker: ModuleLinker<T>,

    // WASI-NN global Graph Registry
    graph_registry: CachedGraphRegistry,
    // Matching receiver closes on drop
    _epoch_tick_handler: std::sync::mpsc::Sender<()>,
}

/// A builder interface for configuring a new [`WasmEngine`].
///
/// A new [`WasmEngineBuilder`] can be obtained with [`WasmEngine::builder`].
pub struct WasmEngineBuilder<T> {
    engine: Engine,
    component_linker: ComponentLinker<T>,
    module_linker: ModuleLinker<T>,
    epoch_tick_interval: Duration,
}

impl<T: Send + Sync> WasmEngine<T> {
    /// Creates a new [`WasmEngineBuilder`] with the given [`wasmtime::Engine`].
    pub fn builder(engine: &Engine) -> anyhow::Result<WasmEngineBuilder<T>> {
        WasmEngineBuilder::new(engine)
    }

    pub fn store_builder(&self, version: WasiVersion) -> StoreBuilder {
        StoreBuilder::new(self.inner.clone(), version, self.graph_registry.clone())
    }

    /// Creates a new [`InstancePre`] for the given [`Component`].
    pub fn component_instantiate_pre(
        &self,
        component: &Component,
    ) -> anyhow::Result<InstancePre<T>> {
        Ok(Arc::new(self.component_linker.instantiate_pre(component)?))
    }

    /// Creates a new [`InstancePre`] for the given [`Module`].
    pub fn module_instantiate_pre(&self, module: &Module) -> anyhow::Result<ModuleInstancePre<T>> {
        Ok(Arc::new(self.module_linker.instantiate_pre(module)?))
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
            epoch_tick_interval: DEFAULT_EPOCH_TICK_INTERVAL,
        })
    }

    pub fn component_linker_ref(&mut self) -> &mut ComponentLinker<T> {
        &mut self.component_linker
    }

    pub fn module_linker_ref(&mut self) -> &mut ModuleLinker<T> {
        &mut self.module_linker
    }

    fn spawn_epoch_ticker(&self) -> std::sync::mpsc::Sender<()> {
        let engine = self.engine.clone();
        let interval = self.epoch_tick_interval;
        let (send, recv) = std::sync::mpsc::channel();
        thread::spawn(move || loop {
            thread::sleep(interval);
            match recv.try_recv() {
                Ok(_) | Err(TryRecvError::Disconnected) => {
                    break;
                }
                Err(TryRecvError::Empty) => {}
            }
            engine.increment_epoch();
        });
        send
    }

    /// Builds an [`WasmEngine`] from this builder.
    pub fn build(self) -> WasmEngine<T> {
        let handler = self.spawn_epoch_ticker();
        WasmEngine {
            inner: self.engine,
            component_linker: self.component_linker,
            module_linker: self.module_linker,
            graph_registry: CachedGraphRegistry::new(),
            _epoch_tick_handler: handler,
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
    type BackendConnector;

    fn make_logger(&self, app_name: SmolStr, wrk: &App) -> Logger;

    fn backend(&self) -> Backend<Self::BackendConnector>;

    fn loader(&self) -> &dyn PreCompiledLoader<u64>;

    fn engine_ref(&self) -> &Engine;
}

pub trait ExecutorCache {
    fn remove(&self, name: &str);
    fn remove_all(&self);
}

pub trait Router: Send + Sync {
    fn lookup_by_name(&self, name: &str) -> impl std::future::Future<Output = Option<App>> + Send;
    fn lookup_by_id(
        &self,
        id: u64,
    ) -> impl std::future::Future<Output = Option<(SmolStr, App)>> + Send;
}

pub fn componentize_if_necessary(buffer: &[u8]) -> anyhow::Result<Cow<[u8]>> {
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
