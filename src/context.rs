use crate::executor::RunExecutor;
use crate::key_value::CliStoreManager;
use crate::secret::SecretImpl;
use dictionary::Dictionary;
use http_backend::Backend;
use http_service::executor::{HttpExecutorImpl, WasiHttpExecutorImpl};
use http_service::state::HttpState;
use http_service::{ContextHeaders, ExecutorFactory};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use key_value_store::KeyValueStore;
use runtime::app::{KvStoreOption, SecretOption};
use runtime::logger::{Console, Logger};
use runtime::util::stats::{StatRow, StatsWriter};
use runtime::{
    componentize_if_necessary, App, ContextT, ExecutorCache, PreCompiledLoader, Router,
    WasiVersion, WasmEngine,
};
use secret::SecretStore;
use smol_str::SmolStr;
use std::collections::HashMap;
use std::sync::Arc;
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

    fn make_key_value_store(&self, stores: &Vec<KvStoreOption>) -> KeyValueStore {
        let allowed_stores = stores
            .iter()
            .map(|s| (s.name.clone(), s.param.clone()))
            .collect();
        let manager = CliStoreManager {
            stores: stores.to_owned(),
        };
        KeyValueStore::new(allowed_stores, Arc::new(manager))
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
            .key_value_store(key_value_store);

        let component = self.loader().load_component(app.binary_id)?;
        let instance_pre = engine.component_instantiate_pre(&component)?;
        if self.wasi_http {
            Ok(RunExecutor::Wasi(WasiHttpExecutorImpl::new(
                instance_pre,
                store_builder,
                self.backend(),
                dictionary,
            )))
        } else {
            Ok(RunExecutor::Http(HttpExecutorImpl::new(
                instance_pre,
                store_builder,
                self.backend(),
                dictionary,
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

impl StatsWriter for Context {
    fn write_stats(&self, _stat: StatRow) {}
}
