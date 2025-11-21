use std::sync::Arc;
use std::time::{Duration, Instant};

pub trait ExtRequestStats: Sync + Send {
    /// Observe elapsed time
    fn observe_ext(&self, elapsed: Duration);
}

pub struct ExtStatsTimer {
    /// A stats ref for automatic recording of observations.
    stats: Arc<dyn ExtRequestStats>,
    /// Whether the timer has already been observed once.
    observed: bool,
    /// Starting instant for the timer.
    start: Instant,
}

impl ExtStatsTimer {
    pub fn new(stats: Arc<dyn ExtRequestStats>) -> Self {
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
        self.stats.observe_ext(v);
    }
}

impl Drop for ExtStatsTimer {
    fn drop(&mut self) {
        if !self.observed {
            self.observe()
        }
    }
}
