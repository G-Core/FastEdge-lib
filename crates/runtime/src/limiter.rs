use anyhow::{Error, Result};
use tracing::instrument;
use wasmtime::{ResourceLimiter, StoreLimits};

/// A proxy wrapper of `wasmtime::ResourceLimiter` trait impl used to track used memory
#[derive(Clone, Debug)]
pub(crate) struct ProxyLimiter {
    pub(crate) allocated: usize,
    inner: StoreLimits,
}

impl ProxyLimiter {
    #[instrument(level = "trace")]
    pub fn new(max_memory_size: usize) -> Self {
        let inner = wasmtime::StoreLimitsBuilder::new()
            .memory_size(max_memory_size)
            .build();
        Self {
            allocated: 0,
            inner,
        }
    }
}

impl Default for ProxyLimiter {
    fn default() -> Self {
        ProxyLimiter {
            allocated: 0,
            inner: Default::default(),
        }
    }
}

impl ResourceLimiter for ProxyLimiter {
    #[instrument(ret, err, level = "trace")]
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool> {
        let ret = self.inner.memory_growing(current, desired, maximum)?;
        // increment used memory
        if ret {
            self.allocated += desired - current;
        }
        Ok(ret)
    }

    #[instrument(ret, err, level = "trace")]
    fn memory_grow_failed(&mut self, error: Error) -> Result<()> {
        self.inner.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool> {
        self.inner.table_growing(current, desired, maximum)
    }

    fn table_grow_failed(&mut self, error: Error) -> Result<()> {
        self.inner.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}
