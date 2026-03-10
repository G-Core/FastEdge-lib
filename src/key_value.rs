use key_value_store::{Error, Store, StoreManager};
use key_value_store::{ReadStats, RedisStore};
use runtime::app::KvStoreOption;
use std::sync::Arc;

pub(crate) struct CliStoreManager {
    pub(crate) stores: Vec<KvStoreOption>,
}

#[async_trait::async_trait]
impl StoreManager for CliStoreManager {
    async fn get_store(
        &self,
        param: &str,
        _stats: Arc<dyn ReadStats>,
    ) -> Result<Arc<dyn Store>, Error> {
        let Some(opts) = self.stores.iter().find(|store| store.param == param) else {
            return Err(Error::NoSuchStore);
        };
        let store = RedisStore::open(&opts.param).await?;
        Ok(Arc::new(store))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol_str::SmolStr;

    struct NoOpStats;

    impl ReadStats for NoOpStats {
        fn count_kv_read(&self, _: i32) {}
        fn count_kv_byod_read(&self, _: i32) {}
    }

    fn make_kv_option(name: &str, param: &str) -> KvStoreOption {
        KvStoreOption {
            param: SmolStr::new(param),
            name: SmolStr::new(name),
            prefix: Default::default(),
            cache_size: 1000,
            cache_ttl: 60,
        }
    }

    #[tokio::test]
    async fn get_store_returns_no_such_store_when_empty() {
        let manager = CliStoreManager { stores: vec![] };
        let result = manager
            .get_store("redis://localhost:6379", Arc::new(NoOpStats))
            .await;
        assert!(matches!(result, Err(Error::NoSuchStore)));
    }

    #[tokio::test]
    async fn get_store_returns_no_such_store_when_param_unmatched() {
        let manager = CliStoreManager {
            stores: vec![make_kv_option("mystore", "redis://localhost:6379")],
        };
        let result = manager
            .get_store("redis://other:6379", Arc::new(NoOpStats))
            .await;
        assert!(matches!(result, Err(Error::NoSuchStore)));
    }

    #[tokio::test]
    async fn get_store_searches_by_param_not_by_name() {
        // get_store receives a Redis URL (param); passing the store name must return NoSuchStore
        let manager = CliStoreManager {
            stores: vec![make_kv_option("mystore", "redis://localhost:6379")],
        };
        let result = manager.get_store("mystore", Arc::new(NoOpStats)).await;
        assert!(matches!(result, Err(Error::NoSuchStore)));
    }
}
