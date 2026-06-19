//! Access/error log: one structured record per request, and optionally one per
//! outbound fetch.
//!
//! This is intentionally distinct from the two existing streams:
//!   * the WASM *application* log (free-form output emitted by the app), and
//!   * the billing *stats* stream (`StatsVisitor` / `StatRow`), which only
//!     covers requests that reached an executor.
//!
//! The access log instead aims to record *every* request — including ones
//! rejected before execution (unknown app, disabled, rate-limited, context
//! errors) — together with HTTP-shaped fields (method, host, path, status).
//!
//! Both the HTTP service and the ProxyWasm service produce the same
//! [`AccessRecord`]; the [`Service`] field discriminates them. Records are
//! handed to an injected [`AccessLogSink`]; the concrete transport (e.g. syslog
//! over UDP) lives in the server crate so this crate stays transport-agnostic.

use serde::Serialize;
use smol_str::{SmolStr, ToSmolStr};
use std::sync::Arc;
use std::time::Instant;

/// Severity of a record. The sink maps these onto its transport's severities
/// (e.g. syslog `info`/`warning`/`error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    #[default]
    Info,
    Warning,
    Error,
}

/// Which service produced the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    Http,
    #[serde(rename = "proxywasm")]
    ProxyWasm,
}

/// High-level request outcome, independent of the HTTP status code. Drives the
/// record [`Level`] via [`Outcome::level`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Request handled normally (does not imply a 2xx status).
    #[default]
    Ok,
    /// No application matched the request.
    UnknownApp,
    /// Application is draft/disabled.
    Disabled,
    /// Application is rate limited.
    RateLimited,
    /// Failed to obtain an executor context.
    ContextError,
    /// Execution timed out (epoch interrupt or elapsed deadline).
    Timeout,
    /// Out of memory.
    Oom,
    /// Application exited with a non-zero code.
    AppError,
    /// Unrecognized WASM trap.
    Trap,
    /// Generic execution error.
    ExecuteError,
}

impl Outcome {
    /// Default severity for this outcome.
    pub fn level(self) -> Level {
        match self {
            Outcome::Ok => Level::Info,
            Outcome::UnknownApp | Outcome::Disabled | Outcome::RateLimited => Level::Warning,
            Outcome::ContextError
            | Outcome::Timeout
            | Outcome::Oom
            | Outcome::AppError
            | Outcome::Trap
            | Outcome::ExecuteError => Level::Error,
        }
    }
}

/// One record per request. Pop/region/country are intentionally *not* here —
/// the sink (which has node context) adds them when framing.
#[derive(Debug, Clone, Serialize)]
pub struct AccessRecord {
    pub service: Service,
    /// Request start, RFC3339 with millisecond precision.
    pub ts: SmolStr,
    /// W3C `traceparent` (or a generated id when the header was absent).
    pub traceparent: SmolStr,
    pub level: Level,
    pub outcome: Outcome,
    pub method: SmolStr,
    pub host: SmolStr,
    pub path: SmolStr,
    pub scheme: SmolStr,
    pub client_ip: SmolStr,
    /// HTTP status returned to the client.
    pub status: u16,
    /// `X-CDN-Internal-Status` value (0 = none).
    pub internal_code: u16,
    pub app_id: u64,
    pub app_name: SmolStr,
    pub client_id: u64,
    /// End-to-end wall-clock duration of the handler, microseconds.
    pub duration_us: u64,
    /// WASM heap used, bytes.
    pub memory_used: u64,
    /// Number of outbound fetches performed.
    pub ext_count: i32,
    /// Cumulative outbound fetch time, microseconds.
    pub ext_time_us: u64,
}

/// One record per outbound fetch (e.g. `proxy_http_call`). Correlates to its
/// parent [`AccessRecord`] via `traceparent`; `call_id` orders fetches within a
/// request.
#[derive(Debug, Clone, Serialize)]
pub struct FetchRecord {
    pub service: Service,
    pub ts: SmolStr,
    pub traceparent: SmolStr,
    pub call_id: u32,
    pub level: Level,
    pub backend: SmolStr,
    pub method: SmolStr,
    pub host: SmolStr,
    pub path: SmolStr,
    /// Upstream response status (0 if the call failed before a response).
    pub status: u16,
    pub duration_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<SmolStr>,
}

/// An event handed to a sink. Kept as an enum so a single channel/queue carries
/// both record kinds.
pub enum LogEvent {
    Request(AccessRecord),
    Fetch(FetchRecord),
}

/// Transport for access/error log events. Implementations MUST NOT block the
/// calling (request) task — they should enqueue and return, dropping on
/// overflow rather than applying backpressure.
pub trait AccessLogSink: Send + Sync {
    fn log(&self, event: LogEvent);
}

/// Current wall-clock time as an RFC3339 string with millisecond precision.
pub fn now_rfc3339() -> SmolStr {
    chrono::Utc::now()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        .to_smolstr()
}

/// RAII guard that guarantees exactly one [`AccessRecord`] is emitted per
/// request — on every return path, including early rejects and panics.
///
/// Create it at the top of the request handler, fill fields as they become
/// known, and let it drop. The wall-clock duration is computed at drop time.
pub struct RequestLog {
    sink: Option<Arc<dyn AccessLogSink>>,
    start: Instant,
    rec: AccessRecord,
}

impl RequestLog {
    /// Begin a record. `sink` is `None` when access logging is disabled, in
    /// which case the guard is a cheap no-op.
    pub fn begin(
        sink: Option<Arc<dyn AccessLogSink>>,
        service: Service,
        traceparent: SmolStr,
    ) -> Self {
        Self {
            start: Instant::now(),
            rec: AccessRecord {
                service,
                ts: now_rfc3339(),
                traceparent,
                level: Level::Info,
                outcome: Outcome::Ok,
                method: SmolStr::default(),
                host: SmolStr::default(),
                path: SmolStr::default(),
                scheme: SmolStr::default(),
                client_ip: SmolStr::default(),
                status: 0,
                internal_code: 0,
                app_id: 0,
                app_name: SmolStr::default(),
                client_id: 0,
                duration_us: 0,
                memory_used: 0,
                ext_count: 0,
                ext_time_us: 0,
            },
            sink,
        }
    }

    /// Whether a sink is attached. Lets callers skip work (e.g. fetching extra
    /// request properties) when logging is disabled.
    pub fn is_enabled(&self) -> bool {
        self.sink.is_some()
    }

    pub fn set_request(
        &mut self,
        method: impl Into<SmolStr>,
        host: impl Into<SmolStr>,
        path: impl Into<SmolStr>,
        scheme: impl Into<SmolStr>,
        client_ip: impl Into<SmolStr>,
    ) {
        self.rec.method = method.into();
        self.rec.host = host.into();
        self.rec.path = path.into();
        self.rec.scheme = scheme.into();
        self.rec.client_ip = client_ip.into();
    }

    pub fn set_app(&mut self, app_id: u64, app_name: impl Into<SmolStr>, client_id: u64) {
        self.rec.app_id = app_id;
        self.rec.app_name = app_name.into();
        self.rec.client_id = client_id;
    }

    pub fn set_status(&mut self, status: u16) {
        self.rec.status = status;
    }

    pub fn set_internal_code(&mut self, code: u16) {
        self.rec.internal_code = code;
    }

    /// Set the outcome; also updates [`Level`] via [`Outcome::level`].
    pub fn set_outcome(&mut self, outcome: Outcome) {
        self.rec.outcome = outcome;
        self.rec.level = outcome.level();
    }

    pub fn set_memory(&mut self, bytes: u64) {
        self.rec.memory_used = bytes;
    }

    pub fn set_ext(&mut self, count: i32, time_us: u64) {
        self.rec.ext_count = count;
        self.rec.ext_time_us = time_us;
    }
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        let Some(sink) = self.sink.take() else {
            return;
        };
        self.rec.duration_us = self.start.elapsed().as_micros() as u64;
        sink.log(LogEvent::Request(self.rec.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct CollectingSink {
        events: Mutex<Vec<AccessRecord>>,
    }

    impl AccessLogSink for CollectingSink {
        fn log(&self, event: LogEvent) {
            if let LogEvent::Request(rec) = event {
                self.events.lock().unwrap().push(rec);
            }
        }
    }

    #[test]
    fn emits_exactly_one_record_on_drop() {
        let sink = Arc::new(CollectingSink::default());
        {
            let mut log = RequestLog::begin(
                Some(sink.clone()),
                Service::ProxyWasm,
                "00-trace-span-01".into(),
            );
            log.set_request("GET", "example.com", "/x", "https", "203.0.113.7");
            log.set_app(42, "my-app", 7);
            log.set_status(429);
            log.set_outcome(Outcome::RateLimited);
        }
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let rec = &events[0];
        assert_eq!(rec.status, 429);
        assert_eq!(rec.outcome, Outcome::RateLimited);
        assert_eq!(rec.level, Level::Warning);
        assert_eq!(rec.app_name, "my-app");
    }

    #[test]
    fn disabled_sink_is_noop() {
        let mut log = RequestLog::begin(None, Service::Http, "id".into());
        assert!(!log.is_enabled());
        log.set_status(200);
        // dropping must not panic
    }

    #[test]
    fn outcome_levels() {
        assert_eq!(Outcome::Ok.level(), Level::Info);
        assert_eq!(Outcome::UnknownApp.level(), Level::Warning);
        assert_eq!(Outcome::Timeout.level(), Level::Error);
    }
}
