use crate::Store;
use reactor::gcore::fastedge::key_value::{Error, Value};
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, AsyncIter};

#[derive(Clone)]
pub struct RedisStore {
    inner: ConnectionManager,
}

impl RedisStore {
    /// Open a store backed by `ConnectionManager`, which holds a multiplexed
    /// connection and transparently reconnects with exponential backoff when
    /// the underlying socket dies (e.g. broken pipe on Redis restart). The
    /// command that hits the dead socket still surfaces as an error, but
    /// follow-up calls land on the freshly re-established connection.
    pub async fn open(params: &str) -> Result<Self, Error> {
        let client = ::redis::Client::open(params).map_err(|error| {
            tracing::warn!(error = ?error, "kv-store: redis open");
            Error::InternalError
        })?;
        let conn = ConnectionManager::new(client).await.map_err(|error| {
            tracing::warn!(error = ?error, "kv-store: redis open");
            Error::InternalError
        })?;
        Ok(Self { inner: conn })
    }
}

#[async_trait::async_trait]
impl Store for RedisStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, Error> {
        self.inner.clone().get(key).await.map_err(|error| {
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
        self.inner
            .clone()
            .zrangebyscore_withscores(key, min, max)
            .await
            .map_err(|error| {
                tracing::warn!(cause = ?error, key, min, max, "kv-store: redis zrangebyscore");
                Error::InternalError
            })
    }

    async fn scan(&self, pattern: &str) -> Result<Vec<String>, Error> {
        let mut conn = self.inner.clone();
        let mut it = conn.scan_match(pattern).await.map_err(|error| {
            tracing::warn!(cause = ?error, pattern, "kv-store: redis scan_match");
            Error::InternalError
        })?;
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
        let mut conn = self.inner.clone();
        let mut it: AsyncIter<(Value, f64)> =
            conn.zscan_match(key, pattern).await.map_err(|error| {
                tracing::warn!(cause = ?error, key, pattern, "kv-store: redis zscan_match");
                Error::InternalError
            })?;
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
        redis::cmd("BF.EXISTS")
            .arg(key)
            .arg(item)
            .query_async(&mut self.inner.clone())
            .await
            .map_err(|error| {
                tracing::warn!(cause = ?error, key, item, "kv-store: redis bf_exists");
                Error::InternalError
            })
    }
}
