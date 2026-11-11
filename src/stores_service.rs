//! Client for a local storage service (the one `@gcoredev/fastedge-test` runs), which
//! emulates the edge cache and Edge KV. Opt-in: used only when `FASTEDGE_STORES_SERVICE`
//! is set, as `<base-url>,<token>[,<dc>]`, e.g. `http://127.0.0.1:4100,abc,local`.
//! Without it fastedge-run keeps its in-memory cache and `redis://` KV stores.
//!
//! Every operation is one `POST <base-url>/v1/<cache|kv>` with a JSON body
//! (`{"op": .., ..}`) and a bearer token; the reply is `{"ok": true, "value": ..}` or
//! `{"ok": false, "error": "<code>"}`. Binary values travel as base64. All semantics
//! (TTL, `incr`, the clock, glob matching) live in the service.

use base64::Engine;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

pub const ENV: &str = "FASTEDGE_STORES_SERVICE";

/// Shorter than the service's own deadline, so the service gives up first.
const TIMEOUT: Duration = Duration::from_secs(4);

pub struct StoresService {
    client: Client<HttpConnector, Full<Bytes>>,
    base: String,
    token: String,
    pub dc: String,
}

/// Service error code, as sent in `{"ok": false, "error": ..}`, or `None` for a transport failure.
pub type ServiceError = Option<String>;

impl StoresService {
    pub fn from_env() -> Option<Arc<Self>> {
        Self::parse(&std::env::var(ENV).ok()?)
    }

    pub(crate) fn parse(spec: &str) -> Option<Arc<Self>> {
        let mut parts = spec.splitn(3, ',');
        let base = parts.next()?.trim().trim_end_matches('/').to_string();
        let token = parts.next()?.trim().to_string();
        if base.is_empty() || token.is_empty() {
            return None;
        }
        let dc = parts
            .next()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or("local");
        let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build_http();
        Some(Arc::new(Self {
            client,
            base,
            token,
            dc: dc.to_string(),
        }))
    }

    /// One call. No retry: a mutation may have landed before a failure.
    pub async fn call(&self, kind: &str, body: Value) -> Result<Value, ServiceError> {
        let request = hyper::Request::post(format!("{}/v1/{kind}", self.base))
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .body(Full::new(body.to_string().into()))
            .map_err(|_| None)?;
        let bytes = tokio::time::timeout(TIMEOUT, async {
            let response = self.client.request(request).await.ok()?;
            let status = response.status();
            let bytes = response.into_body().collect().await.ok()?.to_bytes();
            // The service answers operation errors with a JSON body; anything else is transport.
            (status.is_success() || status.is_client_error()).then_some(bytes)
        })
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            tracing::warn!(kind, "stores service: no reply");
            None
        })?;
        decode_reply(&bytes)
    }
}

fn decode_reply(bytes: &[u8]) -> Result<Value, ServiceError> {
    let mut reply: Value = serde_json::from_slice(bytes).map_err(|_| None)?;
    match reply["ok"].as_bool() {
        Some(true) => Ok(reply["value"].take()),
        Some(false) => Err(Some(
            reply["error"]
                .as_str()
                .unwrap_or("internal-error")
                .to_string(),
        )),
        None => Err(None),
    }
}

pub fn b64(value: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(value)
}

pub fn unb64(value: &Value) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(value.as_str()?)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec() {
        let s = StoresService::parse("http://127.0.0.1:4100/,tok").unwrap();
        assert_eq!(
            (s.base.as_str(), s.token.as_str(), s.dc.as_str()),
            ("http://127.0.0.1:4100", "tok", "local")
        );
        assert_eq!(StoresService::parse("http://h:1,t,ams").unwrap().dc, "ams");
        assert!(StoresService::parse("http://h:1").is_none());
        assert!(StoresService::parse("http://h:1,").is_none());
    }

    #[test]
    fn decode() {
        assert_eq!(
            decode_reply(br#"{"ok":true,"value":5}"#),
            Ok(Value::from(5))
        );
        assert_eq!(decode_reply(br#"{"ok":true}"#), Ok(Value::Null));
        assert_eq!(
            decode_reply(br#"{"ok":false,"error":"bad-argument"}"#),
            Err(Some("bad-argument".into()))
        );
        assert_eq!(decode_reply(b"nope"), Err(None));
    }

    #[test]
    fn base64_round_trip() {
        assert_eq!(
            unb64(&Value::from(b64(b"\x00hi"))),
            Some(b"\x00hi".to_vec())
        );
        assert_eq!(unb64(&Value::Null), None);
    }
}
