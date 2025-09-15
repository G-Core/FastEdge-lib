use crate::limiter::ProxyLimiter;
use crate::logger::Logger;
use crate::registry::CachedGraphRegistry;
use crate::{Data, Wasi, WasiVersion, DEFAULT_EPOCH_TICK_INTERVAL};
use anyhow::Result;
use key_value_store::KeyValueStore;
use secret::SecretStore;
use std::{
    collections::HashMap,
    fmt::{Debug, Formatter},
    ops::{Deref, DerefMut},
};
use tracing::{debug, instrument, trace};
use wasmtime::component::ResourceTable;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_nn::wit::WasiNnCtx;

/// A `Store` holds the runtime state of a app instance.
///
/// `Store` lives only for the lifetime of a single app invocation.
pub struct Store<T: 'static> {
    inner: wasmtime::Store<Data<T>>,
}

impl<T> AsRef<wasmtime::Store<Data<T>>> for Store<T> {
    fn as_ref(&self) -> &wasmtime::Store<Data<T>> {
        &self.inner
    }
}

impl<T> AsMut<wasmtime::Store<Data<T>>> for Store<T> {
    fn as_mut(&mut self) -> &mut wasmtime::Store<Data<T>> {
        &mut self.inner
    }
}

impl<T: 'static> wasmtime::AsContext for Store<T> {
    type Data = Data<T>;

    fn as_context(&self) -> wasmtime::StoreContext<'_, Self::Data> {
        self.inner.as_context()
    }
}

impl<T> wasmtime::AsContextMut for Store<T> {
    fn as_context_mut(&mut self) -> wasmtime::StoreContextMut<'_, Self::Data> {
        self.inner.as_context_mut()
    }
}

impl<T> Store<T> {
    pub fn memory_used(&self) -> usize {
        self.inner.data().store_limits.allocated
    }
}

impl<T> Deref for Store<T> {
    type Target = wasmtime::Store<Data<T>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> DerefMut for Store<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// A builder interface for configuring a new [`Store`].
///
/// A new [`StoreBuilder`] can be obtained with [`crate::Engine::store_builder`].
#[derive(Clone)]
pub struct StoreBuilder {
    engine: wasmtime::Engine,
    max_duration: u64,
    store_limits: ProxyLimiter,
    env: Vec<(String, String)>,
    logger: Option<Logger>,
    version: WasiVersion,
    properties: HashMap<String, String>,
    registry: CachedGraphRegistry,
    secret_store: SecretStore,
    key_value_store: KeyValueStore,
}

impl StoreBuilder {
    // Called by Engine::store_builder.
    pub(crate) fn new(engine: wasmtime::Engine, version: WasiVersion) -> Self {
        Self {
            engine,
            max_duration: 100,
            store_limits: ProxyLimiter::default(),
            env: vec![],
            logger: None,
            version,
            properties: Default::default(),
            registry: CachedGraphRegistry::new(),
            secret_store: Default::default(),
            key_value_store: KeyValueStore::default(),
        }
    }

    /// Sets a maximum memory allocation limit.
    ///
    /// See [`wasmtime::ResourceLimiter::memory_growing`] (`maximum`) for
    /// details on how this limit is enforced.
    pub fn max_memory_size(self, max_memory_size: usize) -> Self {
        trace!(
            "set max memory size: {:.0?}",
            bytesize::ByteSize::b(max_memory_size as u64)
        );
        let store_limits = ProxyLimiter::new(max_memory_size);
        Self {
            store_limits,
            ..self
        }
    }

    /// Sets max number of epoch ticks
    ///
    /// See documentation on
    /// [`Config::epoch_interruption()`](crate::Config::epoch_interruption)
    /// for an introduction to epoch-based interruption.
    pub fn max_epoch_ticks(self, max_duration: u64) -> Self {
        Self {
            max_duration,
            ..self
        }
    }

    /// Set ENV variables
    pub fn set_env(self, env: &[(impl AsRef<str>, impl AsRef<str>)]) -> Self {
        let env = env
            .iter()
            .map(|(k, v)| (k.as_ref().to_owned(), v.as_ref().to_owned()))
            .collect();
        Self { env, ..self }
    }

    /// Set backend proxy address
    pub fn logger(self, logger: Logger) -> Self {
        Self {
            logger: Some(logger),
            ..self
        }
    }

    pub fn with_properties(self, properties: HashMap<String, String>) -> Self {
        Self { properties, ..self }
    }

    /// Set secret store
    pub fn secret_store(self, secret_store: SecretStore) -> Self {
        Self {
            secret_store,
            ..self
        }
    }

    /// Set key value store
    pub fn key_value_store(self, key_value_store: KeyValueStore) -> Self {
        Self {
            key_value_store,
            ..self
        }
    }

    pub fn make_wasi_nn(&self) -> Result<WasiNnCtx> {
        // initialize application specific graph
        let backends: Vec<&str> = self
            .env
            .iter()
            .find(|(k, _)| k == "FASTEDGE_NN_BACKENDS")
            .map(|(_, v)| v.split(",").collect())
            .unwrap_or_default();

        debug!("wasi-nn backends: {:?}", backends);

        let preload_graphs: Vec<(String, String)> = backends
            .into_iter()
            .flat_map(|b| {
                let entry = self.env.iter().filter_map(|(k, v)| {
                    if k.starts_with(&format!("FASTEDGE_NN_{}", b.to_ascii_uppercase())) {
                        Some((b.to_string(), v.to_string()))
                    } else {
                        None
                    }
                });
                entry
            })
            .collect();
        debug!("preload graphs: {:?}", preload_graphs);

        let (backends, registry) = self.registry.preload_graphs(preload_graphs)?;

        Ok(WasiNnCtx::new(backends, registry))
    }

    /// Builds a [`Store`] from this builder with `Default` WasiCtxBuilder
    pub fn build<T>(self, inner: T) -> Result<Store<T>> {
        let wasi_builder = WasiCtxBuilder::new();
        self.build_with_wasi(wasi_builder, inner)
    }

    /// Builds a [`Store`] from this builder with given WasiCtxBuilder.
    #[instrument(skip(self, wasi_ctx_builder, inner), err, level = "debug")]
    pub(crate) fn build_with_wasi<T>(
        self,
        mut wasi_ctx_builder: WasiCtxBuilder,
        inner: T,
    ) -> Result<Store<T>> {
        let table = ResourceTable::new();

        wasi_ctx_builder.envs(&self.env);
        let wasi_nn = self.make_wasi_nn()?;

        let logger = if let Some(mut logger) = self.logger {
            logger.extend(self.properties);
            wasi_ctx_builder.stdout(logger.clone());
            Some(logger)
        } else {
            if cfg!(debug_assertions) {
                wasi_ctx_builder.inherit_stdout();
                wasi_ctx_builder.inherit_stderr();
            }
            None
        };

        let wasi = match self.version {
            WasiVersion::Preview1 => Wasi::Preview1(wasi_ctx_builder.build_p1()),
            WasiVersion::Preview2 => Wasi::Preview2(wasi_ctx_builder.build()),
        };

        let mut inner = wasmtime::Store::new(
            &self.engine,
            Data {
                inner,
                wasi,
                wasi_nn,
                store_limits: self.store_limits,
                timeout: (self.max_duration + 1) * DEFAULT_EPOCH_TICK_INTERVAL,
                table,
                logger,
                http: WasiHttpCtx::new(),
                secret_store: self.secret_store,
                key_value_store: self.key_value_store,
            },
        );
        inner.limiter(|state| &mut state.store_limits);
        // allow max number of epoch ticks (1 tick = 10 ms)
        inner.set_epoch_deadline(self.max_duration); // allow max number of epoch ticks (1 tick = 10 ms)
        inner.epoch_deadline_trap();
        Ok(Store { inner })
    }
}

impl Debug for StoreBuilder {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreBuilder")
            .field("version", &self.version)
            .field("env", &self.env)
            .field("max_duration", &self.max_duration)
            .field("store_limits", &self.store_limits)
            .finish()
    }
}
