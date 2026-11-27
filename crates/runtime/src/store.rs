use crate::limiter::ProxyLimiter;
use crate::logger::Logger;
use crate::registry::{CachedGraphRegistry, LoadedGraph};
use crate::util::stats::StatsVisitor;
use crate::{DEFAULT_EPOCH_TICK_INTERVAL, Data, HttpHooks, Wasi, WasiVersion};
use anyhow::Result;
use secret::SecretStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
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
    /// Process-wide graph cache, shared with every other builder of the engine.
    registry: CachedGraphRegistry,
    /// The app's wasi-nn graphs, resolved once by [`Self::preload_wasi_nn`] so
    /// building a `Store` only clones handles. `None` until preloaded, and a
    /// store built from a never-preloaded builder gets no wasi-nn context at
    /// all: preloading is the opt-in (proxy-wasm does not link wasi-nn).
    nn_graphs: Option<Vec<LoadedGraph>>,
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
    pub(crate) fn new(
        engine: wasmtime::Engine,
        version: WasiVersion,
        registry: CachedGraphRegistry,
    ) -> Self {
        Self {
            engine,
            max_duration: 100,
            store_limits: ProxyLimiter::default(),
            env: vec![],
            logger: None,
            version,
            properties: Default::default(),
            registry,
            nn_graphs: None,
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

    /// Resolves the app's wasi-nn graphs (from `FASTEDGE_NN_*` env, see
    /// [`crate::registry`]) now, so that every `Store` built later gets them
    /// for the price of an `Arc` clone.
    ///
    /// Call this where the executor is created — a cache miss loads a model,
    /// which takes seconds and gigabytes, and that belongs on the blocking
    /// pool, not on the request path. Loads are coalesced process-wide, so
    /// concurrent executor creations for the same app share one load.
    pub fn preload_wasi_nn(self) -> Result<Self> {
        let spec = crate::registry::preload_spec(&self.env)?;
        debug!("preload graphs: {:?}", spec);
        let nn_graphs = self.registry.load_graphs(&spec)?;
        Ok(Self {
            nn_graphs: Some(nn_graphs),
            ..self
        })
    }

    /// Whether the app's env asks for wasi-nn graphs (`FASTEDGE_NN_*`).
    /// Lets an executor factory whose engine lacks wasi-nn flag the misconfig
    /// instead of silently ignoring it.
    pub fn has_wasi_nn_env(&self) -> bool {
        // A malformed spec still asks for wasi-nn; the error surfaces at preload.
        crate::registry::preload_spec(&self.env).map_or(true, |spec| !spec.is_empty())
    }

    /// The store's wasi-nn context, or `None` for a builder that was never
    /// [`Self::preload_wasi_nn`]ed — that is the opt-in, and a store without it
    /// pays nothing here (no backends, no registry, no allocations).
    pub fn make_wasi_nn(&self) -> Option<WasiNnCtx> {
        let graphs = self.nn_graphs.as_ref()?;
        // Keep the process-wide entries alive while this app has traffic;
        // nothing else reads them between executor rebuilds.
        self.registry.mark_used(graphs);
        Some(CachedGraphRegistry::ctx_for(graphs))
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
        let wasi_nn = self.make_wasi_nn();

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
                hooks: HttpHooks::new(inner, epoch_pause_ms.clone(), self.epoch_exclude_http_wait),
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
                _live_instance: crate::instances::LiveInstanceGuard::new(),
            },
        );
        inner.limiter(|state| &mut state.store_limits);
        // allow max number of epoch ticks (1 tick = 10 ms)
        inner.set_epoch_deadline(self.max_duration);
        // When the deadline fires, consume any host-call credit accumulated by
        // `epoch_pause_ms` to extend it; if there is no credit, trap as before.
        inner.epoch_deadline_callback(epoch_deadline_callback(epoch_pause_ms, self.max_duration));
        Ok(Store { inner })
    }
}

/// The store's epoch-deadline callback, for a deadline of `max_ticks` armed
/// now: extends the deadline by the host-I/O credit deposited in
/// `epoch_pause_ms`, or traps with [`wasmtime::Trap::Interrupt`] when there is
/// none.
///
/// The callback keeps the deadline as an absolute wall-clock instant and moves
/// *that* by the credit ([`epoch_refund`]). wasmtime's
/// `UpdateDeadline::Continue(ticks)` counts from the epoch it is at when the
/// callback runs, not from the deadline that expired, so handing it the raw
/// credit would also give back however far a host wait ran past the deadline:
/// a 300 ms outbound request at the start of a 100 ms budget would leave the
/// guest 300 ms to run instead of 100. A guest spinning in wasm never yields,
/// so nothing else would catch it.
///
/// Assumes nothing else re-arms the deadline while credit is being deposited
/// (true on the HTTP paths; the ProxyWasm executor, which does re-arm between
/// phases, deposits no credit and re-arms after host waits itself).
fn epoch_deadline_callback<T>(
    epoch_pause_ms: Arc<AtomicU64>,
    max_ticks: u64,
) -> impl FnMut(wasmtime::StoreContextMut<'_, T>) -> wasmtime::Result<UpdateDeadline>
+ Send
+ Sync
+ 'static {
    let now = Instant::now();
    // An absurd budget that overflows `Instant` is as good as no deadline.
    let mut deadline = now
        .checked_add(ticks_duration(max_ticks))
        .unwrap_or_else(|| now + Duration::from_secs(365 * 24 * 3600));
    move |_ctx| {
        let credit_ms = epoch_pause_ms.swap(0, Ordering::Relaxed);
        match epoch_refund(credit_ms, deadline, Instant::now()) {
            None => Err(wasmtime::Error::new(wasmtime::Trap::Interrupt)),
            Some((ticks, moved)) => {
                deadline = moved;
                Ok(UpdateDeadline::Continue(ticks))
            }
        }
    }
}

/// Wall-clock length of `ticks` epoch ticks, saturating instead of
/// overflowing on absurd budgets.
fn ticks_duration(ticks: u64) -> Duration {
    Duration::from_millis(ticks.saturating_mul(DEFAULT_EPOCH_TICK_INTERVAL))
}

/// Refund `credit_ms` of host I/O to a guest whose epoch `deadline` has
/// fired: the deadline moves to `deadline + credit`, and the result is the
/// ticks from `now` to it, with that new deadline.
///
/// `None` (trap) when no credit was deposited, or when the moved deadline is
/// still not in the future — the guest has used up its budget even with the
/// wait left out. Ticks round **up**, so a sub-tick remainder still counts
/// (15 ms left is 2 ticks, not 1), and are never 0.
fn epoch_refund(credit_ms: u64, deadline: Instant, now: Instant) -> Option<(u64, Instant)> {
    if credit_ms == 0 {
        return None;
    }
    let moved = deadline.checked_add(Duration::from_millis(credit_ms))?;
    let left = moved.checked_duration_since(now)?;
    let left_us = u64::try_from(left.as_micros()).unwrap_or(u64::MAX);
    let ticks = left_us.div_ceil(DEFAULT_EPOCH_TICK_INTERVAL.saturating_mul(1000));
    (ticks > 0).then_some((ticks, moved))
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

    // ── unit tests: epoch_refund helper ───────────────────────────────────

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn epoch_refund_without_credit_traps() {
        let t0 = Instant::now();
        assert_eq!(epoch_refund(0, t0, t0), None);
    }

    #[test]
    fn epoch_refund_at_the_deadline_grants_the_credit() {
        // The deadline fired on time: the whole credit is still ahead.
        let t0 = Instant::now();
        assert_eq!(epoch_refund(100, t0, t0), Some((10, t0 + 100 * MS)));
    }

    #[test]
    fn epoch_refund_rounds_up() {
        // 1 tick == DEFAULT_EPOCH_TICK_INTERVAL ms (10 ms). Any sub-tick
        // remainder must round UP so the guest isn't under-refunded.
        let t0 = Instant::now();
        let ticks = |ms| epoch_refund(ms, t0, t0).map(|(ticks, _)| ticks);
        assert_eq!(ticks(1), Some(1));
        assert_eq!(ticks(9), Some(1));
        assert_eq!(ticks(10), Some(1));
        assert_eq!(ticks(11), Some(2));
        assert_eq!(ticks(15), Some(2));
        assert_eq!(ticks(25), Some(3));
    }

    #[test]
    fn epoch_refund_leaves_out_the_overshoot_past_the_deadline() {
        // 100 ms budget armed at t0, then a 300 ms host wait: the deadline
        // (t0 + 100 ms) fires when the guest resumes at t0 + 300 ms. The guest
        // has used none of its budget, so it gets 100 ms more — not the 300 ms
        // of credit counted from the resumption point.
        let t0 = Instant::now();
        let deadline = t0 + 100 * MS;
        assert_eq!(
            epoch_refund(300, deadline, t0 + 300 * MS),
            Some((10, t0 + 400 * MS))
        );
    }

    #[test]
    fn epoch_refund_traps_when_the_budget_is_spent_even_with_the_credit() {
        // The guest ran its 100 ms, then waited 50 ms, and the deadline only
        // fired at t0 + 200 ms: deadline + credit (t0 + 150 ms) is behind.
        let t0 = Instant::now();
        assert_eq!(epoch_refund(50, t0 + 100 * MS, t0 + 200 * MS), None);
        // Exactly at it: nothing left either.
        assert_eq!(epoch_refund(50, t0 + 100 * MS, t0 + 150 * MS), None);
    }

    #[test]
    fn epoch_refund_handles_huge_credit() {
        // Must not panic or overflow on extreme inputs.
        let t0 = Instant::now();
        let _ = epoch_refund(u64::MAX, t0, t0);
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

    /// Arm a `ticks` deadline and install the epoch-deadline callback used in
    /// production (see `StoreBuilder::build`).
    fn arm_with_callback<T: 'static>(
        store: &mut WtStore<T>,
        epoch_pause_ms: Arc<AtomicU64>,
        ticks: u64,
    ) {
        store.set_epoch_deadline(ticks);
        store.epoch_deadline_callback(epoch_deadline_callback(epoch_pause_ms, ticks));
    }

    #[test]
    fn busy_loop_without_credit_traps_with_interrupt() {
        let engine = make_engine();
        let module = Module::new(&engine, SPIN_WAT).unwrap();
        let epoch_pause_ms = Arc::new(AtomicU64::new(0));

        let mut store = WtStore::new(&engine, ());
        arm_with_callback(&mut store, epoch_pause_ms.clone(), 1); // very short budget: 1 tick

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
        // because `epoch_pause_ms` was never incremented.
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

        let mut store = WtStore::new(&engine, ());
        arm_with_callback(&mut store, epoch_pause_ms.clone(), 1); // very short budget: 1 tick

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

    /// A guest that calls a host function which blocks for `wait_ms` (the
    /// epoch keeps ticking, as during an outbound request) and deposits that
    /// wait as credit, then spins forever.
    const WAIT_THEN_SPIN_WAT: &str = r#"
        (module
            (import "host" "wait" (func $wait))
            (func (export "run")
                (call $wait)
                (loop $l (br $l))))
    "#;

    /// A host wait that runs past the deadline gives back only the budget
    /// that was left, not the whole wait: under a 100 ms budget a guest that
    /// waits 300 ms at once and then spins is stopped ~100 ms after the wait
    /// (it got ~300 ms when the credit was counted from the resumption epoch).
    #[test]
    fn wait_past_the_deadline_refunds_only_the_remaining_budget() {
        const BUDGET_TICKS: u64 = 10; // 100 ms
        const WAIT: Duration = Duration::from_millis(300);

        let engine = make_engine();
        let module = Module::new(&engine, WAIT_THEN_SPIN_WAT).unwrap();
        let epoch_pause_ms = Arc::new(AtomicU64::new(0));
        let mut store = WtStore::new(&engine, ());
        arm_with_callback(&mut store, epoch_pause_ms.clone(), BUDGET_TICKS);

        let wait = {
            let credit = epoch_pause_ms.clone();
            wasmtime::Func::wrap(&mut store, move || {
                thread::sleep(WAIT);
                credit.fetch_add(WAIT.as_millis() as u64, Ordering::Relaxed);
            })
        };
        let instance = Instance::new(&mut store, &module, &[wait.into()]).unwrap();
        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .unwrap();

        // The production ticker period, so ticks and milliseconds agree.
        let stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let engine = engine.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(DEFAULT_EPOCH_TICK_INTERVAL));
                    engine.increment_epoch();
                }
            })
        };
        let started = Instant::now();
        let result = run.call(&mut store, ());
        let elapsed = started.elapsed();
        stop.store(true, Ordering::Relaxed);
        ticker.join().unwrap();

        let err = result.expect_err("the spin must end on the epoch deadline");
        assert_eq!(
            err.root_cause().downcast_ref::<Trap>().copied(),
            Some(Trap::Interrupt)
        );
        assert!(elapsed >= WAIT, "took {elapsed:?}");
        // ~WAIT + 100 ms; the over-refund made it ~WAIT × 2.
        assert!(
            elapsed < WAIT + Duration::from_millis(180),
            "took {elapsed:?}"
        );
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

        let store = StoreBuilder::new(
            engine.clone(),
            WasiVersion::Preview1,
            CachedGraphRegistry::new(),
        )
        .build(NoStats)
        .expect("build store");
        assert_eq!(
            instances::live(),
            before + 1,
            "building a store must count a live instance"
        );

        let second = StoreBuilder::new(engine, WasiVersion::Preview1, CachedGraphRegistry::new())
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

    /// An app without wasi-nn env preloads nothing, and a store's wasi-nn
    /// context then comes from the preloaded (empty) set without touching the
    /// loader. (`make_wasi_nn` is called directly: building a whole `Store`
    /// here would race the live-instance counts asserted by the test above.)
    #[test]
    fn preload_wasi_nn_without_nn_env_is_a_noop() {
        let builder = StoreBuilder::new(
            make_engine(),
            WasiVersion::Preview1,
            CachedGraphRegistry::new(),
        )
        .set_env(&[("UNRELATED", "1")])
        .preload_wasi_nn()
        .expect("preload");
        assert_eq!(builder.nn_graphs.as_ref().map(Vec::len), Some(0));
        builder
            .make_wasi_nn()
            .expect("wasi-nn ctx from preloaded graphs");
    }

    /// A builder that never preloaded (proxy-wasm) builds no wasi-nn context,
    /// whatever the env says: the engine would not link it anyway.
    #[test]
    fn make_wasi_nn_is_none_without_preload() {
        let builder = StoreBuilder::new(
            make_engine(),
            WasiVersion::Preview1,
            CachedGraphRegistry::new(),
        )
        .set_env(&[
            ("FASTEDGE_NN_BACKENDS", "openvino"),
            ("FASTEDGE_NN_OPENVINO_MODEL", "/no/such/model-dir"),
        ]);
        assert!(builder.has_wasi_nn_env());
        assert!(builder.make_wasi_nn().is_none());
    }

    /// A graph the executor cannot load fails executor creation — with the
    /// offending directory in the error — instead of every request later on.
    #[test]
    fn preload_wasi_nn_reports_an_unloadable_graph_at_executor_creation() {
        let error = StoreBuilder::new(
            make_engine(),
            WasiVersion::Preview1,
            CachedGraphRegistry::new(),
        )
        .set_env(&[
            ("FASTEDGE_NN_BACKENDS", "openvino"),
            ("FASTEDGE_NN_OPENVINO_MODEL", "/no/such/model-dir"),
        ])
        .preload_wasi_nn()
        .err()
        .expect("missing model dir must fail")
        .to_string();
        assert!(error.contains("/no/such/model-dir"), "{error}");
    }
}
