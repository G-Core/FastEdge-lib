use async_trait::async_trait;
use reactor::gcore::fastedge::secret;

pub trait SecretStrategy {
    fn get(&self, key: String) -> anyhow::Result<Option<Vec<u8>>>;
    fn get_effective_at(&self, key: String, at: u64) -> anyhow::Result<Option<Vec<u8>>>;
}

#[derive(Clone)]
pub struct Secret<T: SecretStrategy> {
    strategy: T,
}

#[async_trait]
impl<T: SecretStrategy + Send> secret::Host for Secret<T> {
    async fn get(
        &mut self,
        key: String,
    ) -> wasmtime::Result<Result<Option<String>, secret::Error>> {
        Ok(match self.strategy.get(key) {
            Ok(None) => Ok(None),
            Ok(Some(plaintext)) => Ok(Some(String::from_utf8(plaintext)?)),
            Err(error) => {
                tracing::error!(cause=?error, "decryption error");
                Err(secret::Error::DecryptError)
            }
        })
    }

    async fn get_effective_at(
        &mut self,
        key: String,
        at: u32,
    ) -> wasmtime::Result<Result<Option<String>, secret::Error>> {
        Ok(match self.strategy.get_effective_at(key, at as u64) {
            Ok(None) => Ok(None),
            Ok(Some(plaintext)) => Ok(Some(String::from_utf8(plaintext)?)),
            Err(error) => {
                tracing::error!(cause=?error, "decryption error");
                Err(secret::Error::DecryptError)
            }
        })
    }
}

impl<T: SecretStrategy> Secret<T> {
    pub fn new(strategy: T) -> Self {
        Self { strategy }
    }
}
