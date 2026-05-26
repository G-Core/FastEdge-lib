use reactor::gcore::fastedge::{cache_sync, cache_types};
use std::sync::Arc;

pub use cache_types::{Error, Payload};

/// Backend trait for cache implementations
#[async_trait::async_trait]
pub trait CacheBackend: Sync + Send {
    /// Get the value associated with `key`.
    async fn get(&self, key: &str) -> Result<Option<Payload>, Error>;

    /// Set the value for `key` with an optional expiry.
    async fn set(&self, key: &str, value: Payload, ttl_ms: Option<u64>) -> Result<(), Error>;

    /// Delete the key-value pair associated with `key`.
    async fn delete(&self, key: &str) -> Result<(), Error>;

    /// Check whether `key` exists in the cache.
    async fn exists(&self, key: &str) -> Result<bool, Error>;

    /// Increment the integer value stored at `key` by `delta`.
    async fn incr(&self, key: &str, delta: i64) -> Result<i64, Error>;

    /// Set or update the expiry of `key` to `ttl-ms` milliseconds from now.
    async fn expire(&self, key: &str, ttl_ms: u64) -> Result<bool, Error>;

    async fn purge(&self) -> Result<u64, Error>;

    async fn purge_prefix(&self, prefix: String) -> Result<u64, Error>;
}

/// Implementation of cache host functions
pub struct CacheImpl {
    backend: Arc<dyn CacheBackend>,
}

impl CacheImpl {
    /// Create a new cache implementation with the given backend
    pub fn new(backend: Arc<dyn CacheBackend>) -> Self {
        Self { backend }
    }
}

// Empty marker trait
impl cache_types::Host for CacheImpl {}

// Implement the sync-style cache interface (gcore:fastedge/cache-sync)
// This is simpler and serializes calls through &mut self.
impl cache_sync::Host for CacheImpl {
    async fn get(&mut self, key: String) -> Result<Option<Payload>, Error> {
        self.backend.get(&key).await
    }

    async fn set(&mut self, key: String, value: Payload, ttl_ms: Option<u64>) -> Result<(), Error> {
        self.backend.set(&key, value, ttl_ms).await
    }

    async fn delete(&mut self, key: String) -> Result<(), Error> {
        self.backend.delete(&key).await
    }

    async fn exists(&mut self, key: String) -> Result<bool, Error> {
        self.backend.exists(&key).await
    }

    async fn incr(&mut self, key: String, delta: i64) -> Result<i64, Error> {
        self.backend.incr(&key, delta).await
    }

    async fn expire(&mut self, key: String, ttl_ms: u64) -> Result<bool, Error> {
        self.backend.expire(&key, ttl_ms).await
    }
    async fn purge(&mut self) -> Result<u64, Error> {
        self.backend.purge().await
    }
    async fn purge_prefix(&mut self, prefix: String) -> Result<u64, Error> {
        self.backend.purge_prefix(prefix).await
    }
}

/// No-op cache backend that returns access denied for all operations
pub struct NoCacheBackend;

#[async_trait::async_trait]
impl CacheBackend for NoCacheBackend {
    async fn get(&self, _key: &str) -> Result<Option<Payload>, Error> {
        Err(Error::AccessDenied)
    }

    async fn set(&self, _key: &str, _value: Payload, _ttl_ms: Option<u64>) -> Result<(), Error> {
        Err(Error::AccessDenied)
    }

    async fn delete(&self, _key: &str) -> Result<(), Error> {
        Err(Error::AccessDenied)
    }

    async fn exists(&self, _key: &str) -> Result<bool, Error> {
        Err(Error::AccessDenied)
    }

    async fn incr(&self, _key: &str, _delta: i64) -> Result<i64, Error> {
        Err(Error::AccessDenied)
    }

    async fn expire(&self, _key: &str, _ttl_ms: u64) -> Result<bool, Error> {
        Err(Error::AccessDenied)
    }

    async fn purge(&self) -> Result<u64, Error> {
        Err(Error::AccessDenied)
    }

    async fn purge_prefix(&self, _prefix: String) -> Result<u64, Error> {
        Err(Error::AccessDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Compile-only test: verify `cache-sync` linker registration accepts `CacheImpl`.
    #[allow(dead_code)]
    fn _verify_linker_compat<T: 'static + Send>(
        linker: &mut wasmtime::component::Linker<T>,
        getter: fn(&mut T) -> &mut CacheImpl,
    ) -> wasmtime::Result<()> {
        cache_sync::add_to_linker::<_, wasmtime::component::HasSelf<CacheImpl>>(linker, getter)?;
        Ok(())
    }

    /// Mock cache backend for testing
    struct MockCacheBackend {
        data: Mutex<HashMap<String, (Payload, Option<u64>)>>,
    }

    impl MockCacheBackend {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }

        fn with_data(self, key: &str, value: Payload) -> Self {
            self.data
                .lock()
                .unwrap()
                .insert(key.to_string(), (value, None));
            self
        }
    }

    #[async_trait::async_trait]
    impl CacheBackend for MockCacheBackend {
        async fn get(&self, key: &str) -> Result<Option<Payload>, Error> {
            Ok(self.data.lock().unwrap().get(key).map(|(v, _)| v.clone()))
        }

        async fn set(&self, key: &str, value: Payload, ttl_ms: Option<u64>) -> Result<(), Error> {
            self.data
                .lock()
                .unwrap()
                .insert(key.to_string(), (value, ttl_ms));
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), Error> {
            self.data.lock().unwrap().remove(key);
            Ok(())
        }

        async fn exists(&self, key: &str) -> Result<bool, Error> {
            Ok(self.data.lock().unwrap().contains_key(key))
        }

        async fn incr(&self, key: &str, delta: i64) -> Result<i64, Error> {
            let mut data = self.data.lock().unwrap();
            let current = if let Some((val, ttl)) = data.get(key) {
                let s = String::from_utf8(val.clone())
                    .map_err(|_| Error::Other("Invalid UTF-8".to_string()))?;
                let num: i64 = s
                    .parse()
                    .map_err(|_| Error::Other("Not a number".to_string()))?;
                (num, *ttl)
            } else {
                (0, None)
            };
            let new_value = current.0 + delta;
            data.insert(
                key.to_string(),
                (new_value.to_string().into_bytes(), current.1),
            );
            Ok(new_value)
        }

        async fn expire(&self, key: &str, ttl_ms: u64) -> Result<bool, Error> {
            let mut data = self.data.lock().unwrap();
            if let Some((val, _)) = data.get(key) {
                let val = val.clone();
                data.insert(key.to_string(), (val, Some(ttl_ms)));
                Ok(true)
            } else {
                Ok(false)
            }
        }

        async fn purge(&self) -> Result<u64, Error> {
            todo!()
        }

        async fn purge_prefix(&self, _prefix: String) -> Result<u64, Error> {
            todo!()
        }
    }

    #[tokio::test]
    async fn test_no_cache_backend() {
        let backend = Arc::new(NoCacheBackend);
        let mut cache_impl = CacheImpl::new(backend);

        let result = cache_sync::Host::get(&mut cache_impl, "test".to_string()).await;
        assert!(matches!(result, Err(Error::AccessDenied)));
    }

    #[tokio::test]
    async fn test_cache_get_success() {
        let backend = Arc::new(MockCacheBackend::new().with_data("key1", b"value1".to_vec()));
        let mut cache_impl = CacheImpl::new(backend);

        let result = cache_sync::Host::get(&mut cache_impl, "key1".to_string()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(b"value1".to_vec()));
    }

    #[tokio::test]
    async fn test_cache_get_not_found() {
        let backend = Arc::new(MockCacheBackend::new());
        let mut cache_impl = CacheImpl::new(backend);

        let result = cache_sync::Host::get(&mut cache_impl, "nonexistent".to_string()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn test_cache_set() {
        let backend = Arc::new(MockCacheBackend::new());
        let mut cache_impl = CacheImpl::new(backend);

        let result = cache_sync::Host::set(
            &mut cache_impl,
            "key1".to_string(),
            b"value1".to_vec(),
            None,
        )
        .await;
        assert!(result.is_ok());

        // Verify it was set
        let get_result = cache_sync::Host::get(&mut cache_impl, "key1".to_string()).await;
        assert_eq!(get_result.unwrap(), Some(b"value1".to_vec()));
    }

    #[tokio::test]
    async fn test_cache_set_with_ttl() {
        let backend = Arc::new(MockCacheBackend::new());
        let mut cache_impl = CacheImpl::new(backend);

        let result = cache_sync::Host::set(
            &mut cache_impl,
            "key1".to_string(),
            b"value1".to_vec(),
            Some(5000),
        )
        .await;
        assert!(result.is_ok());

        let get_result = cache_sync::Host::get(&mut cache_impl, "key1".to_string()).await;
        assert_eq!(get_result.unwrap(), Some(b"value1".to_vec()));
    }

    #[tokio::test]
    async fn test_cache_delete() {
        let backend = Arc::new(MockCacheBackend::new().with_data("key1", b"value1".to_vec()));
        let mut cache_impl = CacheImpl::new(backend);

        // Verify key exists
        let exists = cache_sync::Host::exists(&mut cache_impl, "key1".to_string())
            .await
            .unwrap();
        assert!(exists);

        // Delete it
        let result = cache_sync::Host::delete(&mut cache_impl, "key1".to_string()).await;
        assert!(result.is_ok());

        // Verify it's gone
        let exists = cache_sync::Host::exists(&mut cache_impl, "key1".to_string())
            .await
            .unwrap();
        assert!(!exists);
    }

    #[tokio::test]
    async fn test_cache_exists() {
        let backend = Arc::new(MockCacheBackend::new().with_data("key1", b"value1".to_vec()));
        let mut cache_impl = CacheImpl::new(backend);

        let result = cache_sync::Host::exists(&mut cache_impl, "key1".to_string()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);

        let result = cache_sync::Host::exists(&mut cache_impl, "nonexistent".to_string()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_cache_incr() {
        let backend = Arc::new(MockCacheBackend::new().with_data("counter", b"10".to_vec()));
        let mut cache_impl = CacheImpl::new(backend);

        let result = cache_sync::Host::incr(&mut cache_impl, "counter".to_string(), 5).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 15);
    }

    #[tokio::test]
    async fn test_cache_incr_nonexistent() {
        let backend = Arc::new(MockCacheBackend::new());
        let mut cache_impl = CacheImpl::new(backend);

        let result = cache_sync::Host::incr(&mut cache_impl, "newcounter".to_string(), 5).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 5);
    }

    #[tokio::test]
    async fn test_cache_incr_negative_delta() {
        let backend = Arc::new(MockCacheBackend::new().with_data("counter", b"10".to_vec()));
        let mut cache_impl = CacheImpl::new(backend);

        let result = cache_sync::Host::incr(&mut cache_impl, "counter".to_string(), -3).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 7);
    }

    #[tokio::test]
    async fn test_cache_expire() {
        let backend = Arc::new(MockCacheBackend::new().with_data("key1", b"value1".to_vec()));
        let mut cache_impl = CacheImpl::new(backend);

        let result = cache_sync::Host::expire(&mut cache_impl, "key1".to_string(), 5000).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn test_cache_expire_nonexistent() {
        let backend = Arc::new(MockCacheBackend::new());
        let mut cache_impl = CacheImpl::new(backend);

        let result =
            cache_sync::Host::expire(&mut cache_impl, "nonexistent".to_string(), 5000).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn test_cache_impl_sync_trait() {
        let backend = Arc::new(MockCacheBackend::new().with_data("key1", b"value1".to_vec()));
        let mut cache_impl = CacheImpl::new(backend);

        // Test using the sync trait methods
        let result = cache_sync::Host::get(&mut cache_impl, "key1".to_string()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(b"value1".to_vec()));
    }

    #[tokio::test]
    async fn test_multiple_operations() {
        let backend = Arc::new(MockCacheBackend::new());
        let mut cache_impl = CacheImpl::new(backend);

        // Set multiple values
        cache_sync::Host::set(
            &mut cache_impl,
            "key1".to_string(),
            b"value1".to_vec(),
            None,
        )
        .await
        .unwrap();
        cache_sync::Host::set(
            &mut cache_impl,
            "key2".to_string(),
            b"value2".to_vec(),
            Some(1000),
        )
        .await
        .unwrap();
        cache_sync::Host::set(&mut cache_impl, "counter".to_string(), b"0".to_vec(), None)
            .await
            .unwrap();

        // Verify all exist
        assert!(
            cache_sync::Host::exists(&mut cache_impl, "key1".to_string())
                .await
                .unwrap()
        );
        assert!(
            cache_sync::Host::exists(&mut cache_impl, "key2".to_string())
                .await
                .unwrap()
        );
        assert!(
            cache_sync::Host::exists(&mut cache_impl, "counter".to_string())
                .await
                .unwrap()
        );

        // Increment counter
        let val = cache_sync::Host::incr(&mut cache_impl, "counter".to_string(), 10)
            .await
            .unwrap();
        assert_eq!(val, 10);

        // Update expiry
        let updated = cache_sync::Host::expire(&mut cache_impl, "key1".to_string(), 2000)
            .await
            .unwrap();
        assert!(updated);

        // Delete one
        cache_sync::Host::delete(&mut cache_impl, "key2".to_string())
            .await
            .unwrap();
        assert!(
            !cache_sync::Host::exists(&mut cache_impl, "key2".to_string())
                .await
                .unwrap()
        );
    }
}
