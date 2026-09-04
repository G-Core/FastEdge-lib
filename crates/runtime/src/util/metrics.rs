//! App-call metrics hook.
//!
//! The runtime reports every app-call outcome through a process-global sink so
//! that embedders (e.g. the FastEdge server) can export them however they like
//! (Prometheus, etc.) without this crate depending on a metrics backend. When
//! no sink is installed (e.g. the `fastedge-run` CLI) reporting is a no-op.

use std::sync::OnceLock;

use crate::AppResult;

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
