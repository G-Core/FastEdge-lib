use std::sync::Arc;

use async_trait::async_trait;
use redis::aio::ConnectionLike;
use slab::Slab;
use wasmtime::component::Resource;

use reactor::gcore::fastedge::key_value;

mod redis_store;

type ResourceStore = Resource<key_value::Store>;

#[async_trait]
pub trait Store: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>>;
}

pub trait StoreFactory {
    fn create(&self, name: &str) -> Arc<dyn Store>;
}

#[derive(Clone)]
pub struct KeyValueResource<F> {
    stores: Slab<Arc<dyn Store>>,
    factory: F,
}

#[async_trait]
impl<F: StoreFactory + Send + Sync> key_value::Host for KeyValueResource<F> {}

#[async_trait]
impl<F: StoreFactory + Send + Sync> key_value::HostStore for KeyValueResource<F> {
    async fn open(
        &mut self,
        name: String,
    ) -> wasmtime::Result<Result<Resource<key_value::Store>, key_value::Error>> {
        Ok(async {
            let id = self.stores.insert(self.factory.create(&name));
            Ok(Resource::new_own(id as u32))
        }
        .await)
    }

    async fn get(
        &mut self,
        store: Resource<key_value::Store>,
        key: String,
    ) -> wasmtime::Result<Result<Option<Vec<u8>>, key_value::Error>> {
        Ok(async {
            let store = self
                .stores
                .get(store.rep() as usize)
                .ok_or(key_value::Error::Other("unknown resource".to_string()))?;
           store
                .get(&key)
                .await
                .map_err(|e| key_value::Error::Other(e.to_string()))
        }
        .await)
    }

    fn drop(&mut self, store: ResourceStore) -> wasmtime::Result<()> {
        let _ = self.stores.remove(store.rep() as usize);
        Ok(())
    }
}

impl<F: StoreFactory> KeyValueResource<F> {
    fn new(factory: F) -> Self {
        Self {
            stores: Slab::new(),
            factory,
        }
    }
}
