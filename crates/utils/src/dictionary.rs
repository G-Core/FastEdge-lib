use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use reactor::gcore::fastedge::dictionary;

#[derive(Clone)]
pub struct Dictionary {
    inner: HashMap<String, String>,
}

impl dictionary::Host for Dictionary {
    async fn get(&mut self, name: String) -> Option<String> {
        self.inner.get(&name).map(|v| v.to_string())
    }
}

impl Default for Dictionary {
    fn default() -> Self {
        Self {
            inner: Default::default(),
        }
    }
}

impl Dictionary {
    pub fn new() -> Self {
        Self {
            inner: Default::default(),
        }
    }
}

impl Deref for Dictionary {
    type Target = HashMap<String, String>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Dictionary {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}