use secret::SecretStrategy;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Default)]
pub struct SecretImpl {
    inner: HashMap<String, String>,
}

impl SecretStrategy for SecretImpl {
    fn get(&self, key: String) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.inner.get(&key).map(|v| v.as_bytes().to_vec()))
    }

    fn get_effective_at(&self, key: String, _at: u64) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.inner.get(&key).map(|v| v.as_bytes().to_vec()))
    }
}

impl Deref for SecretImpl {
    type Target = HashMap<String, String>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for SecretImpl {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
