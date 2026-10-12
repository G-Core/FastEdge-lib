//! Outbound-request access-log seam.
//!
//! Distinct from [`crate::stats`] (billing/Prometheus timing): this sink emits
//! one access-log record per external HTTP request the guest makes. It is kept
//! dependency-free — primitive argument types only — so `http-backend` need not
//! depend on the runtime's access-log types. The concrete implementation (which
//! builds the runtime record and ships it) lives in the embedder.

use smol_str::SmolStr;
use std::sync::Arc;
use std::time::Duration;

/// Sink for one completed outbound (external) HTTP request.
///
/// `status` is `0` when the request failed before a response was received
/// (connection error / timeout). Implementations must be non-blocking — this is
/// called on the request hot path.
pub trait ExtRequestLog: Send + Sync {
    /// Record one completed outbound request, correlated to the inbound request
    /// via `traceparent`.
    fn log_ext_request(
        &self,
        traceparent: &str,
        app_id: u64,
        method: &str,
        uri: &str,
        status: u16,
        elapsed: Duration,
    );
}

/// A cloneable, connector-agnostic handle carrying everything needed to log one
/// outbound request once it completes. Used by the WASI-HTTP path, where the
/// send happens in a spawned task and the [`crate::Backend`] (generic over its
/// connector) is not available; the handle is captured before the spawn.
#[derive(Clone)]
pub struct ExtRequestLogHandle {
    log: Arc<dyn ExtRequestLog>,
    traceparent: SmolStr,
    app_id: u64,
}

impl ExtRequestLogHandle {
    pub fn new(log: Arc<dyn ExtRequestLog>, traceparent: SmolStr, app_id: u64) -> Self {
        Self {
            log,
            traceparent,
            app_id,
        }
    }

    /// Emit one record for a completed outbound request. `status` is `0` on
    /// failure (no response received).
    pub fn log(&self, method: &str, uri: &str, status: u16, elapsed: Duration) {
        self.log
            .log_ext_request(&self.traceparent, self.app_id, method, uri, status, elapsed);
    }
}
