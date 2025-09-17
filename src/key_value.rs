use key_value_store::{Error, Store, StoreManager};
use std::sync::Arc;
use key_value_store::RedisStore;
use runtime::app::KvStoreOption;


pub(crate) struct CliStoreManager {
    pub(crate) stores: Vec<KvStoreOption>,
}

#[async_trait::async_trait]
impl StoreManager for CliStoreManager {
    async fn get_store(&self, name: &str) -> Result<Arc<dyn Store>, Error> {
        let Some(opts) = self.stores.iter().find(|store| store.name == name) else {
            return Err(Error::NoSuchStore);
        };
        let store = RedisStore::open(&opts.param).await?;
        Ok(Arc::new(store))
    }
}


