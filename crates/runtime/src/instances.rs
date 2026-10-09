//! Accounting for concurrently live WASM instances.
//!
//! Every request builds a [`crate::store::Store`], instantiates a module into it, and drops
//! both when the request finishes. For that whole span the instance holds one slot in each
//! of the wasmtime pooling allocator's pools — linear memory, async stack, table, core
//! instance — all sized by `max_execution_stacks` in `wasm-config`.
//!
//! Exhausting those pools is *not* graceful: the allocator returns "maximum concurrent limit
//! of N for ... reached", which surfaces as an instantiation error rather than the adaptive
//! run-queue shedding path. Sizing the pool therefore needs the observed concurrency, which
//! is what this module exports.
//!
//! The guard lives in [`crate::Data`], the store's data type, so every executor that goes
//! through `StoreBuilder::build` — ProxyWasm, `http-handler`, `wasi:http` — is counted
//! against the same pool it actually draws from. There is deliberately no `executor` label:
//! the pools are per-`Engine` and shared, so only the total is meaningful for sizing.
//!
//! The counters are plain atomics; embedders export them (e.g. as the Prometheus gauges
//! `fastedge_wasm_instances_live` / `fastedge_wasm_instances_peak`) by calling
//! [`flush_peak_and_live`] once per scrape. That returns both values from a single `live`
//! sample, which is what keeps the exported peak from ever reading below the exported
//! live count; reading [`live`] and [`flush_peak`] separately does not.

use std::sync::atomic::{AtomicI64, Ordering};

/// Currently live instances.
static LIVE: AtomicI64 = AtomicI64::new(0);

/// Running max since the previous [`flush_peak`]. Kept separately so the peak can be
/// updated with an atomic `fetch_max` and drained with `swap(0)` on scrape. A plain
/// scrape-time sample of [`LIVE`] would miss the sub-second concurrency spikes that a
/// stalled backend produces — exactly the peaks the pool has to be sized for.
static PEAK: AtomicI64 = AtomicI64::new(0);

fn acquire() {
    // `fetch_add` returns the previous value, so `+ 1` is the level that this
    // acquisition genuinely produced — a valid sample for the peak.
    let live = LIVE.fetch_add(1, Ordering::Relaxed) + 1;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

fn release() {
    LIVE.fetch_sub(1, Ordering::Relaxed);
}

/// Currently live instances.
pub fn live() -> i64 {
    LIVE.load(Ordering::Relaxed)
}

/// Peak since the previous [`flush_peak`]. Test/diagnostic accessor.
pub fn peak() -> i64 {
    PEAK.load(Ordering::Relaxed)
}

/// Drain and return the peak since the previous call; invoke on each metrics scrape.
/// Never reports less than the currently live count, so a long-running steady load
/// can't read as 0.
///
/// Prefer [`flush_peak_and_live`] when exporting both gauges: reading `live` separately
/// can observe a *later*, higher level than the one folded in here and so publish a
/// scrape where peak < live.
pub fn flush_peak() -> i64 {
    let (peak, _live) = flush_peak_and_live();
    peak
}

/// Drain the peak and sample the live count for a single scrape, returning
/// `(peak, live)`.
///
/// `live` is sampled *before* the peak is drained and is the same value folded into the
/// peak, so the returned pair always satisfies `peak >= live`. Sampling the two
/// independently does not: an instance acquired between the two reads raises `live`
/// above a peak that was already computed, publishing a scrape that contradicts the
/// documented "never less than currently live" semantics.
pub fn flush_peak_and_live() -> (i64, i64) {
    // Exactly one `live()` read: taking the sample here and threading it through is what
    // makes `peak >= live` structural rather than timing-dependent.
    drain_peak_against(live())
}

/// Drain `PEAK` and fold `live_sample` into it, returning `(peak, live_sample)`.
///
/// Both halves of the returned pair derive from the single caller-supplied sample, so
/// `peak >= live` holds for any input — including a fully drained `PEAK`.
fn drain_peak_against(live_sample: i64) -> (i64, i64) {
    let peak = PEAK.swap(0, Ordering::Relaxed).max(live_sample);
    (peak, live_sample)
}

/// RAII counter for one live WASM instance.
///
/// Held by [`crate::Data`] so the count spans exactly the lifetime of the store that owns
/// the pooled slots, including the unwind path when a guest traps or the request is
/// cancelled mid-flight.
#[derive(Debug)]
pub struct LiveInstanceGuard;

impl LiveInstanceGuard {
    pub fn new() -> Self {
        acquire();
        LiveInstanceGuard
    }
}

impl Default for LiveInstanceGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LiveInstanceGuard {
    fn drop(&mut self) {
        release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// `LIVE` / `PEAK` are process-global, so a test that drains the peak must not run
    /// alongside one asserting it is monotonic. Serialise every test that touches them.
    fn serial() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Guards from concurrent tests share the process-global counter, so assertions are on
    /// deltas rather than absolute values.
    #[test]
    fn guard_tracks_live_count_and_raises_peak() {
        let _serial = serial();
        let before = live();

        let first = LiveInstanceGuard::new();
        assert_eq!(live(), before + 1);
        let second = LiveInstanceGuard::new();
        assert_eq!(live(), before + 2);
        assert!(peak() >= before + 2, "peak must cover the observed level");

        let peak_at_top = peak();
        drop(second);
        assert_eq!(live(), before + 1);
        drop(first);
        assert_eq!(live(), before);

        // The high-water mark is monotonic: dropping instances must not lower it.
        assert_eq!(peak(), peak_at_top);
    }

    /// The core invariant, independent of timing: whatever live level a scrape samples,
    /// the peak reported alongside it is never lower — even when `PEAK` was just drained
    /// and contributes nothing. This is the case the old two-read scrape got wrong: it
    /// computed the peak from one `live()` read and published a second, later one.
    #[test]
    fn drained_peak_still_reports_at_least_the_live_sample() {
        let _serial = serial();
        for live_sample in [0, 1, 7, 1_000] {
            PEAK.store(0, Ordering::Relaxed);
            let (peak, live) = drain_peak_against(live_sample);
            assert_eq!(live, live_sample);
            assert!(peak >= live, "peak {peak} < live {live}");
        }
    }

    /// A recorded spike still wins when it exceeds the live level at scrape time.
    #[test]
    fn recorded_spike_outranks_a_lower_live_sample() {
        let _serial = serial();
        PEAK.store(50, Ordering::Relaxed);
        let (peak, live) = drain_peak_against(3);
        assert_eq!((peak, live), (50, 3));
        // Draining is destructive: the next scrape falls back to the live sample.
        let (peak, _) = drain_peak_against(3);
        assert_eq!(peak, 3);
    }

    /// End-to-end through the real globals: acquiring after a drain must not produce a
    /// scrape where peak < live.
    #[test]
    fn flush_peak_and_live_holds_invariant_after_a_drain() {
        let _serial = serial();
        let _first = LiveInstanceGuard::new();
        // Drain, so `PEAK` is 0 and only the live sample can carry the invariant.
        let _ = flush_peak_and_live();

        let _second = LiveInstanceGuard::new();
        let (peak, live) = flush_peak_and_live();
        assert!(peak >= live, "peak {peak} < live {live}");
    }
}
