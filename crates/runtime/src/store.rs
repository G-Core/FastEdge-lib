use crate::epoch_grace::{EpochGrace, EpochGraceEvent};
use crate::limiter::ProxyLimiter;
use crate::logger::Logger;
use crate::registry::CachedGraphRegistry;
use crate::util::stats::StatsVisitor;
use crate::{DEFAULT_EPOCH_TICK_INTERVAL, Data, Wasi, WasiVersion};
use anyhow::Result;
use secret::SecretStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::HashMap,
    fmt::{Debug, Formatter},
    ops::{Deref, DerefMut},
};
use tracing::{debug, instrument, trace};
use utils::{Dictionary, Utils};
use wasmtime::UpdateDeadline;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_nn::wit::WasiNnCtx;

/// Default extra wall-clock budget (in ms) added to the tokio timeout that
/// wraps wasm execution. Acts as a soft outer bound that must comfortably
/// exceed the epoch budget once host I/O credit (e.g. external HTTP) extends
/// the epoch deadline.
pub const DEFAULT_MAX_EXTERNAL_DURATION_MS: u64 = 30_000;

/// A `Store` holds the runtime state of a app instance.
///
/// `Store` lives only for the lifetime of a single app invocation.
pub struct Store<T: 'static> {
    inner: wasmtime::Store<Data<T>>,
}

impl<T> AsRef<wasmtime::Store<Data<T>>> for Store<T> {
    fn as_ref(&self) -> &wasmtime::Store<Data<T>> {
        &self.inner
    }
}

impl<T> AsMut<wasmtime::Store<Data<T>>> for Store<T> {
    fn as_mut(&mut self) -> &mut wasmtime::Store<Data<T>> {
        &mut self.inner
    }
}

impl<T: 'static> wasmtime::AsContext for Store<T> {
    type Data = Data<T>;

    fn as_context(&self) -> wasmtime::StoreContext<'_, Self::Data> {
        self.inner.as_context()
    }
}

impl<T> wasmtime::AsContextMut for Store<T> {
    fn as_context_mut(&mut self) -> wasmtime::StoreContextMut<'_, Self::Data> {
        self.inner.as_context_mut()
    }
}

impl<T> Store<T> {
    pub fn memory_used(&self) -> usize {
        self.inner.data().store_limits.allocated
    }

    /// Returns `true` if a memory growth request was denied because the
    /// desired size exceeded the configured limit (out-of-memory). Notably
    /// covers instantiation failures where a module's declared minimum memory
    /// already exceeds the limit.
    pub fn is_oom(&self) -> bool {
        self.inner.data().store_limits.oom
    }

    /// (Re-)arm the epoch deadline to `ticks` from now, resetting the stall
    /// grace baseline to the current thread. Use this instead of
    /// `set_epoch_deadline` directly; see [`crate::epoch_grace`].
    pub fn arm_epoch_deadline(&mut self, ticks: u64) {
        arm_epoch_deadline(&mut self.inner, ticks);
    }
}

/// Error indicating a wasm operation failed because it required more memory
/// than the app's configured limit. Surfaced as a typed error (rather than the
/// opaque wasmtime message) so callers can classify the failure as
/// out-of-memory. The originating wasmtime error is preserved as the source.
///
/// Covers the instantiation-time case where a module's declared minimum memory
/// already exceeds the limit (wasmtime reports "memory minimum size of N pages
/// exceeds memory limits").
#[derive(Debug)]
pub struct OutOfMemory(pub anyhow::Error);

impl std::fmt::Display for OutOfMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "out of memory: {}", self.0)
    }
}

impl std::error::Error for OutOfMemory {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

impl<T> Deref for Store<T> {
    type Target = wasmtime::Store<Data<T>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> DerefMut for Store<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub trait HasStats {
    fn get_stats(&self) -> Arc<dyn StatsVisitor>;
}

/// A builder interface for configuring a new [`Store`].
///
/// A new [`StoreBuilder`] can be obtained with [`crate::Engine::store_builder`].
#[derive(Clone)]
pub struct StoreBuilder {
    engine: wasmtime::Engine,
    max_duration: u64,
    store_limits: ProxyLimiter,
    env: Vec<(String, String)>,
    logger: Option<Logger>,
    version: WasiVersion,
    properties: HashMap<String, String>,
    registry: CachedGraphRegistry,
    secret_store: SecretStore,
    key_value_store: key_value_store::Builder,
    dictionary: Dictionary,
    cache_backend: Option<Arc<dyn cache::CacheBackend>>,
    epoch_pause_ms: Option<Arc<AtomicU64>>,
    epoch_exclude_http_wait: bool,
    max_external_duration_ms: u64,
}

impl StoreBuilder {
    // Called by Engine::store_builder.
    pub(crate) fn new(engine: wasmtime::Engine, version: WasiVersion) -> Self {
        Self {
            engine,
            max_duration: 100,
            store_limits: ProxyLimiter::default(),
            env: vec![],
            logger: None,
            version,
            properties: Default::default(),
            registry: CachedGraphRegistry::new(),
            secret_store: Default::default(),
            key_value_store: key_value_store::Builder::default(),
            dictionary: Default::default(),
            cache_backend: None,
            epoch_pause_ms: None,
            epoch_exclude_http_wait: false,
            max_external_duration_ms: DEFAULT_MAX_EXTERNAL_DURATION_MS,
        }
    }

    /// Sets a maximum memory allocation limit.
    ///
    /// See [`wasmtime::ResourceLimiter::memory_growing`] (`maximum`) for
    /// details on how this limit is enforced.
    pub fn max_memory_size(self, max_memory_size: usize) -> Self {
        trace!(
            "set max memory size: {:.0?}",
            bytesize::ByteSize::b(max_memory_size as u64)
        );
        let store_limits = ProxyLimiter::new(max_memory_size);
        Self {
            store_limits,
            ..self
        }
    }

    /// Sets max number of epoch ticks
    ///
    /// See documentation on
    /// [`Config::epoch_interruption()`](crate::Config::epoch_interruption)
    /// for an introduction to epoch-based interruption.
    pub fn max_epoch_ticks(self, max_duration: u64) -> Self {
        Self {
            max_duration,
            ..self
        }
    }

    /// Set ENV variables
    pub fn set_env(self, env: &[(impl AsRef<str>, impl AsRef<str>)]) -> Self {
        let env = env
            .iter()
            .map(|(k, v)| (k.as_ref().to_owned(), v.as_ref().to_owned()))
            .collect();
        Self { env, ..self }
    }

    /// Set backend proxy address
    pub fn logger(self, logger: Logger) -> Self {
        Self {
            logger: Some(logger),
            ..self
        }
    }

    pub fn with_properties(self, properties: HashMap<String, String>) -> Self {
        Self { properties, ..self }
    }

    /// Set secret store
    pub fn secret_store(self, secret_store: SecretStore) -> Self {
        Self {
            secret_store,
            ..self
        }
    }

    /// Set key value store
    pub fn key_value_store(self, key_value_store: key_value_store::Builder) -> Self {
        Self {
            key_value_store,
            ..self
        }
    }

    /// Set key value dictionary
    pub fn dictionary(self, dictionary: Dictionary) -> Self {
        Self { dictionary, ..self }
    }

    /// Set the cache backend implementation. If not set, the cache
    /// host functions will return `AccessDenied` for every operation
    /// (see [`cache::NoCacheBackend`]).
    pub fn cache_backend(self, cache_backend: Arc<dyn cache::CacheBackend>) -> Self {
        Self {
            cache_backend: Some(cache_backend),
            ..self
        }
    }

    /// Provide the shared counter that host functions use to "pause" the
    /// epoch deadline. Each ms accumulated here grants the guest extra
    /// ticks when the epoch deadline next fires. If unset, the store
    /// falls back to a private counter (no host call can deposit credit).
    pub fn epoch_pause_ms(self, counter: Arc<AtomicU64>) -> Self {
        Self {
            epoch_pause_ms: Some(counter),
            ..self
        }
    }

    /// Enable/disable excluding external HTTP wait time from the epoch deadline (refund ticks based on elapsed wall-clock time).
    pub fn epoch_exclude_http_wait(self, enabled: bool) -> Self {
        Self {
            epoch_exclude_http_wait: enabled,
            ..self
        }
    }

    /// Wall-clock slack (in ms) added to the tokio timeout that wraps
    /// wasm execution. Must comfortably exceed the worst-case total time
    /// spent in epoch-paused host calls so the tokio bound does not fire
    /// before the epoch one.
    pub fn max_external_duration_ms(self, ms: u64) -> Self {
        Self {
            max_external_duration_ms: ms,
            ..self
        }
    }

    pub fn make_wasi_nn(&self) -> Result<WasiNnCtx> {
        // initialize application specific graph
        let backends: Vec<&str> = self
            .env
            .iter()
            .find(|(k, _)| k == "FASTEDGE_NN_BACKENDS")
            .map(|(_, v)| v.split(",").collect())
            .unwrap_or_default();

        debug!("wasi-nn backends: {:?}", backends);

        let preload_graphs: Vec<(String, String)> = backends
            .into_iter()
            .flat_map(|b| {
                let entry = self.env.iter().filter_map(|(k, v)| {
                    if k.starts_with(&format!("FASTEDGE_NN_{}", b.to_ascii_uppercase())) {
                        Some((b.to_string(), v.to_string()))
                    } else {
                        None
                    }
                });
                entry
            })
            .collect();
        debug!("preload graphs: {:?}", preload_graphs);

        let (backends, registry) = self.registry.preload_graphs(preload_graphs)?;

        Ok(WasiNnCtx::new(backends, registry))
    }

    /// Builds a [`Store`] from this builder with `Default` WasiCtxBuilder
    pub fn build<T>(self, inner: T) -> Result<Store<T>>
    where
        T: HasStats,
    {
        let wasi_builder = WasiCtxBuilder::new();
        self.build_with_wasi(wasi_builder, inner)
    }

    /// Builds a [`Store`] from this builder with given WasiCtxBuilder.
    #[instrument(skip(self, wasi_ctx_builder, inner), err, level = "debug")]
    pub(crate) fn build_with_wasi<T>(
        self,
        mut wasi_ctx_builder: WasiCtxBuilder,
        inner: T,
    ) -> Result<Store<T>>
    where
        T: HasStats,
    {
        let table = ResourceTable::new();

        wasi_ctx_builder.envs(&self.env);
        let wasi_nn = self.make_wasi_nn()?;

        let logger = if let Some(mut logger) = self.logger {
            logger.extend(self.properties);
            wasi_ctx_builder.stdout(logger.clone());
            Some(logger)
        } else {
            if cfg!(debug_assertions) {
                wasi_ctx_builder.inherit_stdout();
                wasi_ctx_builder.inherit_stderr();
            }
            None
        };

        let wasi = match self.version {
            WasiVersion::Preview1 => Wasi::Preview1(wasi_ctx_builder.build_p1()),
            WasiVersion::Preview2 => Wasi::Preview2(wasi_ctx_builder.build()),
        };

        let key_value_store = self.key_value_store.build(inner.get_stats());
        let utils = Utils::new(inner.get_stats());
        let cache_impl = cache::CacheImpl::new(
            self.cache_backend
                .unwrap_or_else(|| Arc::new(cache::NoCacheBackend)),
        );
        let epoch_pause_ms = self
            .epoch_pause_ms
            .unwrap_or_else(|| Arc::new(AtomicU64::new(0)));

        let mut inner = wasmtime::Store::new(
            &self.engine,
            Data {
                inner,
                wasi,
                wasi_nn,
                store_limits: self.store_limits,
                // Saturating arithmetic: configuration values are `u64` and could, in
                // pathological cases, overflow when combined. Wrapping would silently
                // produce an unexpectedly tiny timeout in release builds, so cap at
                // `u64::MAX` instead.
                timeout: self
                    .max_duration
                    .saturating_add(1)
                    .saturating_mul(DEFAULT_EPOCH_TICK_INTERVAL)
                    .saturating_add(self.max_external_duration_ms),
                table,
                logger,
                http: WasiHttpCtx::new(),
                secret_store: self.secret_store,
                key_value_store,
                dictionary: self.dictionary,
                utils,
                cache: cache_impl,
                epoch_pause_ms: epoch_pause_ms.clone(),
                pause_epoch_timeout_for_external_http: self.epoch_exclude_http_wait,
                epoch_grace: EpochGrace::new(DEFAULT_EPOCH_TICK_INTERVAL),
                _live_instance: crate::instances::LiveInstanceGuard::new(),
            },
        );
        inner.limiter(|state| &mut state.store_limits);
        // allow max number of epoch ticks (1 tick = 10 ms)
        arm_epoch_deadline(&mut inner, self.max_duration);
        // When the deadline fires: first consume any host-call credit
        // accumulated by `epoch_pause_ms` to extend it; failing that, extend
        // once more if the executing thread was parked rather than running
        // (see `epoch_grace`); otherwise trap as before.
        inner.epoch_deadline_callback(move |mut ctx| {
            let credit_ms = epoch_pause_ms.swap(0, Ordering::Relaxed);
            epoch_deadline_decision(credit_ms, &mut ctx.data_mut().epoch_grace)
        });
        Ok(Store { inner })
    }
}

/// Arm the epoch deadline to `ticks` and record the baseline the stall grace
/// measures against. Every place that sets a deadline must go through here,
/// or the grace has no baseline and denies.
pub(crate) fn arm_epoch_deadline<T>(store: &mut wasmtime::Store<Data<T>>, ticks: u64) {
    store.data_mut().epoch_grace.arm(ticks);
    store.set_epoch_deadline(ticks);
}

/// The epoch-deadline callback's decision, factored out so tests exercise the
/// production logic rather than a copy of it.
///
/// Order matters: host-call credit is the *earned* extension (the guest was
/// waiting on external I/O the host chose not to charge) and takes precedence;
/// the stall grace is only consulted when there is none.
pub(crate) fn epoch_deadline_decision(
    credit_ms: u64,
    grace: &mut EpochGrace,
) -> Result<UpdateDeadline> {
    if let Some(ticks) = epoch_credit_ticks(credit_ms) {
        // The credited extension starts a new deadline segment; rebase the
        // stall grace on it so the next no-credit hit measures CPU against
        // this segment, not the long-expired one. Grant allowance is kept.
        grace.rebase(ticks);
        return Ok(UpdateDeadline::Continue(ticks));
    }
    let event = grace.on_deadline();
    crate::util::metrics::report_epoch_grace(event);
    match event {
        EpochGraceEvent::Granted {
            ticks,
            cpu_used_ms,
            wall_ms,
        } => {
            debug!(
                ticks,
                cpu_used_ms,
                wall_ms,
                grants = grace.grants(),
                "epoch deadline reached while parked; extending"
            );
            Ok(UpdateDeadline::Continue(ticks))
        }
        EpochGraceEvent::Denied(reason) => {
            debug!(?reason, "epoch deadline reached; trapping");
            Err(anyhow::Error::new(wasmtime::Trap::Interrupt))
        }
    }
}

/// Compute how many epoch ticks to refund given accumulated host-call
/// credit (in ms).
///
/// Returns `None` when no credit has been deposited — the epoch deadline
/// should fire as an interrupt. Otherwise returns the number of additional
/// ticks to grant, rounded **up** so any sub-tick remainder still counts
/// as a full tick. Without ceil-div, e.g. 15 ms of credit would only
/// refund 1 tick (10 ms) and the next epoch deadline could still trap
/// prematurely.
fn epoch_credit_ticks(credit_ms: u64) -> Option<u64> {
    if credit_ms == 0 {
        None
    } else {
        // Ceil-div: `div_ceil` only returns 0 when the dividend is 0,
        // which is short-circuited above, so this never returns `Some(0)`.
        Some(credit_ms.div_ceil(DEFAULT_EPOCH_TICK_INTERVAL))
    }
}

impl Debug for StoreBuilder {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreBuilder")
            .field("version", &self.version)
            .field("env", &self.env)
            .field("max_duration", &self.max_duration)
            .field("store_limits", &self.store_limits)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::Duration;
    use wasmtime::{Config, Engine, Instance, Module, Store as WtStore, Trap};

    // ── unit tests: epoch_credit_ticks helper ─────────────────────────────

    #[test]
    fn epoch_credit_ticks_zero_means_trap() {
        // No host-call credit accumulated → callback must signal a trap.
        assert_eq!(epoch_credit_ticks(0), None);
    }

    #[test]
    fn epoch_credit_ticks_rounds_up() {
        // 1 tick == DEFAULT_EPOCH_TICK_INTERVAL ms (10 ms).
        // Any sub-tick remainder must round UP so the guest isn't under-refunded.
        assert_eq!(epoch_credit_ticks(1), Some(1));
        assert_eq!(epoch_credit_ticks(9), Some(1));
        assert_eq!(epoch_credit_ticks(10), Some(1));
        assert_eq!(epoch_credit_ticks(11), Some(2)); // <-- regression case
        assert_eq!(epoch_credit_ticks(15), Some(2)); // <-- regression case
        assert_eq!(epoch_credit_ticks(20), Some(2));
        assert_eq!(epoch_credit_ticks(25), Some(3));
        assert_eq!(epoch_credit_ticks(100), Some(10));
    }

    #[test]
    fn epoch_credit_ticks_handles_u64_max() {
        // Must not panic or overflow on extreme inputs.
        assert_eq!(
            epoch_credit_ticks(u64::MAX),
            Some(u64::MAX.div_ceil(DEFAULT_EPOCH_TICK_INTERVAL)),
        );
    }

    #[test]
    fn credited_extension_rebases_the_stall_grace() {
        // A credited Continue starts a new deadline segment; the grace
        // baseline must move to it, or the next no-credit hit measures CPU
        // across both segments against the stale budget.
        let mut grace = EpochGrace::new(DEFAULT_EPOCH_TICK_INTERVAL);
        grace.arm(100);
        let decision = epoch_deadline_decision(25, &mut grace).unwrap();
        assert!(matches!(decision, UpdateDeadline::Continue(3)));
        assert_eq!(grace.baseline_ticks(), Some(3));
        assert_eq!(grace.grants(), 0);
    }

    // ── integration test: end-to-end deadline extension ───────────────────
    //
    // These tests verify the same wiring used in production
    // (`set_epoch_deadline` + `epoch_deadline_callback` consuming
    // `epoch_pause_ms`) on a real wasmtime engine running a busy-loop guest.
    //
    // Without depositing credit, the deadline must fire → `Trap::Interrupt`.
    // With credit deposited (simulating a host HTTP call that paused the
    // epoch), the deadline must be extended and the guest must complete.

    /// A guest with a bounded busy loop. Counts down a mutable global so
    /// the loop body contains a backward branch (which is what triggers
    /// wasmtime's epoch deadline check).
    const SPIN_WAT: &str = r#"
        (module
            (global $i (mut i32) (i32.const 5000000))
            (func (export "spin")
                (loop $l
                    (global.set $i (i32.sub (global.get $i) (i32.const 1)))
                    (br_if $l (i32.gt_s (global.get $i) (i32.const 0))))))
    "#;

    fn make_engine() -> Engine {
        let mut cfg = Config::new();
        cfg.epoch_interruption(true);
        Engine::new(&cfg).unwrap()
    }

    /// Install the epoch-deadline callback used in production (see
    /// `StoreBuilder::build_with_wasi`) — the same `epoch_deadline_decision`,
    /// with the store data standing in for `Data::epoch_grace`.
    fn install_epoch_callback(store: &mut WtStore<EpochGrace>, epoch_pause_ms: Arc<AtomicU64>) {
        store.epoch_deadline_callback(move |mut ctx| {
            let credit_ms = epoch_pause_ms.swap(0, Ordering::Relaxed);
            epoch_deadline_decision(credit_ms, ctx.data_mut())
        });
    }

    /// Like `SPIN_WAT` but effectively unbounded (seconds of CPU), for tests
    /// that must observe a trap: with the stall grace in place a 1-tick budget
    /// can legitimately be extended a bounded number of times before the trap,
    /// and the guest must still be running when that happens.
    const LONG_SPIN_WAT: &str = r#"
        (module
            (global $i (mut i32) (i32.const 2000000000))
            (func (export "spin")
                (loop $l
                    (global.set $i (i32.sub (global.get $i) (i32.const 1)))
                    (br_if $l (i32.gt_s (global.get $i) (i32.const 0))))))
    "#;

    #[test]
    fn busy_loop_without_credit_traps_with_interrupt() {
        let engine = make_engine();
        let module = Module::new(&engine, LONG_SPIN_WAT).unwrap();
        let epoch_pause_ms = Arc::new(AtomicU64::new(0));

        // 1 ms per tick, matching the test ticker below.
        let mut store = WtStore::new(&engine, EpochGrace::new(1));
        store.set_epoch_deadline(1); // very short budget: 1 tick
        store.data_mut().arm(1);
        install_epoch_callback(&mut store, epoch_pause_ms.clone());

        // Background ticker: bump the engine epoch until the test signals stop.
        let stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let engine = engine.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    engine.increment_epoch();
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .unwrap();

        let result = spin.call(&mut store, ());

        stop.store(true, Ordering::Relaxed);
        ticker.join().unwrap();

        // The guest's busy loop must be cut short by the epoch deadline
        // because `epoch_pause_ms` was never incremented — and a *busy* guest
        // consumes CPU at wall rate, so the stall grace cannot rescue it more
        // than its bounded number of times either.
        let err = result.expect_err("guest must trap when no host-call credit is deposited");
        let trap = err.root_cause().downcast_ref::<Trap>().copied();
        assert_eq!(
            trap,
            Some(Trap::Interrupt),
            "expected Trap::Interrupt, got: {err:?}"
        );
        // No leftover credit should remain in the pause counter — the
        // callback must consume it on each fire.
        assert_eq!(epoch_pause_ms.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn busy_loop_completes_when_credit_is_deposited() {
        let engine = make_engine();
        let module = Module::new(&engine, SPIN_WAT).unwrap();
        let epoch_pause_ms = Arc::new(AtomicU64::new(0));

        // 1 ms per tick, matching the test ticker below.
        let mut store = WtStore::new(&engine, EpochGrace::new(1));
        store.set_epoch_deadline(1); // very short budget: 1 tick
        store.data_mut().arm(1);
        install_epoch_callback(&mut store, epoch_pause_ms.clone());

        // Background ticker bumps the epoch AND deposits generous credit so
        // every fired deadline is extended via `UpdateDeadline::Continue`.
        // This is the production scenario where an outbound HTTP request
        // ran for some time and the host-side timer deposited that elapsed
        // time into `epoch_pause_ms` (see `Data::with_paused_epoch`).
        let stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let engine = engine.clone();
            let stop = stop.clone();
            let credit = epoch_pause_ms.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // Deposit *before* incrementing the epoch so the
                    // callback observes non-zero credit when it fires.
                    credit.fetch_add(1_000_000, Ordering::Relaxed);
                    engine.increment_epoch();
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .unwrap();

        let result = spin.call(&mut store, ());

        stop.store(true, Ordering::Relaxed);
        ticker.join().unwrap();

        // With epoch credit being continuously deposited, the deadline
        // must always be extended — execution should reach the natural
        // end of the bounded loop without trapping.
        result.expect("guest must complete when epoch credit is deposited");
    }

    /// A worker that is parked while the wall budget runs out — no CPU
    /// consumed — must be granted the unused budget instead of trapping: the
    /// guest did no work, so it has not used anything. `thread::sleep` is a
    /// faithful model of a kernel park: wall time passes, thread CPU time does
    /// not.
    #[test]
    fn parked_thread_is_granted_grace_and_completes() {
        let engine = make_engine();
        let module = Module::new(&engine, SPIN_WAT).unwrap();
        let epoch_pause_ms = Arc::new(AtomicU64::new(0));

        // 200 ticks × 1 ms = 200 ms of budget: generous next to the few ms the
        // bounded spin needs, so a slow CI box cannot turn the grant into a
        // second, legitimate deadline hit.
        const BUDGET_TICKS: u64 = 200;
        let mut store = WtStore::new(&engine, EpochGrace::new(1));
        store.set_epoch_deadline(BUDGET_TICKS);
        store.data_mut().arm(BUDGET_TICKS);
        install_epoch_callback(&mut store, epoch_pause_ms.clone());

        let stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let engine = engine.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    engine.increment_epoch();
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .unwrap();

        // Park: the ticker blows through the entire budget while this thread
        // consumes no CPU at all.
        thread::sleep(Duration::from_millis(300));

        let result = spin.call(&mut store, ());

        stop.store(true, Ordering::Relaxed);
        ticker.join().unwrap();

        result.expect("a parked guest must be granted its unused budget, not trapped");
        assert_eq!(store.data().grants(), 1, "exactly one grace grant expected");
        assert_eq!(epoch_pause_ms.load(Ordering::Relaxed), 0);
    }

    // ── live-instance accounting ──────────────────────────────────────────

    /// No-op stats sink; `StoreBuilder::build` only needs `HasStats` to wire the
    /// key-value store and utils host state.
    struct NoStats;

    mod no_stats_impls {
        use super::NoStats;
        use crate::util::stats::{CdnPhase, ReadStats, StatsVisitor};
        use http_backend::stats::ExtRequestStats;
        use std::time::Duration;
        use utils::UserDiagStats;

        impl ReadStats for NoStats {
            fn count_kv_read(&self, _: i32) {}
            fn count_kv_byod_read(&self, _: i32) {}
            fn count_kv_read_cached(&self) {}
        }
        impl UserDiagStats for NoStats {
            fn set_user_diag(&self, _: &str) {}
        }
        impl ExtRequestStats for NoStats {
            fn observe_ext(&self, _: Duration) {}
        }
        impl StatsVisitor for NoStats {
            fn status_code(&self, _: u16) {}
            fn memory_used(&self, _: u64) {}
            fn fail_reason(&self, _: i32) {}
            fn observe(&self, _: Duration) {}
            fn get_time_elapsed(&self) -> u64 {
                0
            }
            fn get_memory_used(&self) -> u64 {
                0
            }
            fn cdn_phase(&self, _: CdnPhase) {}
        }
    }

    impl HasStats for NoStats {
        fn get_stats(&self) -> Arc<dyn StatsVisitor> {
            Arc::new(NoStats)
        }
    }

    /// End-to-end wiring check: a store built the way every executor builds it must be
    /// counted in `fastedge_wasm_instances_live` for exactly as long as it is alive, since
    /// that is the window in which it holds pooling-allocator slots.
    #[test]
    fn store_lifetime_is_counted_as_a_live_instance() {
        use crate::instances;

        let engine = make_engine();
        let before = instances::live();

        let store = StoreBuilder::new(engine.clone(), WasiVersion::Preview1)
            .build(NoStats)
            .expect("build store");
        assert_eq!(
            instances::live(),
            before + 1,
            "building a store must count a live instance"
        );

        let second = StoreBuilder::new(engine, WasiVersion::Preview1)
            .build(NoStats)
            .expect("build store");
        assert_eq!(instances::live(), before + 2);
        assert!(instances::peak() >= before + 2);

        drop(second);
        assert_eq!(instances::live(), before + 1);
        drop(store);
        assert_eq!(
            instances::live(),
            before,
            "dropping the store must release the count"
        );
    }
}
