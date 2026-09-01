use crate::Store;
use reactor::gcore::fastedge::key_value::{Error, Value};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{AsyncCommands, AsyncIter};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Fail-fast timeouts for the KV-store Redis connection. Redis sits on the
/// request hot path, so a slow/unreachable Redis must surface as a quick error
/// rather than parking the calling task: a stalled dependency with no bound
/// lets in-flight requests pile up until the node collapses. Set explicitly so
/// a redis-crate bump can't silently change them.
const REDIS_RESPONSE_TIMEOUT: Duration = Duration::from_millis(100);
const REDIS_CONNECTION_TIMEOUT: Duration = Duration::from_millis(100);
const REDIS_NUMBER_OF_RETRIES: usize = 2;
const REDIS_MAX_RECONNECT_DELAY: Duration = Duration::from_millis(100);

/// Retry budget for a single read command (on top of connection-manager
/// reconnection above). Kept small and fixed for the same reason as the
/// timeouts: Redis is on the request hot path, so a retry loop must have a
/// bounded worst case rather than let a stalled dependency pile up requests.
const REDIS_READ_RETRIES: usize = 2;
const REDIS_READ_RETRY_BASE_DELAY: Duration = Duration::from_millis(10);
const REDIS_READ_RETRY_MAX_DELAY: Duration = Duration::from_millis(50);

fn redis_read_retry_delay(attempt: usize) -> Duration {
    let scale = 1u32 << (attempt.saturating_sub(1) as u32);
    (REDIS_READ_RETRY_BASE_DELAY * scale).min(REDIS_READ_RETRY_MAX_DELAY)
}

/// Retry a read command up to `REDIS_READ_RETRIES` times with capped
/// exponential backoff. `op` is re-invoked from scratch on each attempt so it
/// can pick a fresh pooled connection (used by `get`/`zrange_by_score`/
/// `bf_exists`; `scan`/`zscan` retry the same connection since their iterator
/// borrows it across the loop).
async fn retry_read<T, F, Fut>(mut op: F) -> Result<T, ::redis::RedisError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ::redis::RedisError>>,
{
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < REDIS_READ_RETRIES => {
                attempt += 1;
                tracing::debug!(attempt, cause = ?error, "kv-store: redis read retry");
                tokio::time::sleep(redis_read_retry_delay(attempt)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Build the fail-fast connection-manager config for KV-store Redis connections.
fn connection_manager_config() -> ConnectionManagerConfig {
    ConnectionManagerConfig::new()
        .set_response_timeout(Some(REDIS_RESPONSE_TIMEOUT))
        .set_connection_timeout(Some(REDIS_CONNECTION_TIMEOUT))
        .set_number_of_retries(REDIS_NUMBER_OF_RETRIES)
        .set_max_delay(REDIS_MAX_RECONNECT_DELAY)
}

#[derive(Clone)]
pub struct RedisStore {
    /// Pool of multiplexed connections. Each `ConnectionManager` owns its own
    /// socket and background driver task, so spreading commands across the pool
    /// round-robin keeps a burst from serializing behind a single connection
    /// (which would push tail latency past the response timeout). Wrapped in
    /// `Arc` so cloning a `RedisStore` shares the same pool and cursor.
    conns: Arc<Vec<ConnectionManager>>,
    next: Arc<AtomicUsize>,
}

impl RedisStore {
    /// Open a store backed by a pool of `ConnectionManager`s. Each connection
    /// holds a multiplexed connection and transparently reconnects with
    /// exponential backoff when the underlying socket dies (e.g. broken pipe on
    /// Redis restart). The command that hits the dead socket still surfaces as
    /// an error, but follow-up calls land on a freshly re-established
    /// connection. `pool_size` is clamped to at least 1.
    pub async fn open(params: &str, pool_size: usize) -> Result<Self, Error> {
        let pool_size = pool_size.max(1);
        let client = ::redis::Client::open(params).map_err(|error| {
            tracing::warn!(error = ?error, "kv-store: redis open");
            Error::InternalError
        })?;
        let mut conns = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let conn =
                ConnectionManager::new_with_config(client.clone(), connection_manager_config())
                    .await
                    .map_err(|error| {
                        tracing::warn!(error = ?error, "kv-store: redis open");
                        Error::InternalError
                    })?;
            conns.push(conn);
        }
        Ok(Self {
            conns: Arc::new(conns),
            next: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Pick the next connection from the pool (round-robin). Clones are cheap:
    /// `ConnectionManager` is internally reference-counted and shares its
    /// socket, so this just hands back another handle to a pooled connection.
    fn conn(&self) -> ConnectionManager {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.conns.len();
        self.conns[idx].clone()
    }
}

#[async_trait::async_trait]
impl Store for RedisStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, Error> {
        retry_read(|| async { self.conn().get(key).await })
            .await
            .map_err(|error| {
                tracing::warn!(cause = ?error, key, "kv-store: redis get");
                Error::InternalError
            })
    }

    async fn zrange_by_score(
        &self,
        key: &str,
        min: f64,
        max: f64,
    ) -> Result<Vec<(Value, f64)>, Error> {
        retry_read(|| async { self.conn().zrangebyscore_withscores(key, min, max).await })
            .await
            .map_err(|error| {
                tracing::warn!(cause = ?error, key, min, max, "kv-store: redis zrangebyscore");
                Error::InternalError
            })
    }

    async fn scan(&self, pattern: &str) -> Result<Vec<String>, Error> {
        let mut conn = self.conn();
        let mut attempt = 0;
        let mut it = loop {
            match conn.scan_match(pattern).await {
                Ok(it) => break it,
                Err(error) if attempt < REDIS_READ_RETRIES => {
                    attempt += 1;
                    tracing::debug!(attempt, cause = ?error, pattern, "kv-store: redis scan_match retry");
                    tokio::time::sleep(redis_read_retry_delay(attempt)).await;
                }
                Err(error) => {
                    tracing::warn!(cause = ?error, pattern, "kv-store: redis scan_match");
                    return Err(Error::InternalError);
                }
            }
        };
        let mut ret = vec![];
        while let Some(element) = it.next_item().await {
            ret.push(element.map_err(|error| {
                tracing::warn!(cause = ?error, pattern, "kv-store: redis scan_match: item");
                Error::Other(error.to_string())
            })?);
        }
        Ok(ret)
    }

    async fn zscan(&self, key: &str, pattern: &str) -> Result<Vec<(Value, f64)>, Error> {
        let mut conn = self.conn();
        let mut attempt = 0;
        let mut it: AsyncIter<(Value, f64)> = loop {
            match conn.zscan_match(key, pattern).await {
                Ok(it) => break it,
                Err(error) if attempt < REDIS_READ_RETRIES => {
                    attempt += 1;
                    tracing::debug!(attempt, cause = ?error, key, pattern, "kv-store: redis zscan_match retry");
                    tokio::time::sleep(redis_read_retry_delay(attempt)).await;
                }
                Err(error) => {
                    tracing::warn!(cause = ?error, key, pattern, "kv-store: redis zscan_match");
                    return Err(Error::InternalError);
                }
            }
        };
        let mut ret = vec![];
        while let Some(element) = it.next_item().await {
            ret.push(element.map_err(|error| {
                tracing::warn!(cause = ?error, key, pattern, "kv-store: redis zscan_match: item");
                Error::Other(error.to_string())
            })?);
        }
        Ok(ret)
    }

    async fn bf_exists(&self, key: &str, item: &str) -> Result<bool, Error> {
        retry_read(|| async {
            redis::cmd("BF.EXISTS")
                .arg(key)
                .arg(item)
                .query_async(&mut self.conn())
                .await
        })
        .await
        .map_err(|error| {
            tracing::warn!(cause = ?error, key, item, "kv-store: redis bf_exists");
            Error::InternalError
        })
    }
}
