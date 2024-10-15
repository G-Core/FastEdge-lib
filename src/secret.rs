use secret::SecretStrategy;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

#[derive(Clone)]
pub struct SecretImpl {
    inner: HashMap<String, String>,
}

impl SecretStrategy for SecretImpl {
    fn get(&self, key: String) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.inner.get(&key).map(|v| v.as_bytes().to_vec()))
    }
}

impl Default for SecretImpl {
    fn default() -> Self {
        Self {
            inner: Default::default(),
        }
    }
}

impl SecretImpl {
    pub fn new() -> Self {
        Self {
            inner: Default::default(),
        }
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
