use key_value_store::{Error, Store, StoreManager, Value};

pub struct CliStore;

#[derive(Default)]
pub struct CliStoreManager;

#[async_trait::async_trait]
impl StoreManager for CliStoreManager {
    type StoreType = CliStore;

    async fn get_store(&mut self, _name: &str) -> Result<CliStore, Error> {
        Ok(CliStore)
    }
}

#[async_trait::async_trait]
impl Store for CliStore {
    async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        Ok(Some(key.as_bytes().to_vec()))
    }

    async fn get_by_range(&mut self, _key: &str, _min: u32, _max: u32) -> Result<Vec<Value>, Error> {
        todo!()
    }

    async fn bf_exists(&mut self, _bf: &str, _key: &str) -> Result<bool, Error> {
        todo!()
    }
}
