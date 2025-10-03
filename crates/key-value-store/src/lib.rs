#[cfg(feature = "redis")]
mod redis_impl;

use reactor::gcore::fastedge::key_value;
use slab::Slab;
use smol_str::SmolStr;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::instrument;
use wasmtime::component::Resource;

pub use key_value::{Error, Value};

#[cfg(feature = "redis")]
pub use redis_impl::RedisStore;

#[async_trait::async_trait]
pub trait Store: Sync + Send {
    async fn get(&self, key: &str) -> Result<Option<Value>, Error>;

    async fn zrange(&self, key: &str, min: f64, max: f64) -> Result<Vec<Value>, Error>;

    async fn scan(&self, pattern: &str) -> Result<Vec<String>, Error>;

    async fn zscan(&self, key: &str, pattern: &str) -> Result<Vec<(Value, f64)>, Error>;

    async fn bf_exists(&self, key: &str, item: &str) -> Result<bool, Error>;
}

#[async_trait::async_trait]
pub trait StoreManager: Sync + Send {
    /// Get a store by db url.
    async fn get_store(&self, param: &str) -> Result<Arc<dyn Store>, Error>;
}

#[derive(Clone)]
pub struct KeyValueStore {
    allowed_stores: HashMap<SmolStr, SmolStr>,
    manager: Arc<dyn StoreManager>,
    stores: Slab<Arc<dyn Store>>,
}

impl key_value::HostStore for KeyValueStore {
    async fn open(&mut self, name: String) -> Result<Resource<key_value::Store>, Error> {
        let store_id = KeyValueStore::open(self, &name).await?;
        Ok(Resource::new_own(store_id))
    }

    async fn get(
        &mut self,
        store: Resource<key_value::Store>,
        key: String,
    ) -> Result<Option<Vec<u8>>, Error> {
        let store_id = store.rep();
        KeyValueStore::get(self, store_id, &key).await
    }

    async fn scan(
        &mut self,
        store: Resource<key_value::Store>,
        pattern: String,
    ) -> Result<Vec<String>, Error> {
        let store_id = store.rep();
        KeyValueStore::scan(self, store_id, &pattern).await
    }

    async fn zrange(
        &mut self,
        store: Resource<key_value::Store>,
        key: String,
        min: f64,
        max: f64,
    ) -> Result<Vec<Value>, Error> {
        let store_id = store.rep();
        KeyValueStore::zrange(self, store_id, &key, min, max).await
    }

    async fn zscan(
        &mut self,
        store: Resource<key_value::Store>,
        key: String,
        pattern: String,
    ) -> Result<Vec<(Value, f64)>, Error> {
        let store_id = store.rep();
        KeyValueStore::zscan(self, store_id, &key, &pattern).await
    }

    async fn bf_exists(
        &mut self,
        store: Resource<key_value::Store>,
        key: String,
        item: String,
    ) -> Result<bool, Error> {
        let store_id = store.rep();
        KeyValueStore::bf_exists(self, store_id, &key, &item).await
    }

    async fn drop(&mut self, store: Resource<key_value::Store>) -> Result<(), wasmtime::Error> {
        self.stores.remove(store.rep() as usize);
        Ok(())
    }
}

impl key_value::Host for KeyValueStore {}

impl KeyValueStore {
    #[instrument(skip(manager), level = "trace")]
    pub fn new(allowed_stores: Vec<(SmolStr, SmolStr)>, manager: Arc<dyn StoreManager>) -> Self {
        Self {
            allowed_stores: allowed_stores.into_iter().collect(),
            manager,
            stores: Slab::new(),
        }
    }

    /// Open a store by name. Return the store ID.
    #[instrument(skip(self), level = "trace", ret, err)]
    pub async fn open(&mut self, name: &str) -> Result<u32, Error> {
        if let Some(param) = self.allowed_stores.get(name) {
            let store = self.manager.get_store(&param).await?;
            Ok(self.stores.insert(store) as u32)
        } else {
            Err(Error::AccessDenied)
        }
    }

    /// Get a value from a store by key.
    #[instrument(skip(self), level = "trace", ret, err)]
    pub async fn get(&self, store: u32, key: &str) -> Result<Option<Value>, Error> {
        let Some(store) = self.stores.get(store as usize) else {
            return Err(Error::NoSuchStore);
        };
        store.get(key).await
    }

    /// Get a values from a store by key.
    #[instrument(skip(self), level = "trace", ret, err)]
    pub async fn zrange(
        &self,
        store: u32,
        key: &str,
        min: f64,
        max: f64,
    ) -> Result<Vec<Value>, Error> {
        let Some(store) = self.stores.get(store as usize) else {
            return Err(Error::NoSuchStore);
        };
        store.zrange(key, min, max).await
    }

    #[instrument(skip(self), level = "trace", ret, err)]
    pub async fn scan(&mut self, store: u32, pattern: &str) -> Result<Vec<String>, Error> {
        let Some(store) = self.stores.get(store as usize) else {
            return Err(Error::NoSuchStore);
        };
        store.scan(pattern).await
    }

    #[instrument(skip(self), level = "trace", ret, err)]
    pub async fn zscan(
        &mut self,
        store: u32,
        key: &str,
        pattern: &str,
    ) -> Result<Vec<(Value, f64)>, Error> {
        let Some(store) = self.stores.get(store as usize) else {
            return Err(Error::NoSuchStore);
        };
        store.zscan(key, pattern).await
    }

    /// Get a value from a store by key.
    #[instrument(skip(self), level = "trace", ret, err)]
    pub async fn bf_exists(&self, store: u32, key: &str, item: &str) -> Result<bool, Error> {
        let Some(store) = self.stores.get(store as usize) else {
            return Err(Error::NoSuchStore);
        };
        store.bf_exists(key, item).await
    }
}

impl Default for KeyValueStore {
    fn default() -> Self {
        Self {
            allowed_stores: Default::default(),
            manager: Arc::new(NoSuchStoreManager),
            stores: Slab::new(),
        }
    }
}

pub struct NoSuchStoreManager;

#[async_trait::async_trait]
impl StoreManager for NoSuchStoreManager {
    async fn get_store(&self, _name: &str) -> Result<Arc<dyn Store>, Error> {
        Err(Error::NoSuchStore)
    }
}
