use crate::executor::RunExecutor;
use crate::key_value::CliStoreManager;
use crate::secret::SecretImpl;
use http_backend::stats::ExtRequestStats;
use http_backend::Backend;
use http_service::executor::{HttpExecutorImpl, WasiHttpExecutorImpl};
use http_service::state::HttpState;
use http_service::{ContextHeaders, ExecutorFactory};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use key_value_store::ReadStats;
use runtime::app::{KvStoreOption, SecretOption};
use runtime::logger::{Console, Logger};
use runtime::util::stats::{CdnPhase, StatsVisitor};
use runtime::{
    componentize_if_necessary, App, ContextT, ExecutorCache, PreCompiledLoader, Router,
    WasiVersion, WasmEngine,
};
use secret::SecretStore;
use smol_str::SmolStr;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;
use utils::{Dictionary, UserDiagStats};
use wasmtime::component::Component;
use wasmtime::{Engine, Module};

/// Test tool execution context
#[derive(Clone)]
pub struct Context {
    pub(crate) headers: HashMap<SmolStr, SmolStr>,
    pub(crate) engine: Engine,
    pub(crate) app: Option<App>,
    pub(crate) backend: Backend<HttpsConnector<HttpConnector>>,
    pub(crate) wasm_bytes: Vec<u8>,
    pub(crate) wasi_http: bool,
}

impl PreCompiledLoader<u64> for Context {
    fn load_component(&self, _id: u64) -> anyhow::Result<Component> {
        let wasm_sample = componentize_if_necessary(&self.wasm_bytes)?;
        Component::new(&self.engine, wasm_sample)
    }

    fn load_module(&self, _id: u64) -> anyhow::Result<Module> {
        unreachable!("")
    }
}

impl ContextT for Context {
    type BackendConnector = HttpsConnector<HttpConnector>;
    fn make_logger(&self, _app_name: SmolStr, _wrk: &App) -> Logger {
        Logger::new(Console::default())
    }

    fn backend(&self) -> Backend<HttpsConnector<HttpConnector>> {
        self.backend.to_owned()
    }

    fn loader(&self) -> &dyn PreCompiledLoader<u64> {
        self
    }

    fn engine_ref(&self) -> &Engine {
        &self.engine
    }

    fn make_secret_store(&self, secrets: &Vec<SecretOption>) -> anyhow::Result<SecretStore> {
        let mut secret_impl = SecretImpl::default();
        for s in secrets {
            if let Some(value) = s.secret_values.first() {
                secret_impl.insert(s.name.to_string(), value.value.to_string());
            }
        }
        Ok(SecretStore::new(Arc::new(secret_impl)))
    }

    fn make_key_value_store(&self, stores: &Vec<KvStoreOption>) -> key_value_store::Builder {
        let allowed_stores = stores
            .iter()
            .map(|s| (s.name.clone(), s.param.clone()))
            .collect();
        let manager = CliStoreManager {
            stores: stores.clone(),
        };
        key_value_store::Builder::new(allowed_stores, Arc::new(manager))
    }

    fn new_stats_row(
        &self,
        _request_id: &SmolStr,
        _app: &SmolStr,
        _cfg: &App,
    ) -> Arc<dyn StatsVisitor> {
        Arc::new(StatsStub::default())
    }
}

impl ExecutorFactory<HttpState<HttpsConnector<HttpConnector>>> for Context {
    type Executor = RunExecutor;

    fn get_executor(
        &self,
        name: SmolStr,
        app: &App,
        engine: &WasmEngine<HttpState<HttpsConnector<HttpConnector>>>,
    ) -> anyhow::Result<Self::Executor> {
        let mut dictionary = Dictionary::new();
        for (k, v) in app.env.iter() {
            dictionary.insert(k.to_string(), v.to_string());
        }

        let env = app.env.iter().collect::<Vec<(&SmolStr, &SmolStr)>>();

        let logger = self.make_logger(name, app);
        let secret_store = self.make_secret_store(app.secrets.as_ref())?;
        let key_value_store = self.make_key_value_store(app.kv_stores.as_ref());

        let version = WasiVersion::Preview2;
        let store_builder = engine
            .store_builder(version)
            .set_env(&env)
            .max_memory_size(app.mem_limit)
            .max_epoch_ticks(app.max_duration)
            .logger(logger)
            .secret_store(secret_store)
            .key_value_store(key_value_store)
            .dictionary(dictionary);

        let component = self.loader().load_component(app.binary_id)?;
        let instance_pre = engine.component_instantiate_pre(&component)?;
        if self.wasi_http {
            Ok(RunExecutor::Wasi(WasiHttpExecutorImpl::new(
                instance_pre,
                store_builder,
                self.backend(),
            )))
        } else {
            Ok(RunExecutor::Http(HttpExecutorImpl::new(
                instance_pre,
                store_builder,
                self.backend(),
            )))
        }
    }
}

impl ExecutorCache for Context {
    fn remove(&self, _name: &str) {
        unreachable!()
    }

    fn remove_all(&self) {
        unreachable!()
    }
}

impl ContextHeaders for Context {
    fn append_headers(&self) -> impl Iterator<Item = (SmolStr, SmolStr)> {
        self.headers.iter().map(|(k, v)| (k.clone(), v.clone()))
    }
}

impl Router for Context {
    async fn lookup_by_name(&self, _name: &str) -> Option<App> {
        self.app.to_owned()
    }

    async fn lookup_by_id(&self, _id: u64) -> Option<(SmolStr, App)> {
        unreachable!()
    }
}

#[derive(Default)]
pub struct StatsStub {
    elapsed: AtomicU64,
    memory_used: AtomicU64,
}

impl ReadStats for StatsStub {
    fn count_kv_read(&self, _value: i32) {}

    fn count_kv_byod_read(&self, _value: i32) {}
}

impl UserDiagStats for StatsStub {
    fn set_user_diag(&self, _diag: &str) {}
}

impl ExtRequestStats for StatsStub {
    fn observe_ext(&self, _elapsed: Duration) {}
}

impl StatsVisitor for StatsStub {
    fn status_code(&self, _status_code: u16) {}

    fn memory_used(&self, memory_used: u64) {
        self.memory_used
            .store(memory_used, std::sync::atomic::Ordering::Relaxed);
    }

    fn fail_reason(&self, _fail_reason: u32) {}

    fn observe(&self, elapsed: Duration) {
        self.elapsed.store(
            elapsed.as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn get_time_elapsed(&self) -> u64 {
        self.elapsed.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn get_memory_used(&self) -> u64 {
        self.memory_used.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn cdn_phase(&self, _phase: CdnPhase) {}
}
