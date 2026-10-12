use crate::Store;
use reactor::gcore::fastedge::key_value::{Error, Value};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{AsyncCommands, AsyncIter, RedisError, RetryMethod};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Fail-fast timeouts for the KV-store Redis connection. Redis sits on the
/// request hot path, so a slow/unreachable Redis must surface as a quick error
/// rather than parking the calling task: a stalled dependency with no bound
/// lets in-flight requests pile up until the node collapses. Set explicitly so
/// a redis-crate bump can't silently change them.
const REDIS_RESPONSE_TIMEOUT: Duration = Duration::from_millis(100);
const REDIS_CONNECTION_TIMEOUT: Duration = Duration::from_millis(100);
const REDIS_NUMBER_OF_RETRIES: usize = 2;
const REDIS_MAX_RECONNECT_DELAY: Duration = Duration::from_millis(100);

/// Retry budget for a single read command (on top of connection-manager
/// reconnection above). Kept small and fixed for the same reason as the
/// timeouts: Redis is on the request hot path, so a retry loop must have a
/// bounded worst case rather than let a stalled dependency pile up requests.
const REDIS_READ_RETRIES: usize = 2;
const REDIS_READ_RETRY_BASE_DELAY: Duration = Duration::from_millis(10);
const REDIS_READ_RETRY_MAX_DELAY: Duration = Duration::from_millis(50);

/// Backoff applied between attempts when the server asked us to wait.
/// Saturating throughout, so a larger retry budget can never overflow the
/// shift or the multiplication into a panic.
fn redis_read_retry_delay(attempt: usize) -> Duration {
    let scale = 1u32
        .checked_shl(attempt.saturating_sub(1) as u32)
        .unwrap_or(u32::MAX);
    REDIS_READ_RETRY_BASE_DELAY
        .saturating_mul(scale)
        .min(REDIS_READ_RETRY_MAX_DELAY)
}

/// How long to wait before re-attempting a failed read, or `None` when the
/// failure must not be retried at all. `attempt` is the 1-based number of the
/// attempt about to be made, used to scale the backoff.
///
/// Allowlist rather than denylist: [`RetryMethod`] is `#[non_exhaustive]`, so a
/// redis-crate bump can introduce a variant, and defaulting those to "retry"
/// would silently spend the budget on failures we have never classified.
///
/// The cluster redirect methods (`MovedRedirect`, `AskRedirect`,
/// `RefreshSlotsAndRetry`, `ReconnectFromInitialConnections`) are deliberately
/// excluded: this store speaks to Redis through a plain `ConnectionManager`,
/// not the cluster API, so nothing here can follow a redirect — a bare retry
/// would just re-send to the same node and collect the same error.
fn retry_backoff(error: &RedisError, attempt: usize) -> Option<Duration> {
    match error.retry_method() {
        // Nothing to back off from: the command never reached a working
        // server, so there is no load to relieve and sleeping would only add
        // latency to the request. `Reconnect` means the socket is gone or the
        // stream desynced — re-running the op picks the next pooled connection
        // while `ConnectionManager` re-establishes its own socket underneath
        // (with its own capped backoff), so the next attempt already gets a
        // fresh channel.
        RetryMethod::RetryImmediately | RetryMethod::Reconnect => Some(Duration::ZERO),
        // The server is reachable but told us it cannot serve yet (`LOADING`,
        // `TRYAGAIN`). Here the pause is the point: it keeps a struggling node
        // from being hammered by every in-flight request at once.
        RetryMethod::WaitAndRetry => Some(redis_read_retry_delay(attempt)),
        _ => None,
    }
}

/// Whether the server rejected the command in a way that can never succeed on
/// retry, because the guest's own request was at fault — e.g. `WRONGTYPE` when
/// `get` addresses a key holding a sorted set, or `NOPERM`/`ERR` for a command
/// the guest may not run.
///
/// Such a rejection says nothing about node or Redis health, so it must not
/// raise a node-level warning; it is handed back to the guest instead.
///
/// Distinct from `!should_retry`: a `MOVED` reply or a failed authentication is
/// equally pointless to retry, but it is the node's problem, not the app's, so
/// it stays an opaque internal error and still warns.
///
/// Only failures the server itself answered are classified: [`RedisError::code`]
/// is `None` for I/O, timeout and client-config failures.
fn is_guest_fault(error: &RedisError) -> bool {
    matches!(error.retry_method(), RetryMethod::NoRetry) && error.code().is_some()
}

/// The server's own code and detail, for handing back to the guest.
fn guest_fault_message(error: &RedisError) -> String {
    let code = error.code().unwrap_or_default();
    match error.detail() {
        Some(detail) => format!("{code}: {detail}"),
        None => code.to_string(),
    }
}

/// Map a failed read onto the error the guest sees.
///
/// A guest-caused rejection is returned verbatim as [`Error::Other`] so the app
/// can fix its request, and is only traced at debug level. Anything else is
/// opaque to the guest and is reported by `log_unexpected`, which each caller
/// supplies so the warning keeps that command's own fields.
fn read_error(error: RedisError, log_unexpected: impl FnOnce(&RedisError)) -> Error {
    if is_guest_fault(&error) {
        tracing::debug!(cause = ?error, "kv-store: redis rejected read");
        return Error::Other(guest_fault_message(&error));
    }
    log_unexpected(&error);
    Error::InternalError
}

/// Retry a read command up to `REDIS_READ_RETRIES` times with capped
/// exponential backoff. `op` is re-invoked from scratch on each attempt so it
/// can pick a fresh pooled connection (used by `get`/`zrange_by_score`/
/// `bf_exists`; `scan`/`zscan` retry the same connection since their iterator
/// borrows it across the loop). Each retry is reported to the installed
/// [`crate::ReadRetryObserver`] under `command`.
///
/// Only failures [`retry_backoff`] accepts are attempted again, and only those
/// that ask for it sleep first; deterministic failures (a `WRONGTYPE` key, a
/// `MOVED` slot) return on the first error rather than spending the budget on a
/// result that cannot change.
async fn retry_read<T, F, Fut>(command: &'static str, mut op: F) -> Result<T, ::redis::RedisError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ::redis::RedisError>>,
{
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if attempt >= REDIS_READ_RETRIES {
                    return Err(error);
                }
                let Some(delay) = retry_backoff(&error, attempt + 1) else {
                    return Err(error);
                };
                attempt += 1;
                crate::note_read_retry(command);
                tracing::debug!(attempt, cause = ?error, "kv-store: redis read retry");
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

/// Build the fail-fast connection-manager config for KV-store Redis connections.
fn connection_manager_config() -> ConnectionManagerConfig {
    ConnectionManagerConfig::new()
        .set_response_timeout(Some(REDIS_RESPONSE_TIMEOUT))
        .set_connection_timeout(Some(REDIS_CONNECTION_TIMEOUT))
        .set_number_of_retries(REDIS_NUMBER_OF_RETRIES)
        .set_max_delay(REDIS_MAX_RECONNECT_DELAY)
}

#[derive(Clone)]
pub struct RedisStore {
    /// Pool of multiplexed connections. Each `ConnectionManager` owns its own
    /// socket and background driver task, so spreading commands across the pool
    /// round-robin keeps a burst from serializing behind a single connection
    /// (which would push tail latency past the response timeout). Wrapped in
    /// `Arc` so cloning a `RedisStore` shares the same pool and cursor.
    conns: Arc<Vec<ConnectionManager>>,
    next: Arc<AtomicUsize>,
}

impl RedisStore {
    /// Open a store backed by a pool of `ConnectionManager`s. Each connection
    /// holds a multiplexed connection and transparently reconnects with
    /// exponential backoff when the underlying socket dies (e.g. broken pipe on
    /// Redis restart). The command that hits the dead socket still surfaces as
    /// an error, but follow-up calls land on a freshly re-established
    /// connection. `pool_size` is clamped to at least 1.
    pub async fn open(params: &str, pool_size: usize) -> Result<Self, Error> {
        let pool_size = pool_size.max(1);
        let client = ::redis::Client::open(params).map_err(|error| {
            tracing::warn!(error = ?error, "kv-store: redis open");
            Error::InternalError
        })?;
        let mut conns = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let conn =
                ConnectionManager::new_with_config(client.clone(), connection_manager_config())
                    .await
                    .map_err(|error| {
                        tracing::warn!(error = ?error, "kv-store: redis open");
                        Error::InternalError
                    })?;
            conns.push(conn);
        }
        Ok(Self {
            conns: Arc::new(conns),
            next: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Pick the next connection from the pool (round-robin). Clones are cheap:
    /// `ConnectionManager` is internally reference-counted and shares its
    /// socket, so this just hands back another handle to a pooled connection.
    fn conn(&self) -> ConnectionManager {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.conns.len();
        self.conns[idx].clone()
    }
}

#[async_trait::async_trait]
impl Store for RedisStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, Error> {
        retry_read(crate::CMD_GET, || async { self.conn().get(key).await })
            .await
            .map_err(|error| {
                read_error(error, |error| {
                    tracing::warn!(cause = ?error, key, "kv-store: redis get");
                })
            })
    }

    async fn zrange_by_score(
        &self,
        key: &str,
        min: f64,
        max: f64,
    ) -> Result<Vec<(Value, f64)>, Error> {
        retry_read(crate::CMD_ZRANGE_BY_SCORE, || async {
            self.conn().zrangebyscore_withscores(key, min, max).await
        })
        .await
        .map_err(|error| {
            read_error(error, |error| {
                tracing::warn!(cause = ?error, key, min, max, "kv-store: redis zrangebyscore");
            })
        })
    }

    async fn scan(&self, pattern: &str) -> Result<Vec<String>, Error> {
        let mut conn = self.conn();
        let mut attempt = 0;
        let mut it = loop {
            match conn.scan_match(pattern).await {
                Ok(it) => break it,
                Err(error) => {
                    let backoff = if attempt < REDIS_READ_RETRIES {
                        retry_backoff(&error, attempt + 1)
                    } else {
                        None
                    };
                    let Some(delay) = backoff else {
                        return Err(read_error(error, |error| {
                            tracing::warn!(cause = ?error, pattern, "kv-store: redis scan_match");
                        }));
                    };
                    attempt += 1;
                    crate::note_read_retry(crate::CMD_SCAN);
                    tracing::debug!(attempt, cause = ?error, pattern, "kv-store: redis scan_match retry");
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        };
        let mut ret = vec![];
        while let Some(element) = it.next_item().await {
            ret.push(element.map_err(|error| {
                read_error(error, |error| {
                    tracing::warn!(cause = ?error, pattern, "kv-store: redis scan_match: item");
                })
            })?);
        }
        Ok(ret)
    }

    async fn zscan(&self, key: &str, pattern: &str) -> Result<Vec<(Value, f64)>, Error> {
        let mut conn = self.conn();
        let mut attempt = 0;
        let mut it: AsyncIter<(Value, f64)> = loop {
            match conn.zscan_match(key, pattern).await {
                Ok(it) => break it,
                Err(error) => {
                    let backoff = if attempt < REDIS_READ_RETRIES {
                        retry_backoff(&error, attempt + 1)
                    } else {
                        None
                    };
                    let Some(delay) = backoff else {
                        return Err(read_error(error, |error| {
                            tracing::warn!(cause = ?error, key, pattern, "kv-store: redis zscan_match");
                        }));
                    };
                    attempt += 1;
                    crate::note_read_retry(crate::CMD_ZSCAN);
                    tracing::debug!(attempt, cause = ?error, key, pattern, "kv-store: redis zscan_match retry");
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        };
        let mut ret = vec![];
        while let Some(element) = it.next_item().await {
            ret.push(element.map_err(|error| {
                read_error(error, |error| {
                    tracing::warn!(cause = ?error, key, pattern, "kv-store: redis zscan_match: item");
                })
            })?);
        }
        Ok(ret)
    }

    async fn bf_exists(&self, key: &str, item: &str) -> Result<bool, Error> {
        retry_read(crate::CMD_BF_EXISTS, || async {
            redis::cmd("BF.EXISTS")
                .arg(key)
                .arg(item)
                .query_async(&mut self.conn())
                .await
        })
        .await
        .map_err(|error| {
            read_error(error, |error| {
                tracing::warn!(cause = ?error, key, item, "kv-store: redis bf_exists");
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Build a real `RedisError` by parsing a RESP error frame, so the tests
    /// exercise the same classification path as a live server reply.
    fn server_error(frame: &str) -> RedisError {
        match ::redis::parse_redis_value(frame.as_bytes()) {
            Ok(::redis::Value::ServerError(error)) => error.into(),
            other => panic!("RESP error frame must parse as a server error: {other:?}"),
        }
    }

    #[test]
    fn wrongtype_is_a_guest_fault() {
        let error =
            server_error("-WRONGTYPE Operation against a key holding the wrong kind of value\r\n");
        assert!(is_guest_fault(&error));
        assert_eq!(
            guest_fault_message(&error),
            "WRONGTYPE: Operation against a key holding the wrong kind of value"
        );
    }

    #[test]
    fn other_permanent_server_rejections_are_guest_faults() {
        for frame in [
            "-ERR wrong number of arguments for 'get' command\r\n",
            "-NOPERM this user has no permissions to run the 'get' command\r\n",
        ] {
            assert!(is_guest_fault(&server_error(frame)), "frame: {frame}");
        }
    }

    #[test]
    fn retriable_server_errors_are_not_guest_faults() {
        // LOADING/TRYAGAIN are transient: the same command can succeed later,
        // so they must keep retrying and keep warning.
        for frame in [
            "-LOADING Redis is loading the dataset in memory\r\n",
            "-TRYAGAIN Multiple keys request during rehashing of slot\r\n",
        ] {
            let error = server_error(frame);
            assert!(!is_guest_fault(&error), "frame: {frame}");
            // The server asked us to wait, so the pause is the point.
            assert_eq!(
                retry_backoff(&error, 1),
                Some(REDIS_READ_RETRY_BASE_DELAY),
                "frame: {frame}"
            );
        }
    }

    #[test]
    fn deterministic_errors_are_not_retried() {
        for frame in [
            "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
            "-ERR wrong number of arguments for 'get' command\r\n",
            "-NOPERM this user has no permissions to run the 'get' command\r\n",
            // Cluster redirects: this store uses a plain (non-cluster) client,
            // so a bare retry can only re-collect the same reply.
            "-MOVED 3999 127.0.0.1:6381\r\n",
            "-ASK 3999 127.0.0.1:6381\r\n",
        ] {
            assert_eq!(
                retry_backoff(&server_error(frame), 1),
                None,
                "frame: {frame}"
            );
        }
    }

    #[test]
    fn cluster_redirects_are_node_problems_not_guest_faults() {
        // Not retriable, but the app did nothing wrong: must stay opaque and
        // still warn, unlike WRONGTYPE.
        let error = server_error("-MOVED 3999 127.0.0.1:6381\r\n");
        assert_eq!(retry_backoff(&error, 1), None);
        assert!(!is_guest_fault(&error));

        let mut warned = false;
        let mapped = read_error(error, |_| warned = true);
        assert!(warned);
        assert!(matches!(mapped, Error::InternalError));
    }

    #[test]
    fn connection_errors_are_retried_but_are_not_guest_faults() {
        // No server reply at all, so there is no code to hand back.
        let error = RedisError::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert!(!is_guest_fault(&error));
        // A dead socket is not congestion: reconnect and re-issue at once
        // rather than adding latency to a request already on the hot path.
        assert_eq!(retry_backoff(&error, 1), Some(Duration::ZERO));
    }

    #[test]
    fn wait_and_retry_backoff_grows_and_is_capped() {
        let error = server_error("-LOADING Redis is loading the dataset in memory\r\n");
        assert_eq!(retry_backoff(&error, 1), Some(REDIS_READ_RETRY_BASE_DELAY));
        assert_eq!(
            retry_backoff(&error, 2),
            Some(REDIS_READ_RETRY_BASE_DELAY * 2)
        );
        assert_eq!(retry_backoff(&error, 99), Some(REDIS_READ_RETRY_MAX_DELAY));
    }

    #[test]
    fn guest_fault_is_returned_verbatim_without_warning() {
        let error =
            server_error("-WRONGTYPE Operation against a key holding the wrong kind of value\r\n");
        let mut warned = false;
        let mapped = read_error(error, |_| warned = true);

        assert!(!warned, "a guest-caused rejection must not warn");
        assert!(
            matches!(mapped, Error::Other(message) if message.starts_with("WRONGTYPE: ")),
            "guest must receive the server's own message"
        );
    }

    #[test]
    fn unexpected_error_warns_and_stays_opaque() {
        let error = RedisError::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        let mut warned = false;
        let mapped = read_error(error, |_| warned = true);

        assert!(warned);
        assert!(matches!(mapped, Error::InternalError));
    }

    #[tokio::test]
    async fn retry_read_does_not_retry_a_guest_fault() {
        let attempts = AtomicUsize::new(0);
        let result: Result<(), _> = retry_read(crate::CMD_GET, || async {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err(server_error(
                "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
            ))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "a deterministic rejection must not burn the retry budget"
        );
    }

    #[tokio::test]
    async fn retry_read_does_not_retry_a_cluster_redirect() {
        let attempts = AtomicUsize::new(0);
        let result: Result<(), _> = retry_read(crate::CMD_GET, || async {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err(server_error("-MOVED 3999 127.0.0.1:6381\r\n"))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn retry_read_still_retries_transient_errors() {
        let attempts = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let result: Result<(), _> = retry_read(crate::CMD_GET, || async {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err(RedisError::from(std::io::Error::from(
                std::io::ErrorKind::ConnectionReset,
            )))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            REDIS_READ_RETRIES + 1,
            "transient failures must still use the full retry budget"
        );
        assert!(
            started.elapsed() < REDIS_READ_RETRY_BASE_DELAY,
            "a dead socket must be re-issued without sleeping, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn retry_read_sleeps_only_when_the_server_asks_to_wait() {
        let attempts = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let result: Result<(), _> = retry_read(crate::CMD_GET, || async {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err(server_error(
                "-LOADING Redis is loading the dataset in memory\r\n",
            ))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::Relaxed), REDIS_READ_RETRIES + 1);
        assert!(
            started.elapsed() >= REDIS_READ_RETRY_BASE_DELAY,
            "a loading node must be given time to recover"
        );
    }
}
