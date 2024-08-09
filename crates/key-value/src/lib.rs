use std::sync::Arc;

use async_trait::async_trait;
use redis::AsyncCommands;
use wasmtime::component::Resource;

use reactor::gcore::fastedge::key_value;

//mod redis_store;

type ResourceStore = Resource<key_value::Store>;

#[async_trait]
pub trait Store: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>>;
}

pub trait StoreFactory {
    fn create(&self, name: &str) -> Arc<dyn Store>;
}

#[derive(Clone)]
pub struct KeyValueResource {
    /*stores: Slab<Arc<dyn Store>>,
    factory: F,*/
    redis_connection: redis::aio::ConnectionManager
}

#[async_trait]
impl key_value::Host for KeyValueResource {}

#[async_trait]
impl key_value::HostStore for KeyValueResource {
    async fn open(
        &mut self,
        _name: String,
    ) -> wasmtime::Result<Result<Resource<key_value::Store>, key_value::Error>> {
        Ok(async {
            //let id = self.stores.insert(self.factory.create(&name));
            Ok(Resource::new_own(1))
        }
        .await)

    }

    async fn get(
        &mut self,
        _store: Resource<key_value::Store>,
        key: String,
    ) -> wasmtime::Result<Result<Option<Vec<u8>>, key_value::Error>> {
        Ok(async {
            /*let store = self
                .stores
                .get(store.rep() as usize)
                .ok_or(key_value::Error::Other("unknown resource".to_string()))?;
           store
                .get(&key)
                .await
                .map_err(|e| key_value::Error::Other(e.to_string()))*/
            self.redis_connection.get(&key).await.map_err(|e| key_value::Error::Other(e.to_string()))
        }
        .await)
    }

    fn drop(&mut self, _store: ResourceStore) -> wasmtime::Result<()> {
        /*let _ = self.stores.remove(store.rep() as usize);*/
        Ok(())
    }
}

impl KeyValueResource {
   pub fn new(redis_connection: redis::aio::ConnectionManager) -> Self {
        Self {
            redis_connection,
        }
    }
}
