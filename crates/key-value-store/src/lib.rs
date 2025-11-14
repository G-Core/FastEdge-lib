#[cfg(feature = "redis")]
mod redis_impl;

use reactor::gcore::fastedge::key_value;
use slab::Slab;
use smol_str::SmolStr;
use std::sync::Arc;
use tracing::instrument;
use wasmtime::component::Resource;

pub use key_value::{Error, Value};

#[cfg(feature = "redis")]
pub use redis_impl::RedisStore;

#[async_trait::async_trait]
pub trait Store: Sync + Send {
    async fn get(&self, key: &str) -> Result<Option<Value>, Error>;

    async fn zrange_by_score(
        &self,
        key: &str,
        min: f64,
        max: f64,
    ) -> Result<Vec<(Value, f64)>, Error>;

    async fn scan(&self, pattern: &str) -> Result<Vec<String>, Error>;

    async fn zscan(&self, key: &str, pattern: &str) -> Result<Vec<(Value, f64)>, Error>;

    async fn bf_exists(&self, key: &str, item: &str) -> Result<bool, Error>;
}

#[async_trait::async_trait]
pub trait StoreManager: Sync + Send {
    /// Get a store by db url.
    async fn get_store(
        &self,
        param: &str,
        metric: Arc<dyn ReadStats>,
    ) -> Result<Arc<dyn Store>, Error>;
}

/// Key-Value store read metrics
pub trait ReadStats: Sync + Send {
    /// Increment key-value read count and size
    fn count_kv_read(&self, value: i32);
    /// Increment key-value read count and size for BYOD
    fn count_kv_byod_read(&self, value: i32);
}

#[derive(Clone)]
pub struct Builder {
    allowed_stores: Vec<(SmolStr, SmolStr)>,
    manager: Arc<dyn StoreManager>,
}

pub struct StoreImpl {
    allowed_stores: Vec<(SmolStr, SmolStr)>,
    manager: Arc<dyn StoreManager>,
    stores: Slab<Arc<dyn Store>>,
    stats: Arc<dyn ReadStats>,
}

impl Builder {
    pub fn new(allowed_stores: Vec<(SmolStr, SmolStr)>, manager: Arc<dyn StoreManager>) -> Self {
        Self {
            allowed_stores,
            manager,
        }
    }

    pub fn build(self, stats: Arc<dyn ReadStats>) -> StoreImpl {
        StoreImpl {
            allowed_stores: self.allowed_stores,
            manager: self.manager,
            stores: Slab::new(),
            stats,
        }
    }
}

impl key_value::HostStore for StoreImpl {
    async fn open(&mut self, name: String) -> Result<Resource<key_value::Store>, Error> {
        let store_id = StoreImpl::open(self, &name).await?;
        Ok(Resource::new_own(store_id))
    }

    async fn get(
        &mut self,
        store: Resource<key_value::Store>,
        key: String,
    ) -> Result<Option<Vec<u8>>, Error> {
        let store_id = store.rep();
        StoreImpl::get(self, store_id, &key).await
    }

    async fn scan(
        &mut self,
        store: Resource<key_value::Store>,
        pattern: String,
    ) -> Result<Vec<String>, Error> {
        let store_id = store.rep();
        StoreImpl::scan(self, store_id, &pattern).await
    }

    async fn zrange_by_score(
        &mut self,
        store: Resource<key_value::Store>,
        key: String,
        min: f64,
        max: f64,
    ) -> Result<Vec<(Value, f64)>, Error> {
        let store_id = store.rep();
        StoreImpl::zrange_by_score(self, store_id, &key, min, max).await
    }

    async fn zscan(
        &mut self,
        store: Resource<key_value::Store>,
        key: String,
        pattern: String,
    ) -> Result<Vec<(Value, f64)>, Error> {
        let store_id = store.rep();
        StoreImpl::zscan(self, store_id, &key, &pattern).await
    }

    async fn bf_exists(
        &mut self,
        store: Resource<key_value::Store>,
        key: String,
        item: String,
    ) -> Result<bool, Error> {
        let store_id = store.rep();
        StoreImpl::bf_exists(self, store_id, &key, &item).await
    }

    async fn drop(&mut self, store: Resource<key_value::Store>) -> Result<(), wasmtime::Error> {
        self.stores.remove(store.rep() as usize);
        Ok(())
    }
}

impl key_value::Host for StoreImpl {}

impl StoreImpl {
    /// Open a store by name. Return the store ID.
    #[instrument(skip(self), level = "trace", ret, err)]
    pub async fn open(&mut self, name: &str) -> Result<u32, Error> {
        if let Some(param) =
            self.allowed_stores
                .iter()
                .find_map(|s| (s.0 == name).then_some(&s.1))
        {
            let store = self.manager.get_store(param, self.stats.clone()).await?;
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
    pub async fn zrange_by_score(
        &self,
        store: u32,
        key: &str,
        min: f64,
        max: f64,
    ) -> Result<Vec<(Value, f64)>, Error> {
        let Some(store) = self.stores.get(store as usize) else {
            return Err(Error::NoSuchStore);
        };
        store.zrange_by_score(key, min, max).await
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

impl Default for Builder {
    fn default() -> Self {
        Self {
            allowed_stores: Default::default(),
            manager: Arc::new(NoSuchStoreManager),
        }
    }
}

pub struct NoSuchStoreManager;

#[async_trait::async_trait]
impl StoreManager for NoSuchStoreManager {
    async fn get_store(
        &self,
        _name: &str,
        _metric: Arc<dyn ReadStats>,
    ) -> Result<Arc<dyn Store>, Error> {
        Err(Error::NoSuchStore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicI32;

    // Mock implementation of Store
    struct MockStore {
        data: HashMap<String, Value>,
        zset_data: HashMap<String, Vec<(Value, f64)>>,
        bloom_filters: HashMap<String, Vec<String>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                data: HashMap::new(),
                zset_data: HashMap::new(),
                bloom_filters: HashMap::new(),
            }
        }

        fn with_data(mut self, key: &str, value: Value) -> Self {
            self.data.insert(key.to_string(), value);
            self
        }

        fn with_zset(mut self, key: &str, items: Vec<(Value, f64)>) -> Self {
            self.zset_data.insert(key.to_string(), items);
            self
        }

        fn with_bloom(mut self, key: &str, items: Vec<String>) -> Self {
            self.bloom_filters.insert(key.to_string(), items);
            self
        }
    }

    #[async_trait::async_trait]
    impl Store for MockStore {
        async fn get(&self, key: &str) -> Result<Option<Value>, Error> {
            Ok(self.data.get(key).cloned())
        }

        async fn zrange_by_score(
            &self,
            key: &str,
            min: f64,
            max: f64,
        ) -> Result<Vec<(Value, f64)>, Error> {
            if let Some(items) = self.zset_data.get(key) {
                let filtered: Vec<_> = items
                    .iter()
                    .filter(|(_, score)| *score >= min && *score <= max)
                    .cloned()
                    .collect();
                Ok(filtered)
            } else {
                Ok(vec![])
            }
        }

        async fn scan(&self, pattern: &str) -> Result<Vec<String>, Error> {
            let keys: Vec<String> = self
                .data
                .keys()
                .filter(|k| k.contains(pattern))
                .cloned()
                .collect();
            Ok(keys)
        }

        async fn zscan(&self, key: &str, pattern: &str) -> Result<Vec<(Value, f64)>, Error> {
            if let Some(items) = self.zset_data.get(key) {
                let filtered: Vec<_> = items
                    .iter()
                    .filter(|(val, _)| {
                        if let Some(s) = String::from_utf8(val.clone()).ok() {
                            s.contains(pattern)
                        } else {
                            false
                        }
                    })
                    .cloned()
                    .collect();
                Ok(filtered)
            } else {
                Ok(vec![])
            }
        }

        async fn bf_exists(&self, key: &str, item: &str) -> Result<bool, Error> {
            if let Some(items) = self.bloom_filters.get(key) {
                Ok(items.iter().any(|i| i == item))
            } else {
                Ok(false)
            }
        }
    }

    // Mock implementation of ReadStats
    #[derive(Default)]
    struct MockReadStats {
        kv_reads: AtomicI32,
        byod_reads: AtomicI32,
    }

    impl MockReadStats {
        fn new() -> Self {
            Self::default()
        }

        fn get_kv_reads(&self) -> i32 {
            self.kv_reads.load(std::sync::atomic::Ordering::Relaxed)
        }

        fn get_byod_reads(&self) -> i32 {
            self.byod_reads.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl ReadStats for MockReadStats {
        fn count_kv_read(&self, value: i32) {
            self.kv_reads
                .fetch_add(value, std::sync::atomic::Ordering::Relaxed);
        }

        fn count_kv_byod_read(&self, value: i32) {
            self.byod_reads
                .fetch_add(value, std::sync::atomic::Ordering::Relaxed);
        }
    }

    // Mock implementation of StoreManager
    struct MockStoreManager {
        stores: HashMap<String, Arc<dyn Store>>,
    }

    impl MockStoreManager {
        fn new() -> Self {
            Self {
                stores: HashMap::new(),
            }
        }

        fn with_store(mut self, param: &str, store: Arc<dyn Store>) -> Self {
            self.stores.insert(param.to_string(), store);
            self
        }
    }

    #[async_trait::async_trait]
    impl StoreManager for MockStoreManager {
        async fn get_store(
            &self,
            param: &str,
            _metric: Arc<dyn ReadStats>,
        ) -> Result<Arc<dyn Store>, Error> {
            self.stores.get(param).cloned().ok_or(Error::NoSuchStore)
        }
    }

    #[tokio::test]
    async fn test_builder_default() {
        let builder = Builder::default();
        assert_eq!(builder.allowed_stores.len(), 0);
    }

    #[tokio::test]
    async fn test_builder_new() {
        let allowed_stores = vec![(
            SmolStr::new("store1"),
            SmolStr::new("redis://localhost:6379"),
        )];
        let manager = Arc::new(NoSuchStoreManager);
        let builder = Builder::new(allowed_stores.clone(), manager);

        assert_eq!(builder.allowed_stores.len(), 1);
        assert_eq!(builder.allowed_stores[0].0, "store1");
    }

    #[tokio::test]
    async fn test_store_impl_open_allowed_store() {
        let mock_store = Arc::new(MockStore::new());
        let manager = Arc::new(
            MockStoreManager::new().with_store("redis://localhost:6379", mock_store.clone()),
        );

        let allowed_stores = vec![(
            SmolStr::new("mystore"),
            SmolStr::new("redis://localhost:6379"),
        )];
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(allowed_stores, manager).build(stats.clone());

        let result = store_impl.open("mystore").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_store_impl_open_denied_store() {
        let manager = Arc::new(NoSuchStoreManager);
        let allowed_stores = vec![(
            SmolStr::new("allowed"),
            SmolStr::new("redis://localhost:6379"),
        )];
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(allowed_stores, manager).build(stats.clone());

        let result = store_impl.open("notallowed").await;
        assert!(matches!(result, Err(Error::AccessDenied)));
    }

    #[tokio::test]
    async fn test_store_impl_open_multiple_stores() {
        let mock_store1 = Arc::new(MockStore::new());
        let mock_store2 = Arc::new(MockStore::new());
        let manager = Arc::new(
            MockStoreManager::new()
                .with_store("redis://store1", mock_store1.clone())
                .with_store("redis://store2", mock_store2.clone()),
        );

        let allowed_stores = vec![
            (SmolStr::new("store1"), SmolStr::new("redis://store1")),
            (SmolStr::new("store2"), SmolStr::new("redis://store2")),
        ];
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(allowed_stores, manager).build(stats.clone());

        let store1_id = store_impl.open("store1").await.unwrap();
        let store2_id = store_impl.open("store2").await.unwrap();

        assert_eq!(store1_id, 0);
        assert_eq!(store2_id, 1);
    }

    #[tokio::test]
    async fn test_get_value_success() {
        let mock_store = Arc::new(MockStore::new().with_data("key1", b"value1".to_vec()));
        let manager = Arc::new(MockStoreManager::new().with_store("redis://localhost", mock_store));

        let allowed_stores = vec![(SmolStr::new("mystore"), SmolStr::new("redis://localhost"))];
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(allowed_stores, manager).build(stats.clone());

        let store_id = store_impl.open("mystore").await.unwrap();
        let result = store_impl.get(store_id, "key1").await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(b"value1".to_vec()));
    }

    #[tokio::test]
    async fn test_get_value_not_found() {
        let mock_store = Arc::new(MockStore::new());
        let manager = Arc::new(MockStoreManager::new().with_store("redis://localhost", mock_store));

        let allowed_stores = vec![(SmolStr::new("mystore"), SmolStr::new("redis://localhost"))];
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(allowed_stores, manager).build(stats.clone());

        let store_id = store_impl.open("mystore").await.unwrap();
        let result = store_impl.get(store_id, "nonexistent").await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn test_get_invalid_store_id() {
        let manager = Arc::new(NoSuchStoreManager);
        let stats = Arc::new(MockReadStats::new());
        let store_impl = Builder::new(vec![], manager).build(stats.clone());

        let result = store_impl.get(999, "key1").await;
        assert!(matches!(result, Err(Error::NoSuchStore)));
    }

    #[tokio::test]
    async fn test_zrange_by_score() {
        let items = vec![
            (b"item1".to_vec(), 1.0),
            (b"item2".to_vec(), 2.5),
            (b"item3".to_vec(), 5.0),
            (b"item4".to_vec(), 7.5),
        ];
        let mock_store = Arc::new(MockStore::new().with_zset("sorted_set", items));
        let manager = Arc::new(MockStoreManager::new().with_store("redis://localhost", mock_store));

        let allowed_stores = vec![(SmolStr::new("mystore"), SmolStr::new("redis://localhost"))];
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(allowed_stores, manager).build(stats.clone());

        let store_id = store_impl.open("mystore").await.unwrap();
        let result = store_impl
            .zrange_by_score(store_id, "sorted_set", 2.0, 6.0)
            .await;

        assert!(result.is_ok());
        let items = result.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].1, 2.5);
        assert_eq!(items[1].1, 5.0);
    }

    #[tokio::test]
    async fn test_zrange_by_score_invalid_store() {
        let manager = Arc::new(NoSuchStoreManager);
        let stats = Arc::new(MockReadStats::new());
        let store_impl = Builder::new(vec![], manager).build(stats.clone());

        let result = store_impl.zrange_by_score(999, "key", 0.0, 10.0).await;
        assert!(matches!(result, Err(Error::NoSuchStore)));
    }

    #[tokio::test]
    async fn test_scan_pattern() {
        let mock_store = Arc::new(
            MockStore::new()
                .with_data("user:1", b"alice".to_vec())
                .with_data("user:2", b"bob".to_vec())
                .with_data("post:1", b"hello".to_vec()),
        );
        let manager = Arc::new(MockStoreManager::new().with_store("redis://localhost", mock_store));

        let allowed_stores = vec![(SmolStr::new("mystore"), SmolStr::new("redis://localhost"))];
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(allowed_stores, manager).build(stats.clone());

        let store_id = store_impl.open("mystore").await.unwrap();
        let result = store_impl.scan(store_id, "user:").await;

        assert!(result.is_ok());
        let keys = result.unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"user:1".to_string()));
        assert!(keys.contains(&"user:2".to_string()));
    }

    #[tokio::test]
    async fn test_scan_invalid_store() {
        let manager = Arc::new(NoSuchStoreManager);
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(vec![], manager).build(stats.clone());

        let result = store_impl.scan(999, "pattern").await;
        assert!(matches!(result, Err(Error::NoSuchStore)));
    }

    #[tokio::test]
    async fn test_zscan() {
        let items = vec![
            (b"apple".to_vec(), 1.0),
            (b"apricot".to_vec(), 2.0),
            (b"banana".to_vec(), 3.0),
        ];
        let mock_store = Arc::new(MockStore::new().with_zset("fruits", items));
        let manager = Arc::new(MockStoreManager::new().with_store("redis://localhost", mock_store));

        let allowed_stores = vec![(SmolStr::new("mystore"), SmolStr::new("redis://localhost"))];
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(allowed_stores, manager).build(stats.clone());

        let store_id = store_impl.open("mystore").await.unwrap();
        let result = store_impl.zscan(store_id, "fruits", "ap").await;

        assert!(result.is_ok());
        let items = result.unwrap();
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn test_zscan_invalid_store() {
        let manager = Arc::new(NoSuchStoreManager);
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(vec![], manager).build(stats.clone());

        let result = store_impl.zscan(999, "key", "pattern").await;
        assert!(matches!(result, Err(Error::NoSuchStore)));
    }

    #[tokio::test]
    async fn test_bf_exists_true() {
        let mock_store = Arc::new(
            MockStore::new()
                .with_bloom("bloom_key", vec!["item1".to_string(), "item2".to_string()]),
        );
        let manager = Arc::new(MockStoreManager::new().with_store("redis://localhost", mock_store));

        let allowed_stores = vec![(SmolStr::new("mystore"), SmolStr::new("redis://localhost"))];
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(allowed_stores, manager).build(stats.clone());

        let store_id = store_impl.open("mystore").await.unwrap();
        let result = store_impl.bf_exists(store_id, "bloom_key", "item1").await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_bf_exists_false() {
        let mock_store =
            Arc::new(MockStore::new().with_bloom("bloom_key", vec!["item1".to_string()]));
        let manager = Arc::new(MockStoreManager::new().with_store("redis://localhost", mock_store));

        let allowed_stores = vec![(SmolStr::new("mystore"), SmolStr::new("redis://localhost"))];
        let stats = Arc::new(MockReadStats::new());
        let mut store_impl = Builder::new(allowed_stores, manager).build(stats.clone());

        let store_id = store_impl.open("mystore").await.unwrap();
        let result = store_impl
            .bf_exists(store_id, "bloom_key", "nonexistent")
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_bf_exists_invalid_store() {
        let manager = Arc::new(NoSuchStoreManager);
        let stats = Arc::new(MockReadStats::new());
        let store_impl = Builder::new(vec![], manager).build(stats.clone());

        let result = store_impl.bf_exists(999, "key", "item").await;
        assert!(matches!(result, Err(Error::NoSuchStore)));
    }

    #[tokio::test]
    async fn test_no_such_store_manager() {
        let manager = NoSuchStoreManager;
        let stats = Arc::new(MockReadStats::new());

        let result = manager.get_store("any", stats.clone()).await;
        assert!(matches!(result, Err(Error::NoSuchStore)));
    }

    #[tokio::test]
    async fn test_read_stats() {
        let stats = MockReadStats::new();

        stats.count_kv_read(10);
        stats.count_kv_read(5);
        assert_eq!(stats.get_kv_reads(), 15);

        stats.count_kv_byod_read(3);
        stats.count_kv_byod_read(7);
        assert_eq!(stats.get_byod_reads(), 10);
    }
}
