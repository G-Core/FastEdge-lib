use crate::stores_service::{ServiceError, StoresService, unb64};
use key_value_store::{Error, Store, StoreManager, Value};
use key_value_store::{ReadStats, RedisStore};
use runtime::app::KvStoreOption;
use serde_json::json;
use std::sync::Arc;

pub(crate) struct CliStoreManager {
    pub(crate) stores: Vec<KvStoreOption>,
    /// When set, stores whose param is not a `redis://` URL are served by the storage service.
    pub(crate) service: Option<Arc<StoresService>>,
}

fn is_redis(param: &str) -> bool {
    param.starts_with("redis://") || param.starts_with("rediss://")
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
        match &self.service {
            Some(service) if !is_redis(param) => Ok(Arc::new(ServiceStore {
                service: service.clone(),
                store: opts.param.to_string(),
            })),
            _ => Ok(Arc::new(RedisStore::open(&opts.param, 1).await?)),
        }
    }
}

/// Edge KV store served by the local storage service (see `stores_service`). `store` is the
/// param from `--kv-stores <name>=<param>`: the service's identity for the store.
struct ServiceStore {
    service: Arc<StoresService>,
    store: String,
}

impl ServiceStore {
    async fn call(&self, mut body: serde_json::Value) -> Result<serde_json::Value, Error> {
        body["store"] = self.store.clone().into();
        self.service.call("kv", body).await.map_err(kv_error)
    }
}

fn kv_error(error: ServiceError) -> Error {
    match error.as_deref() {
        Some("no-such-store") => Error::NoSuchStore,
        Some("access-denied") => Error::AccessDenied,
        _ => Error::InternalError,
    }
}

/// `[[<base64 value>, <score>], ..]`
fn scored(value: serde_json::Value) -> Result<Vec<(Value, f64)>, Error> {
    let items = value.as_array().ok_or(Error::InternalError)?;
    items
        .iter()
        .map(|item| Some((unb64(&item[0])?, item[1].as_f64()?)))
        .collect::<Option<_>>()
        .ok_or(Error::InternalError)
}

#[async_trait::async_trait]
impl Store for ServiceStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, Error> {
        let value = self.call(json!({"op": "get", "key": key})).await?;
        if value.is_null() {
            return Ok(None);
        }
        unb64(&value).map(Some).ok_or(Error::InternalError)
    }

    async fn zrange_by_score(
        &self,
        key: &str,
        min: f64,
        max: f64,
    ) -> Result<Vec<(Value, f64)>, Error> {
        // f64 bounds may be +-inf, which JSON numbers cannot carry.
        scored(
            self.call(json!({"op": "zrange-by-score", "key": key, "min": min.to_string(), "max": max.to_string()}))
                .await?,
        )
    }

    async fn scan(&self, pattern: &str) -> Result<Vec<String>, Error> {
        let value = self.call(json!({"op": "scan", "pattern": pattern})).await?;
        serde_json::from_value(value).map_err(|_| Error::InternalError)
    }

    async fn zscan(&self, key: &str, pattern: &str) -> Result<Vec<(Value, f64)>, Error> {
        scored(
            self.call(json!({"op": "zscan", "key": key, "pattern": pattern}))
                .await?,
        )
    }

    async fn bf_exists(&self, key: &str, item: &str) -> Result<bool, Error> {
        let value = self
            .call(json!({"op": "bf-exists", "key": key, "item": item}))
            .await?;
        value.as_bool().ok_or(Error::InternalError)
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
        fn count_kv_read_cached(&self) {}
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
        let manager = CliStoreManager {
            stores: vec![],
            service: None,
        };
        let result = manager
            .get_store("redis://localhost:6379", Arc::new(NoOpStats))
            .await;
        assert!(matches!(result, Err(Error::NoSuchStore)));
    }

    #[tokio::test]
    async fn get_store_returns_no_such_store_when_param_unmatched() {
        let manager = CliStoreManager {
            stores: vec![make_kv_option("mystore", "redis://localhost:6379")],
            service: None,
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
            service: None,
        };
        let result = manager.get_store("mystore", Arc::new(NoOpStats)).await;
        assert!(matches!(result, Err(Error::NoSuchStore)));
    }

    #[tokio::test]
    async fn get_store_forwards_non_redis_params_to_the_service() {
        let service = crate::stores_service::StoresService::parse("http://127.0.0.1:1,t").unwrap();
        let manager = CliStoreManager {
            stores: vec![make_kv_option("mystore", "mystore")],
            service: Some(service),
        };
        // No network call on open: a ServiceStore is returned as is.
        assert!(
            manager
                .get_store("mystore", Arc::new(NoOpStats))
                .await
                .is_ok()
        );
        let without = CliStoreManager {
            stores: vec![make_kv_option("mystore", "mystore")],
            service: None,
        };
        assert!(
            without
                .get_store("mystore", Arc::new(NoOpStats))
                .await
                .is_err()
        );
    }

    #[test]
    fn scored_and_errors() {
        let ok = scored(serde_json::json!([["YQ==", 1.5], ["Yg==", 2.0]])).unwrap();
        assert_eq!(ok, vec![(b"a".to_vec(), 1.5), (b"b".to_vec(), 2.0)]);
        assert!(scored(serde_json::json!([["not base64!", 1]])).is_err());
        assert!(matches!(
            kv_error(Some("no-such-store".into())),
            Error::NoSuchStore
        ));
        assert!(matches!(kv_error(None), Error::InternalError));
    }
}
