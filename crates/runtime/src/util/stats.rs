use serde::Serialize;
use std::sync::Arc;

use http_backend::stats::ExtRequestStats;
pub use key_value_store::ReadStats;
use std::time::{Duration, Instant};
use utils::UserDiagStats;

#[repr(i32)]
pub enum CdnPhase {
    Http = 0,
    RequestHeaders = 1,
    RequestBody = 2,
    ResponseHeaders = 3,
    ResponseBody = 4,
    Log = 5,
}

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

pub trait StatsVisitor: ReadStats + UserDiagStats + ExtRequestStats + Send + Sync {
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
    /// Register cdn phase
    fn cdn_phase(&self, phase: CdnPhase);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::thread;

    // Mock implementation of StatsVisitor for testing
    #[derive(Default)]
    struct MockStatsVisitor {
        status_code: AtomicU16,
        memory_used: AtomicU64,
        fail_reason: AtomicU32,
        observed_duration: Mutex<Option<Duration>>,
        time_elapsed: AtomicU64,
        cdn_phase: AtomicI32,
        user_diag: Mutex<String>,
        kv_reads: AtomicI32,
        byod_reads: AtomicI32,
        observe_called: AtomicBool,
    }

    impl MockStatsVisitor {
        fn new() -> Self {
            Self::default()
        }

        fn get_status_code(&self) -> u16 {
            self.status_code.load(Ordering::Relaxed)
        }

        fn get_memory_used_internal(&self) -> u64 {
            self.memory_used.load(Ordering::Relaxed)
        }

        fn get_fail_reason(&self) -> u32 {
            self.fail_reason.load(Ordering::Relaxed)
        }

        fn get_observed_duration(&self) -> Option<Duration> {
            self.observed_duration.lock().unwrap().clone()
        }

        fn get_cdn_phase(&self) -> i32 {
            self.cdn_phase.load(Ordering::Relaxed)
        }

        fn get_user_diag(&self) -> String {
            self.user_diag.lock().unwrap().clone()
        }

        fn was_observe_called(&self) -> bool {
            self.observe_called.load(Ordering::Relaxed)
        }
    }

    impl ReadStats for MockStatsVisitor {
        fn count_kv_read(&self, value: i32) {
            self.kv_reads.fetch_add(value, Ordering::Relaxed);
        }

        fn count_kv_byod_read(&self, value: i32) {
            self.byod_reads.fetch_add(value, Ordering::Relaxed);
        }
    }

    impl UserDiagStats for MockStatsVisitor {
        fn set_user_diag(&self, diag: &str) {
            *self.user_diag.lock().unwrap() = diag.to_string();
        }
    }

    impl ExtRequestStats for MockStatsVisitor {
        fn observe_ext(&self, _: std::time::Duration) {}
    }

    impl StatsVisitor for MockStatsVisitor {
        fn status_code(&self, status_code: u16) {
            self.status_code.store(status_code, Ordering::Relaxed);
        }

        fn memory_used(&self, memory_used: u64) {
            self.memory_used.store(memory_used, Ordering::Relaxed);
        }

        fn fail_reason(&self, fail_reason: u32) {
            self.fail_reason.store(fail_reason, Ordering::Relaxed);
        }

        fn observe(&self, elapsed: Duration) {
            self.observe_called.store(true, Ordering::Relaxed);
            *self.observed_duration.lock().unwrap() = Some(elapsed);
        }

        fn get_time_elapsed(&self) -> u64 {
            self.time_elapsed.load(Ordering::Relaxed)
        }

        fn get_memory_used(&self) -> u64 {
            self.memory_used.load(Ordering::Relaxed)
        }

        fn cdn_phase(&self, phase: CdnPhase) {
            self.cdn_phase.store(phase as i32, Ordering::Relaxed);
        }
    }

    #[test]
    fn test_cdn_phase_values() {
        assert_eq!(CdnPhase::Http as i32, 0);
        assert_eq!(CdnPhase::RequestHeaders as i32, 1);
        assert_eq!(CdnPhase::RequestBody as i32, 2);
        assert_eq!(CdnPhase::ResponseHeaders as i32, 3);
        assert_eq!(CdnPhase::ResponseBody as i32, 4);
        assert_eq!(CdnPhase::Log as i32, 5);
    }

    #[test]
    fn test_stats_visitor_status_code() {
        let stats = MockStatsVisitor::new();

        stats.status_code(200);
        assert_eq!(stats.get_status_code(), 200);

        stats.status_code(404);
        assert_eq!(stats.get_status_code(), 404);

        stats.status_code(500);
        assert_eq!(stats.get_status_code(), 500);
    }

    #[test]
    fn test_stats_visitor_memory_used() {
        let stats = MockStatsVisitor::new();

        stats.memory_used(1024);
        assert_eq!(stats.get_memory_used_internal(), 1024);

        stats.memory_used(2048);
        assert_eq!(stats.get_memory_used_internal(), 2048);
    }

    #[test]
    fn test_stats_visitor_fail_reason() {
        let stats = MockStatsVisitor::new();

        stats.fail_reason(1);
        assert_eq!(stats.get_fail_reason(), 1);

        stats.fail_reason(42);
        assert_eq!(stats.get_fail_reason(), 42);
    }

    #[test]
    fn test_stats_visitor_cdn_phase() {
        let stats = MockStatsVisitor::new();

        stats.cdn_phase(CdnPhase::Http);
        assert_eq!(stats.get_cdn_phase(), 0);

        stats.cdn_phase(CdnPhase::RequestHeaders);
        assert_eq!(stats.get_cdn_phase(), 1);

        stats.cdn_phase(CdnPhase::ResponseBody);
        assert_eq!(stats.get_cdn_phase(), 4);
    }

    #[test]
    fn test_stats_visitor_user_diag() {
        let stats = MockStatsVisitor::new();

        stats.set_user_diag("test diagnostic");
        assert_eq!(stats.get_user_diag(), "test diagnostic");

        stats.set_user_diag("another message");
        assert_eq!(stats.get_user_diag(), "another message");
    }

    #[test]
    fn test_stats_visitor_read_stats() {
        let stats = MockStatsVisitor::new();

        stats.count_kv_read(10);
        stats.count_kv_read(5);
        assert_eq!(stats.kv_reads.load(Ordering::Relaxed), 15);

        stats.count_kv_byod_read(3);
        stats.count_kv_byod_read(7);
        assert_eq!(stats.byod_reads.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn test_stats_timer_new() {
        let stats = Arc::new(MockStatsVisitor::new());
        let timer = StatsTimer::new(stats.clone());

        assert!(!timer.observed);
        assert!(!stats.was_observe_called());
    }

    #[test]
    fn test_stats_timer_observe_duration() {
        let stats = Arc::new(MockStatsVisitor::new());
        let timer = StatsTimer::new(stats.clone());

        thread::sleep(Duration::from_millis(10));
        timer.observe_duration();

        assert!(stats.was_observe_called());
        let duration = stats.get_observed_duration();
        assert!(duration.is_some());
        assert!(duration.unwrap() >= Duration::from_millis(10));
    }

    #[test]
    fn test_stats_timer_discard() {
        let stats = Arc::new(MockStatsVisitor::new());
        let timer = StatsTimer::new(stats.clone());

        timer.discard();

        assert!(!stats.was_observe_called());
        assert!(stats.get_observed_duration().is_none());
    }

    #[test]
    fn test_stats_timer_drop_auto_observe() {
        let stats = Arc::new(MockStatsVisitor::new());

        {
            let _timer = StatsTimer::new(stats.clone());
            thread::sleep(Duration::from_millis(10));
            // Timer goes out of scope here, should auto-observe
        }

        assert!(stats.was_observe_called());
        let duration = stats.get_observed_duration();
        assert!(duration.is_some());
        assert!(duration.unwrap() >= Duration::from_millis(10));
    }

    #[test]
    fn test_stats_timer_drop_after_observe_no_double_observe() {
        let stats = Arc::new(MockStatsVisitor::new());
        let first_duration;

        {
            let timer = StatsTimer::new(stats.clone());
            thread::sleep(Duration::from_millis(10));
            timer.observe_duration();
            first_duration = stats.get_observed_duration().unwrap();
            // Even though timer goes out of scope, observe should not be called again
        }

        let final_duration = stats.get_observed_duration().unwrap();
        assert_eq!(first_duration, final_duration);
    }

    #[test]
    fn test_stats_timer_drop_after_discard_no_observe() {
        let stats = Arc::new(MockStatsVisitor::new());

        {
            let timer = StatsTimer::new(stats.clone());
            thread::sleep(Duration::from_millis(10));
            timer.discard();
            // Timer goes out of scope, but should not observe since discarded
        }

        assert!(!stats.was_observe_called());
        assert!(stats.get_observed_duration().is_none());
    }

    #[test]
    fn test_stats_timer_multiple_instances() {
        let stats = Arc::new(MockStatsVisitor::new());

        let timer1 = StatsTimer::new(stats.clone());
        let timer2 = StatsTimer::new(stats.clone());

        thread::sleep(Duration::from_millis(10));
        timer1.observe_duration();

        assert!(stats.was_observe_called());

        thread::sleep(Duration::from_millis(5));
        timer2.observe_duration();

        // Last observed duration should be from timer2
        let duration = stats.get_observed_duration().unwrap();
        assert!(duration >= Duration::from_millis(5));
    }

    #[test]
    fn test_stats_timer_measures_elapsed_time() {
        let stats = Arc::new(MockStatsVisitor::new());
        let timer = StatsTimer::new(stats.clone());

        let sleep_duration = Duration::from_millis(50);
        thread::sleep(sleep_duration);
        timer.observe_duration();

        let observed = stats.get_observed_duration().unwrap();
        // Allow some tolerance for timing variations
        assert!(observed >= sleep_duration);
        assert!(observed < sleep_duration + Duration::from_millis(50));
    }

    #[test]
    fn test_mock_stats_visitor_concurrent_access() {
        let stats = Arc::new(MockStatsVisitor::new());
        let mut handles = vec![];

        for i in 0..10 {
            let stats_clone = stats.clone();
            let handle = thread::spawn(move || {
                stats_clone.status_code(200 + i);
                stats_clone.memory_used(1024 * (i as u64 + 1));
                stats_clone.count_kv_read(1);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Should have 10 kv reads
        assert_eq!(stats.kv_reads.load(Ordering::Relaxed), 10);
    }
}
