use serde::Serialize;
use std::sync::Arc;

use std::time::{Duration, Instant};

pub trait StatsWriter<T: Serialize> {
    fn write_stats(&self, stat: T);
}

pub struct StatsTimer {
    /// A stats ref for automatic recording of observations.
    stats: Arc<dyn StatsVisitor>,
    /// Whether the timer has already been observed once.
    observed: bool,
    /// Starting instant for the timer.
    start: Instant,
}

pub trait StatsVisitor: Send + Sync {
    /// Register stats related to an app
    //fn register_app<T: ToSmolStr>(&self, name: T, app: &App);
    /// Register http execution status code
    fn status_code(&self, status_code: u16);
    /// Register memory used by wasm execution
    fn memory_used(&self, memory_used: u64);
    /// Register failure reason code
    fn fail_reason(&self, fail_reason: u32);
    /// Observe elapsed time
    fn observe(&self, elapsed: Duration);
    /// Get elapsed time in microseconds
    fn get_time_elapsed(&self) -> u64;
    /// Get memory used in bytes
    fn get_memory_used(&self) -> u64;
}

impl StatsTimer {
    pub fn new(stats: Arc<dyn StatsVisitor>) -> Self {
        Self {
            stats,
            observed: false,
            start: Instant::now(),
        }
    }

    /// Discard timer without recording duration
    pub fn discard(mut self) {
        self.observed = true;
    }

    /// Observe and record timer duration
    pub fn observe_duration(mut self) {
        self.observe();
    }

    fn observe(&mut self) {
        let v = self.start.elapsed();
        self.observed = true;
        self.stats.observe(v);
    }
}

impl Drop for StatsTimer {
    fn drop(&mut self) {
        if !self.observed {
            self.observe()
        }
    }
}
