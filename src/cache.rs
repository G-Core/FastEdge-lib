use crate::stores_service::{ServiceError, StoresService, b64, unb64};
use cache::{CacheBackend, Error, Payload};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Default)]
pub struct MemoryCacheBackend {
    entries: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    value: Payload,
    expires_at: Option<Instant>,
}

impl Entry {
    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }
}

impl MemoryCacheBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl CacheBackend for MemoryCacheBackend {
    async fn get(&self, key: &str) -> Result<Option<Payload>, Error> {
        let now = Instant::now();
        let mut entries = self.entries.lock().await;
        match entries.get(key) {
            Some(entry) if entry.is_expired(now) => {
                entries.remove(key);
                Ok(None)
            }
            Some(entry) => Ok(Some(entry.value.clone())),
            None => Ok(None),
        }
    }

    async fn set(&self, key: &str, value: Payload, ttl_ms: Option<u64>) -> Result<(), Error> {
        let expires_at = ttl_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        self.entries
            .lock()
            .await
            .insert(key.to_string(), Entry { value, expires_at });
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.entries.lock().await.remove(key);
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, Error> {
        let now = Instant::now();
        let mut entries = self.entries.lock().await;
        match entries.get(key) {
            Some(entry) if entry.is_expired(now) => {
                entries.remove(key);
                Ok(false)
            }
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }

    async fn incr(&self, key: &str, delta: i64) -> Result<i64, Error> {
        let now = Instant::now();
        let mut entries = self.entries.lock().await;
        let (current, expires_at) = match entries.get(key) {
            Some(entry) if entry.is_expired(now) => (0, None),
            Some(entry) => {
                let s = std::str::from_utf8(&entry.value)
                    .map_err(|_| Error::Other("value is not valid UTF-8".to_string()))?;
                let n: i64 = s
                    .parse()
                    .map_err(|_| Error::Other("value is not an integer".to_string()))?;
                (n, entry.expires_at)
            }
            None => (0, None),
        };
        let new_value = current.wrapping_add(delta);
        entries.insert(
            key.to_string(),
            Entry {
                value: new_value.to_string().into_bytes(),
                expires_at,
            },
        );
        Ok(new_value)
    }

    async fn expire(&self, key: &str, ttl_ms: u64) -> Result<bool, Error> {
        let now = Instant::now();
        let mut entries = self.entries.lock().await;
        let expired = match entries.get(key) {
            Some(entry) => entry.is_expired(now),
            None => return Ok(false),
        };
        if expired {
            entries.remove(key);
            Ok(false)
        } else {
            entries.get_mut(key).unwrap().expires_at = Some(now + Duration::from_millis(ttl_ms));
            Ok(true)
        }
    }

    async fn purge(&self) -> Result<u64, Error> {
        let mut entries = self.entries.lock().await;
        let ret = entries.len() as u64;
        entries.clear();
        Ok(ret)
    }

    async fn purge_prefix(&self, prefix: &str) -> Result<u64, Error> {
        let mut entries = self.entries.lock().await;
        let ret = entries.len() as u64;
        entries.retain(|k, _| !k.starts_with(prefix));
        Ok(ret - entries.len() as u64)
    }
}

/// Forwards every cache call to the local storage service (see `stores_service`), which owns
/// all semantics: TTL rules, `incr`, purge scope and the clock. Keys are scoped by `dc`.
pub struct ServiceCacheBackend {
    service: Arc<StoresService>,
}

impl ServiceCacheBackend {
    pub fn new(service: Arc<StoresService>) -> Self {
        Self { service }
    }

    async fn call(&self, mut body: Value) -> Result<Value, Error> {
        body["dc"] = self.service.dc.clone().into();
        self.service.call("cache", body).await.map_err(cache_error)
    }
}

fn cache_error(error: ServiceError) -> Error {
    match error.as_deref() {
        // The edge returns BadArgument here; the cache WIT has no such variant.
        Some("bad-argument") | Some("access-denied") => Error::AccessDenied,
        _ => Error::InternalError,
    }
}

fn int(value: &Value) -> Result<i64, Error> {
    // Sent as a string by the service: JSON numbers lose precision past 2^53.
    value
        .as_i64()
        .or_else(|| value.as_str()?.parse().ok())
        .ok_or(Error::InternalError)
}

#[async_trait::async_trait]
impl CacheBackend for ServiceCacheBackend {
    async fn get(&self, key: &str) -> Result<Option<Payload>, Error> {
        let value = self.call(json!({"op": "get", "key": key})).await?;
        if value.is_null() {
            return Ok(None);
        }
        unb64(&value).map(Some).ok_or(Error::InternalError)
    }

    async fn set(&self, key: &str, value: Payload, ttl_ms: Option<u64>) -> Result<(), Error> {
        let ttl = ttl_ms.map(|t| t.to_string());
        self.call(json!({"op": "set", "key": key, "value": b64(&value), "ttlMs": ttl}))
            .await
            .map(drop)
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.call(json!({"op": "delete", "key": key}))
            .await
            .map(drop)
    }

    async fn exists(&self, key: &str) -> Result<bool, Error> {
        let value = self.call(json!({"op": "exists", "key": key})).await?;
        value.as_bool().ok_or(Error::InternalError)
    }

    async fn incr(&self, key: &str, delta: i64) -> Result<i64, Error> {
        int(&self
            .call(json!({"op": "incr", "key": key, "delta": delta.to_string()}))
            .await?)
    }

    async fn expire(&self, key: &str, ttl_ms: u64) -> Result<bool, Error> {
        let value = self
            .call(json!({"op": "expire", "key": key, "ttlMs": ttl_ms.to_string()}))
            .await?;
        value.as_bool().ok_or(Error::InternalError)
    }

    async fn purge(&self) -> Result<u64, Error> {
        let count = int(&self.call(json!({"op": "purge"})).await?)?;
        u64::try_from(count).map_err(|_| Error::InternalError)
    }

    async fn purge_prefix(&self, prefix: &str) -> Result<u64, Error> {
        let count = int(&self
            .call(json!({"op": "purge-prefix", "prefix": prefix}))
            .await?)?;
        u64::try_from(count).map_err(|_| Error::InternalError)
    }
}
