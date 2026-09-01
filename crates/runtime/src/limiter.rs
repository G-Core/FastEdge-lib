use anyhow::{Error, Result};
use tracing::instrument;
use wasmtime::{ResourceLimiter, StoreLimits};

/// A proxy wrapper of `wasmtime::ResourceLimiter` trait impl used to track used memory
#[derive(Clone, Debug)]
pub(crate) struct ProxyLimiter {
    pub(crate) allocated: usize,
    /// Set when a memory growth request is denied because the desired size
    /// exceeds the configured limit. This covers the instantiation-time case
    /// where a module's declared minimum memory already exceeds the limit
    /// (surfaced by wasmtime as "memory minimum size of N pages exceeds memory
    /// limits"), letting callers classify the failure as out-of-memory.
    pub(crate) oom: bool,
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
            oom: false,
            inner,
        }
    }
}

impl Default for ProxyLimiter {
    fn default() -> Self {
        ProxyLimiter {
            allocated: 0,
            oom: false,
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
        } else {
            // Growth denied because `desired` exceeds the configured limit.
            // Record it so the failure can be classified as out-of-memory.
            self.oom = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 64 * 1024;

    #[test]
    fn grow_within_limit_sets_no_oom() {
        let mut limiter = ProxyLimiter::new(2 * PAGE);
        let ret = limiter.memory_growing(0, PAGE, None).unwrap();
        assert!(ret);
        assert!(!limiter.oom);
        assert_eq!(limiter.allocated, PAGE);
    }

    #[test]
    fn grow_exceeding_limit_sets_oom() {
        // Requesting more than the configured limit is denied and flagged as OOM.
        let mut limiter = ProxyLimiter::new(PAGE);
        let ret = limiter.memory_growing(0, 2 * PAGE, None).unwrap();
        assert!(!ret);
        assert!(limiter.oom);
        assert_eq!(limiter.allocated, 0);
    }
}
