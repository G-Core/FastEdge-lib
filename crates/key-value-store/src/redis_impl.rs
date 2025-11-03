use crate::Store;
use reactor::gcore::fastedge::key_value::{Error, Value};
use redis::{AsyncCommands, AsyncIter};

#[derive(Clone)]
pub struct RedisStore {
    inner: redis::aio::MultiplexedConnection,
}

impl RedisStore {
    pub async fn open(params: &str) -> Result<Self, Error> {
        let conn = ::redis::Client::open(params)
            .map_err(|error| {
                tracing::warn!(error=?error, "redis open");
                Error::InternalError
            })?
            .get_multiplexed_async_connection()
            .await
            .map_err(|error| {
                tracing::warn!(error=?error, "redis open");
                Error::InternalError
            })?;
        Ok(Self { inner: conn })
    }
}

#[async_trait::async_trait]
impl Store for RedisStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, Error> {
        self.inner.clone().get(key).await.map_err(|error| {
            tracing::warn!(cause=?error, "redis get");
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
                tracing::warn!(cause=?error, "redis zrangebyscore");
                Error::InternalError
            })
    }

    async fn scan(&self, pattern: &str) -> Result<Vec<String>, Error> {
        let mut conn = self.inner.clone();
        let mut it = conn.scan_match(pattern).await.map_err(|error| {
            tracing::warn!(cause=?error, "redis scan_match");
            Error::InternalError
        })?;
        let mut ret = vec![];
        while let Some(element) = it.next_item().await {
            ret.push(element);
        }
        Ok(ret)
    }

    async fn zscan(&self, key: &str, pattern: &str) -> Result<Vec<(Value, f64)>, Error> {
        let mut conn = self.inner.clone();
        let mut it: AsyncIter<(Value, f64)> =
            conn.zscan_match(key, pattern).await.map_err(|error| {
                tracing::warn!(cause=?error, "redis zscan_match");
                Error::InternalError
            })?;
        let mut ret = vec![];
        while let Some(element) = it.next_item().await {
            ret.push(element);
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
                tracing::warn!(cause=?error, "redis bf_exists");
                Error::InternalError
            })
    }
}
