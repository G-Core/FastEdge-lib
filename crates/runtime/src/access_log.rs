//! Per-request system log.
//!
//! This is **not** the guest application log (that ships the WASM app's stdout
//! via [`crate::logger`]). This log emits exactly one record per processed
//! request describing how the edge server handled it — traceparent, final HTTP
//! status, the `X-CDN-Internal-Status` diagnostic code, timing, etc. — for both
//! successful and failed requests, across every execution path (ProxyWasm,
//! FastEdge http-handler, and standard wasi:http).
//!
//! The transport is intentionally abstracted behind [`AccessLogSender`]. The
//! concrete appender (planned: a system log shipped over UDP with a JSON
//! payload) is implemented separately; until one is wired in, the runtime uses
//! [`NoopAccessLogSender`].

use std::time::Duration;

/// Which execution path served the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// ProxyWasm protocol (v1/v2/v2a) — WASI Preview1 core modules.
    ProxyWasm,
    /// HTTP request served by a Preview2 component — both the FastEdge
    /// `gcore:fastedge/http-handler` export and the standard
    /// `wasi:http/incoming-handler` export are reported under this variant.
    Http,
    /// Outbound HTTP request the guest made while serving a request (ProxyWasm
    /// `proxy_http_call` or the WASI/http-handler fetch client). Emitted as its
    /// own record, correlated to the inbound request via `traceparent`.
    ExtHttp,
}

impl RequestKind {
    /// Stable lowercase identifier, suitable for a log field.
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestKind::ProxyWasm => "cdn",
            RequestKind::Http => "http",
            RequestKind::ExtHttp => "fetch",
        }
    }
}

/// ProxyWasm lifecycle callback ("trigger") that produced this record.
///
/// A single ProxyWasm request is driven through several host callbacks; each
/// one is handled — and logged — independently. Only set on the ProxyWasm path;
/// the HTTP paths run a single execution and leave it `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    RequestHeaders,
    ResponseHeaders,
    RequestBody,
    ResponseBody,
    Log,
}

impl Trigger {
    /// Short, stable identifier for the ProxyWasm callback (kept compact to
    /// minimise datagram size): `rqh`/`rsh` = request/response headers,
    /// `rqb`/`rsb` = request/response body, `log` = on_log.
    pub fn as_str(&self) -> &'static str {
        match self {
            Trigger::RequestHeaders => "rqh",
            Trigger::ResponseHeaders => "rsh",
            Trigger::RequestBody => "rqb",
            Trigger::ResponseBody => "rsb",
            Trigger::Log => "log",
        }
    }
}

/// Upstream service that initiated a ProxyWasm request. Derived from the
/// ProxyWasm protocol version: nginx speaks v1/v2a/v2b and the v3 yamux
/// dialect, core-proxy speaks v2/v2c/v2d. Only set on the ProxyWasm path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Initiator {
    Nginx,
    CoreProxy,
}

impl Initiator {
    /// Stable identifier, suitable for a log field.
    pub fn as_str(&self) -> &'static str {
        match self {
            Initiator::Nginx => "nginx",
            Initiator::CoreProxy => "core-proxy",
        }
    }
}

/// One log record describing the processing of a single request.
///
/// `traceparent` and `status_code` are the primary fields; the rest are
/// best-effort context that may be absent on early-return paths (e.g. an
/// unknown application is rejected before its config — and thus its ids — are
/// known).
#[derive(Debug, Clone)]
pub struct AccessLog {
    /// W3C trace-context id used to correlate the request across systems.
    pub traceparent: String,
    /// Execution path that handled the request.
    pub kind: RequestKind,
    /// Final HTTP status code returned to the client.
    pub status_code: u16,
    /// `X-CDN-Internal-Status` diagnostic code (range 3000–3999); `0` when the
    /// request completed without an internal error.
    pub internal_status_code: u16,
    /// Edge application id, once resolved.
    pub app_id: Option<u64>,
    /// Edge application name, once resolved.
    pub app_name: Option<String>,
    /// Owning customer id, once resolved.
    pub client_id: Option<u64>,
    /// Request HTTP method, when cheaply available.
    pub method: Option<String>,
    /// Request URI, when cheaply available.
    pub uri: Option<String>,
    /// Wall-clock time spent executing the WASM app, when the app ran.
    pub duration: Option<Duration>,
    /// ProxyWasm lifecycle callback that produced this record (ProxyWasm only).
    pub trigger: Option<Trigger>,
    /// Upstream service that initiated the request (ProxyWasm only).
    pub initiator: Option<Initiator>,
}

impl AccessLog {
    /// Start a record for a request of the given `kind`, keyed by `traceparent`.
    /// Remaining fields are filled in as they become known during processing.
    pub fn new(traceparent: String, kind: RequestKind) -> Self {
        Self {
            traceparent,
            kind,
            status_code: 0,
            internal_status_code: 0,
            app_id: None,
            app_name: None,
            client_id: None,
            method: None,
            uri: None,
            duration: None,
            trigger: None,
            initiator: None,
        }
    }

    /// A request is considered successful when no internal error code was set
    /// (client errors such as 404/429 are not internal failures).
    pub fn success(&self) -> bool {
        self.internal_status_code == 0
    }
}

/// Sink for [`AccessLog`] records.
///
/// Implementations must be non-blocking / fire-and-forget: `log` is called on
/// the request hot path and must not await or block on I/O. The planned UDP
/// appender simply sends a datagram (or hands off to a background task).
pub trait AccessLogSender: Send + Sync {
    /// Record one completed request. Must not block.
    fn log(&self, record: AccessLog);
}

/// A [`AccessLogSender`] that discards every record. Used as the default until
/// a concrete appender is configured.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAccessLogSender;

impl AccessLogSender for NoopAccessLogSender {
    fn log(&self, _record: AccessLog) {}
}
