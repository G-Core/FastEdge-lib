use reactor::gcore::fastedge::secret;
use std::ops::Deref;
use std::sync::Arc;

/// Secret strategy trait
pub trait SecretStrategy: Send + Sync {
    fn get(&self, key: String) -> anyhow::Result<Option<Vec<u8>>>;
    fn get_effective_at(&self, key: String, at: u64) -> anyhow::Result<Option<Vec<u8>>>;
}

/// Secret store implementation
#[derive(Clone)]
pub struct SecretStore {
    strategy: Arc<dyn SecretStrategy>,
}

impl secret::Host for SecretStore {
    async fn get(&mut self, key: String) -> Result<Option<String>, secret::Error> {
        match self.strategy.get(key) {
            Ok(None) => Ok(None),
            Ok(Some(plaintext)) => Ok(Some(
                String::from_utf8(plaintext).map_err(|e| secret::Error::Other(e.to_string()))?,
            )),
            Err(error) => {
                tracing::error!(cause=?error, "decryption error");
                Err(secret::Error::DecryptError)
            }
        }
    }

    async fn get_effective_at(
        &mut self,
        key: String,
        at: u32,
    ) -> Result<Option<String>, secret::Error> {
        match self.strategy.get_effective_at(key, at as u64) {
            Ok(None) => Ok(None),
            Ok(Some(plaintext)) => Ok(Some(
                String::from_utf8(plaintext).map_err(|e| secret::Error::Other(e.to_string()))?,
            )),
            Err(error) => {
                tracing::error!(cause=?error, "decryption error");
                Err(secret::Error::DecryptError)
            }
        }
    }
}

impl SecretStore {
    pub fn new(strategy: Arc<dyn SecretStrategy>) -> Self {
        Self { strategy }
    }
}

impl Deref for SecretStore {
    type Target = dyn SecretStrategy;

    fn deref(&self) -> &Self::Target {
        self.strategy.as_ref()
    }
}

/// Default secret strategy that does not decrypt anything and always returns None
struct DefaultSecretStrategy {}

impl SecretStrategy for DefaultSecretStrategy {
    fn get(&self, _key: String) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn get_effective_at(&self, _key: String, _at: u64) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

impl Default for SecretStore {
    fn default() -> Self {
        Self {
            strategy: Arc::new(DefaultSecretStrategy {}),
        }
    }
}
