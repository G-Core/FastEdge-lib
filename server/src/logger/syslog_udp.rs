//! Access/error log sink: ships [`AccessRecord`]/[`FetchRecord`] events as
//! RFC 5424 syslog frames over UDP, with the record serialized as a JSON
//! payload in the MSG field.
//!
//! This is distinct from the WASM application log (`logger::kafka`/
//! `logger::victoria_log`) and from the billing stats stream (`stats`). It
//! records one line per request — including requests rejected before execution
//! — plus an optional line per outbound fetch.
//!
//! The sink never blocks the request path: [`SyslogUdpSink::log`] does a
//! non-blocking `try_send` into a bounded queue and drops on overflow. A
//! background task owns the socket and emits one datagram per event.

use lazy_static::lazy_static;
use prometheus::{IntCounter, IntGauge, register_int_counter, register_int_gauge};
use runtime::util::access_log::{AccessLogSink, AccessRecord, FetchRecord, Level, LogEvent};
use serde::Serialize;
use smol_str::SmolStr;
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

lazy_static! {
    static ref ACCESSLOG_QUEUE_SIZE: IntGauge = register_int_gauge!(
        "fastedge_accesslog_queue",
        "Pending records in the access-log sink queue."
    )
    .unwrap();
    static ref ACCESSLOG_DROPPED: IntCounter = register_int_counter!(
        "fastedge_accesslog_dropped",
        "Access-log records dropped (queue full or send error)."
    )
    .unwrap();
}

/// syslog APP-NAME field.
const APP_NAME: &str = "fastedge";
const MSGID_ACCESS: &str = "fe-access";
const MSGID_FETCH: &str = "fe-fetch";

/// Configuration for the syslog/UDP access-log sink.
#[derive(Debug, Clone)]
pub struct SyslogUdpConfig {
    /// Remote syslog collector address (`host:port`).
    pub target: SocketAddr,
    /// syslog facility (e.g. 16 = local0). PRI = facility * 8 + severity.
    pub facility: u8,
    /// syslog HOSTNAME field (typically the PoP/node name).
    pub hostname: SmolStr,
    /// Node location, added to every JSON payload.
    pub pop: SmolStr,
    pub region: SmolStr,
    pub country: SmolStr,
    /// Drop datagrams whose framed size exceeds this (bytes).
    pub max_payload: usize,
    /// Bounded queue capacity.
    pub queue_size: usize,
}

/// JSON payload for an access record, augmented with node location.
#[derive(Serialize)]
struct AccessEnvelope<'a> {
    #[serde(flatten)]
    rec: &'a AccessRecord,
    pop: &'a str,
    region: &'a str,
    country: &'a str,
}

/// JSON payload for a fetch record, augmented with the PoP for correlation.
#[derive(Serialize)]
struct FetchEnvelope<'a> {
    #[serde(flatten)]
    rec: &'a FetchRecord,
    pop: &'a str,
}

#[derive(Clone)]
pub struct SyslogUdpSink {
    tx: mpsc::Sender<LogEvent>,
}

impl SyslogUdpSink {
    /// Bind a local UDP socket connected to `target` and spawn the background
    /// sender. Must be called inside a tokio runtime.
    pub async fn new(config: SyslogUdpConfig) -> std::io::Result<Self> {
        // Bind an ephemeral local address matching the target's family, then
        // `connect` so the background task can use `send` (single peer).
        let bind: SocketAddr = if config.target.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let socket = UdpSocket::bind(bind).await?;
        socket.connect(config.target).await?;

        let (tx, rx) = mpsc::channel::<LogEvent>(config.queue_size);
        tokio::spawn(run(socket, config, rx));
        Ok(Self { tx })
    }
}

impl AccessLogSink for SyslogUdpSink {
    fn log(&self, event: LogEvent) {
        // Never block the request task; shed load on overflow.
        if self.tx.try_send(event).is_err() {
            ACCESSLOG_DROPPED.inc();
        }
    }
}

/// Background task: receive events, frame them, emit one datagram each.
async fn run(socket: UdpSocket, config: SyslogUdpConfig, mut rx: mpsc::Receiver<LogEvent>) {
    let pid = std::process::id();
    while let Some(event) = rx.recv().await {
        ACCESSLOG_QUEUE_SIZE.set(rx.len() as i64);

        let (severity, ts, msgid, payload) = match &event {
            LogEvent::Request(rec) => {
                let env = AccessEnvelope {
                    rec,
                    pop: &config.pop,
                    region: &config.region,
                    country: &config.country,
                };
                (
                    severity(rec.level),
                    rec.ts.clone(),
                    MSGID_ACCESS,
                    serde_json::to_string(&env),
                )
            }
            LogEvent::Fetch(rec) => {
                let env = FetchEnvelope {
                    rec,
                    pop: &config.pop,
                };
                (
                    severity(rec.level),
                    rec.ts.clone(),
                    MSGID_FETCH,
                    serde_json::to_string(&env),
                )
            }
        };

        let payload = match payload {
            Ok(p) => p,
            Err(error) => {
                tracing::warn!(cause=?error, "serializing access-log record");
                ACCESSLOG_DROPPED.inc();
                continue;
            }
        };

        let pri = (config.facility as u16) * 8 + severity as u16;
        // RFC 5424: <PRI>VERSION TS HOSTNAME APP-NAME PROCID MSGID SD MSG
        let frame = format!(
            "<{pri}>1 {ts} {host} {APP_NAME} {pid} {msgid} - {payload}",
            host = config.hostname,
        );

        if frame.len() > config.max_payload {
            tracing::warn!(
                len = frame.len(),
                max = config.max_payload,
                "access-log datagram exceeds max_payload; dropping"
            );
            ACCESSLOG_DROPPED.inc();
            continue;
        }

        if let Err(error) = socket.send(frame.as_bytes()).await {
            tracing::warn!(cause=?error, "sending access-log datagram");
            ACCESSLOG_DROPPED.inc();
        }
    }
    tracing::debug!("access-log sink task exiting");
}

/// Map a record level onto a syslog severity.
fn severity(level: Level) -> u8 {
    match level {
        Level::Info => 6,    // Informational
        Level::Warning => 4, // Warning
        Level::Error => 3,   // Error
    }
}
