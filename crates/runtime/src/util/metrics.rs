//! App-call metrics hook.
//!
//! The runtime reports every app-call outcome through a process-global sink so
//! that embedders (e.g. the FastEdge server) can export them however they like
//! (Prometheus, etc.) without this crate depending on a metrics backend. When
//! no sink is installed (e.g. the `fastedge-run` CLI) reporting is a no-op.

use std::sync::OnceLock;

use crate::AppResult;
use crate::epoch_grace::EpochGraceEvent;

/// Where in an app call the outcome was decided.
///
/// Refines the metrics `outcome` label only — stats (`fail_reason`) always
/// carry the plain [`AppResult`], so the ClickHouse encoding is untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPhase {
    /// Guest initialisation (`_initialize`, `on_context_create`): no request
    /// callback had run yet.
    Init,
    /// A request callback (`on_request_headers`, body chunks, `on_log`, …), or
    /// a rejection before the guest was involved at all.
    Request,
}

/// Sink invoked for every app call: outcome, the phase it was decided in,
/// executor label(s), duration in microseconds and WASM linear memory used in
/// bytes.
pub type MetricsSink = fn(
    result: AppResult,
    phase: CallPhase,
    label: &[&str],
    duration: Option<u64>,
    memory_used: Option<u64>,
);

static SINK: OnceLock<MetricsSink> = OnceLock::new();

/// Install the process-global metrics sink. Subsequent calls are ignored.
pub fn set_sink(sink: MetricsSink) {
    let _ = SINK.set(sink);
}

/// Report one app call decided in the request phase (no-op when no sink is
/// installed). The common case; see [`metrics_in_phase`] for the rest.
pub fn metrics(result: AppResult, label: &[&str], duration: Option<u64>, memory_used: Option<u64>) {
    metrics_in_phase(result, CallPhase::Request, label, duration, memory_used);
}

/// Report one app call, stating the phase its outcome was decided in.
pub fn metrics_in_phase(
    result: AppResult,
    phase: CallPhase,
    label: &[&str],
    duration: Option<u64>,
    memory_used: Option<u64>,
) {
    if let Some(sink) = SINK.get() {
        sink(result, phase, label, duration, memory_used);
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
