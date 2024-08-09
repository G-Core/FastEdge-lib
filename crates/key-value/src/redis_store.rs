use std::sync::Arc;
use crate::{Store, StoreFactory};
use anyhow::Error;
use async_trait::async_trait;
use redis::aio::ConnectionLike;

#[derive(Clone)]
pub struct RedisKeyValue {
    client: redis::Client,
}

pub struct RedisStoreFactory;

#[async_trait]
impl Store for RedisKeyValue {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        redis::cmd("GET")
            .arg(key)
            .query_async(&mut connection)
            .await
            .map_err(Error::msg)
    }
}

impl StoreFactory for RedisStoreFactory {
    fn create(&self, name: &str) -> Arc<dyn Store> {
        /*let store = RedisKeyValue{};
        Arc::new(store)*/
        todo!()
    }
}

