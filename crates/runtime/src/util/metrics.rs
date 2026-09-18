//! App-call metrics hook.
//!
//! The runtime reports every app-call outcome through a process-global sink so
//! that embedders (e.g. the FastEdge server) can export them however they like
//! (Prometheus, etc.) without this crate depending on a metrics backend. When
//! no sink is installed (e.g. the `fastedge-run` CLI) reporting is a no-op.

use std::sync::OnceLock;

use crate::AppResult;
use crate::epoch_grace::EpochGraceEvent;

/// Sink invoked for every app call: outcome, executor label(s), duration in
/// microseconds and WASM linear memory used in bytes.
pub type MetricsSink =
    fn(result: AppResult, label: &[&str], duration: Option<u64>, memory_used: Option<u64>);

static SINK: OnceLock<MetricsSink> = OnceLock::new();

/// Install the process-global metrics sink. Subsequent calls are ignored.
pub fn set_sink(sink: MetricsSink) {
    let _ = SINK.set(sink);
}

/// Report one app call to the installed sink (no-op when none is installed).
pub fn metrics(result: AppResult, label: &[&str], duration: Option<u64>, memory_used: Option<u64>) {
    if let Some(sink) = SINK.get() {
        sink(result, label, duration, memory_used);
    }
}

// ── epoch stall grace ────────────────────────────────────────────────────────

/// Sink for epoch-deadline decisions taken when the wall budget ran out with
/// no host-call credit (see [`crate::epoch_grace`]).
pub type EpochGraceSink = fn(event: EpochGraceEvent);

static EPOCH_GRACE_SINK: OnceLock<EpochGraceSink> = OnceLock::new();

/// Install the process-global epoch-grace sink. Subsequent calls are ignored.
pub fn set_epoch_grace_sink(sink: EpochGraceSink) {
    let _ = EPOCH_GRACE_SINK.set(sink);
}

/// Report one epoch-grace decision (no-op when no sink is installed).
pub fn report_epoch_grace(event: EpochGraceEvent) {
    if let Some(sink) = EPOCH_GRACE_SINK.get() {
        sink(event);
    }
}
