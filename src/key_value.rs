use key_value_store::{Error, Store, StoreManager, Value};
use std::sync::Arc;

struct CliStore;

pub(crate) struct CliStoreManager;

#[async_trait::async_trait]
impl StoreManager for CliStoreManager {
    async fn get_store(&self, _name: &str) -> Result<Arc<dyn Store>, Error> {
        Ok(Arc::new(CliStore))
    }
}

#[async_trait::async_trait]
impl Store for CliStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, Error> {
        Ok(Some(key.as_bytes().to_vec()))
    }

    async fn zrange(&self, _key: &str, _min: u32, _max: u32) -> Result<Vec<Value>, Error> {
        todo!()
    }

    async fn scan(&self, _pattern: &str) -> Result<Vec<String>, Error> {
        todo!()
    }

    async fn zscan(&self, _key: &str, _pattern: &str) -> Result<Vec<(Value, u32)>, Error> {
        todo!()
    }

    async fn cf_exists(&self, _key: &str, _item: &str) -> Result<bool, Error> {
        todo!()
    }
}
