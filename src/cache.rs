use cache::{CacheBackend, Error, Payload};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().expect("cache mutex poisoned")
    }
}

#[async_trait::async_trait]
impl CacheBackend for MemoryCacheBackend {
    async fn get(&self, key: &str) -> Result<Option<Payload>, Error> {
        let now = Instant::now();
        let mut entries = self.lock();
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
        self.lock()
            .insert(key.to_string(), Entry { value, expires_at });
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.lock().remove(key);
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, Error> {
        let now = Instant::now();
        let mut entries = self.lock();
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
        let mut entries = self.lock();
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
        let mut entries = self.lock();
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
}
