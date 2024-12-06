use async_trait::async_trait;
use reactor::gcore::fastedge::secret;

pub trait SecretStrategy {
    fn get(&self, key: String) -> anyhow::Result<Option<Vec<u8>>>;
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
}

impl<T: SecretStrategy> Secret<T> {
    pub fn new(strategy: T) -> Self {
        Self { strategy }
    }
}
