use reactor::gcore::fastedge::key_value;
use slab::Slab;
use smol_str::SmolStr;
use std::collections::HashMap;
use wasmtime::component::Resource;

pub use key_value::{Error, Value};

#[async_trait::async_trait]
pub trait Store: Sync + Send{
    async fn get(&mut self, key: &str) -> Result<Option<Value>, Error>;

    async fn get_by_range(&mut self, key: &str, min: u32, max: u32) -> Result<Vec<Value>, Error>;

    async fn bf_exists(&mut self, bf: &str, key: &str) -> Result<bool, Error>;
}

#[async_trait::async_trait]
pub trait StoreManager: Sync + Send {
    type StoreType: Store;
    /// Get a store by db url.
    async fn get_store(&mut self, param: &str) -> Result<Self::StoreType, Error>;
}

#[derive(Clone)]
pub struct KeyValueStore<M: StoreManager> {
    allowed_stores: HashMap<SmolStr, SmolStr>,
    manager: M,
    stores: Slab<M::StoreType>,
}

impl<M: StoreManager> key_value::HostStore for KeyValueStore<M> {
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

    async fn get_by_range(
        &mut self,
        store: Resource<key_value::Store>,
        key: String,
        min: u32,
        max: u32,
    ) -> Result<Vec<Value>, Error> {
        let store_id = store.rep();
        KeyValueStore::get_by_range(self, store_id, &key, min, max).await
    }

    async fn bf_exists(
        &mut self,
        store: Resource<key_value::Store>,
        bf: String,
        key: String,
    ) -> Result<bool, Error> {
        let store_id = store.rep();
        KeyValueStore::bf_exists(self, store_id, &bf, &key).await
    }

    async fn drop(&mut self, store: Resource<key_value::Store>) -> Result<(), wasmtime::Error> {
        self.stores.remove(store.rep() as usize);
        Ok(())
    }
}

impl<M: StoreManager> key_value::Host for KeyValueStore<M> {}

impl<M: StoreManager> KeyValueStore<M> {
    pub fn new(allowed_stores: Vec<(SmolStr, SmolStr)>, manager: M) -> Self {
        Self {
            allowed_stores: allowed_stores.into_iter().collect(),
            manager,
            stores: Slab::new(),
        }
    }

    /// Open a store by name. Return the store ID.
    pub async fn open(&mut self, name: &str) -> Result<u32, Error> {
        if let Some(param) = self.allowed_stores.get(name) {
            let store = self.manager.get_store(&param).await?;
            Ok(self.stores.insert(store) as u32)
        } else {
            Err(Error::AccessDenied)
        }
    }

    /// Get a value from a store by key.
    pub async fn get(&mut self, store: u32, key: &str) -> Result<Option<Value>, Error> {
        let Some(store) = self.stores.get_mut(store as usize) else {
            return Err(Error::NoSuchStore);
        };
        store.get(key).await
    }

    /// Get a values from a store by key.
    pub async fn get_by_range(
        &mut self,
        store: u32,
        key: &str,
        min: u32,
        max: u32,
    ) -> Result<Vec<Value>, Error> {
        let Some(store) = self.stores.get_mut(store as usize) else {
            return Err(Error::NoSuchStore);
        };
        store.get_by_range(key, min, max).await
    }

    /// Get a value from a store by key.
    pub async fn bf_exists(&mut self, store: u32, bf: &str, key: &str) -> Result<bool, Error> {
        let Some(store) = self.stores.get_mut(store as usize) else {
            return Err(Error::NoSuchStore);
        };
        store.bf_exists(bf, key).await
    }
}

impl<M: StoreManager + Default> Default for KeyValueStore<M> {
    fn default() -> Self {
        Self {
            allowed_stores: Default::default(),
            manager: M::default(),
            stores: Slab::new(),
        }
    }
}

pub struct NoSuchStoreManager;

pub struct NoStore;

#[async_trait::async_trait]
impl Store for NoStore {
    async fn get(&mut self, _key: &str) -> Result<Option<Value>, Error> {
        Err(Error::NoSuchStore)
    }

    async fn get_by_range(&mut self, _key: &str, _min: u32, _max: u32) -> Result<Vec<Value>, Error> {
        Err(Error::NoSuchStore)
    }

    async fn bf_exists(&mut self, _bf: &str, _key: &str) -> Result<bool, Error> {
        Err(Error::NoSuchStore)
    }
}

#[async_trait::async_trait]
impl StoreManager for NoSuchStoreManager {
    type StoreType = NoStore;

    async fn get_store(&mut self, _name: &str) -> Result<Self::StoreType, Error> {
        Err(Error::NoSuchStore)
    }
}
