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

#[cfg(feature = "metrics")]
mod imp {
    use lazy_static::lazy_static;
    use prometheus::{IntGauge, register_int_gauge};
    use std::sync::atomic::{AtomicI64, Ordering};

    lazy_static! {
        static ref WASM_INSTANCES_LIVE: IntGauge = register_int_gauge!(
            "fastedge_wasm_instances_live",
            "WASM instances currently alive, each holding one slot in every wasmtime pooling-allocator pool"
        )
        .unwrap();

        /// Peak since the previous scrape. A plain gauge is sampled at the scrape
        /// interval and will miss the sub-second concurrency spikes that a stalled backend
        /// produces — exactly the peaks the pool has to be sized for. This one cannot.
        static ref WASM_INSTANCES_PEAK: IntGauge = register_int_gauge!(
            "fastedge_wasm_instances_peak",
            "Highest number of concurrently live WASM instances observed since the \
             previous scrape (resets on scrape)"
        )
        .unwrap();
    }

    /// Running max since the previous scrape, kept separately so the peak can be updated
    /// with an atomic `fetch_max` — `IntGauge` only offers `set`, which cannot express
    /// "raise to" — and drained with `swap(0)` by [`flush_peak`] on scrape.
    static PEAK: AtomicI64 = AtomicI64::new(0);

    pub(super) fn acquire() {
        WASM_INSTANCES_LIVE.inc();
        // Reading back after `inc` may observe another thread's concurrent increment. That
        // is still a level that genuinely occurred, so it is a valid sample for the peak.
        let live = WASM_INSTANCES_LIVE.get();
        PEAK.fetch_max(live, Ordering::Relaxed);
    }

    pub(super) fn release() {
        WASM_INSTANCES_LIVE.dec();
    }

    /// Currently live instances. Test/diagnostic accessor.
    pub fn live() -> i64 {
        WASM_INSTANCES_LIVE.get()
    }

    /// Peak since the previous [`flush_peak`]. Test/diagnostic accessor.
    pub fn peak() -> i64 {
        PEAK.load(Ordering::Relaxed)
    }

    /// Export and reset the peak gauge; call on each Prometheus scrape. Never reports
    /// less than the currently live count, so a long-running steady load can't read as 0.
    pub fn flush_peak() {
        let peak = PEAK
            .swap(0, Ordering::Relaxed)
            .max(WASM_INSTANCES_LIVE.get());
        WASM_INSTANCES_PEAK.set(peak);
    }
}

#[cfg(not(feature = "metrics"))]
mod imp {
    pub(super) fn acquire() {}
    pub(super) fn release() {}
    pub fn flush_peak() {}
}

#[cfg(feature = "metrics")]
pub use imp::{live, peak};
pub use imp::flush_peak;

/// RAII counter for one live WASM instance.
///
/// Held by [`crate::Data`] so the count spans exactly the lifetime of the store that owns
/// the pooled slots, including the unwind path when a guest traps or the request is
/// cancelled mid-flight.
#[derive(Debug)]
pub struct LiveInstanceGuard;

impl LiveInstanceGuard {
    pub fn new() -> Self {
        imp::acquire();
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
        imp::release();
    }
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use super::*;

    /// Guards from concurrent tests share the process-global gauge, so assertions are on
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
