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
//! `fastedge_wasm_instances_live` / `fastedge_wasm_instances_peak`) by reading [`live`] and
//! draining [`flush_peak`] on each scrape.

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
pub fn flush_peak() -> i64 {
    PEAK.swap(0, Ordering::Relaxed).max(live())
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

    /// Guards from concurrent tests share the process-global counter, so assertions are on
    /// deltas rather than absolute values.
    #[test]
    fn guard_tracks_live_count_and_raises_peak() {
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
}
