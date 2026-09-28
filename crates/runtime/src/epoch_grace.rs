//! CPU-aware grace for the wasm epoch deadline.
//!
//! The epoch budget is wall-clock: a ticker thread bumps the engine epoch every
//! `DEFAULT_EPOCH_TICK_INTERVAL` ms whether or not the guest's worker is
//! running. A worker parked in the kernel — most often in direct reclaim on a
//! first-touch page fault, which under memory pressure waits with no timeout —
//! therefore burns its budget while executing nothing, and traps with
//! `Trap::Interrupt` at the next epoch check. The guest did nothing wrong.
//!
//! This module lets the deadline callback tell that case apart from a guest
//! that really spent its budget. When the deadline fires with no host-call
//! credit left, compare the CPU time the executing thread consumed since the
//! budget was armed against the wall budget. If it used less than
//! `1 / STALL_CPU_DIVISOR` of it, the thread was parked: hand back the unused
//! part as wall time, at most [`MAX_STALL_GRANTS`] times per armed budget.
//! Otherwise trap exactly as before.
//!
//! Measurement is per thread (`CLOCK_THREAD_CPUTIME_ID`), so the comparison is
//! only meaningful while the guest keeps running on the thread the budget was
//! armed on. A fiber resumed on another worker after an awaited host call
//! fails that check and is denied — never granted on a garbage delta. That is
//! the conservative choice: a denial is today's behaviour, while a wrong grant
//! would let a runaway guest exceed its budget.

use std::thread::{self, ThreadId};
use std::time::Instant;

/// Bounded number of extensions per armed budget. The worst-case wall time for
/// a guest that keeps getting parked is `budget × (1 + MAX_STALL_GRANTS)`.
pub const MAX_STALL_GRANTS: u32 = 3;

/// A deadline hit counts as a stall when the CPU time consumed is below
/// `budget / STALL_CPU_DIVISOR`. Half is deliberately strict: a guest that
/// genuinely ran for most of its budget is not rescued by a little jitter.
pub const STALL_CPU_DIVISOR: u64 = 2;

/// Why a deadline hit was not extended. Every variant is a trap, exactly as
/// before this module existed; the reason is exported so the "never granted"
/// cases stay visible instead of being folded into the timeout count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The deadline fired before any budget was armed.
    NotArmed,
    /// [`MAX_STALL_GRANTS`] extensions were already handed out for this budget.
    GrantsExhausted,
    /// The guest is running on a different thread than the one the budget was
    /// armed on, so the per-thread CPU delta means nothing.
    ThreadMigrated,
    /// No thread CPU clock on this platform.
    CpuClockUnavailable,
    /// The guest really consumed its budget.
    CpuExhausted { cpu_used_ms: u64, budget_ms: u64 },
}

impl DenyReason {
    fn label(self) -> &'static str {
        match self {
            DenyReason::NotArmed => "denied_not_armed",
            DenyReason::GrantsExhausted => "denied_grants_exhausted",
            DenyReason::ThreadMigrated => "denied_thread_migrated",
            DenyReason::CpuClockUnavailable => "denied_cpu_clock_unavailable",
            DenyReason::CpuExhausted { .. } => "denied_cpu_exhausted",
        }
    }
}

/// Outcome of one deadline hit that found no host-call credit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochGraceEvent {
    /// The thread was parked; the deadline is extended by `ticks`.
    Granted {
        ticks: u64,
        cpu_used_ms: u64,
        wall_ms: u64,
    },
    /// Trap, as before.
    Denied(DenyReason),
}

impl EpochGraceEvent {
    /// Every value [`Self::outcome_label`] can return, for pre-initialising a
    /// labelled counter so an outcome that never happened reads as `0`.
    pub const OUTCOME_LABELS: &'static [&'static str] = &[
        "granted",
        "denied_not_armed",
        "denied_grants_exhausted",
        "denied_thread_migrated",
        "denied_cpu_clock_unavailable",
        "denied_cpu_exhausted",
    ];

    /// Stable metric label for this outcome.
    pub fn outcome_label(&self) -> &'static str {
        match self {
            EpochGraceEvent::Granted { .. } => "granted",
            EpochGraceEvent::Denied(reason) => reason.label(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Baseline {
    thread: ThreadId,
    /// Thread CPU time when the budget was armed (or last extended).
    cpu_ns: Option<u64>,
    /// Wall clock when the budget was armed (or last extended); for logging.
    wall: Instant,
    /// Ticks the current deadline was set to.
    ticks: u64,
}

/// Per-store state for the deadline callback. Lives in the store data, so the
/// callback reaches it through `StoreContextMut::data_mut` with no shared
/// state and no atomics.
#[derive(Debug)]
pub struct EpochGrace {
    tick_ms: u64,
    baseline: Option<Baseline>,
    grants: u32,
}

impl EpochGrace {
    /// `tick_ms` is the epoch ticker period the budget is denominated in
    /// (`DEFAULT_EPOCH_TICK_INTERVAL` in production).
    pub fn new(tick_ms: u64) -> Self {
        Self {
            tick_ms,
            baseline: None,
            grants: 0,
        }
    }

    /// Record that a fresh budget of `ticks` was just armed on the current
    /// thread. Call this together with `Store::set_epoch_deadline`; every arm
    /// starts a new grant allowance.
    pub fn arm(&mut self, ticks: u64) {
        self.arm_at(
            ticks,
            thread::current().id(),
            thread_cpu_ns(),
            Instant::now(),
        );
    }

    fn arm_at(&mut self, ticks: u64, thread: ThreadId, cpu_ns: Option<u64>, wall: Instant) {
        self.baseline = Some(Baseline {
            thread,
            cpu_ns,
            wall,
            ticks,
        });
        self.grants = 0;
    }

    /// Re-baseline for a *credited* deadline extension (host-call credit
    /// returned `Continue(ticks)`): the new segment of `ticks` is measured
    /// from the current thread, CPU clock and wall clock, so a later
    /// no-credit hit compares CPU used within this segment against this
    /// segment's budget — not the accumulated usage since the original arm
    /// against a stale budget.
    ///
    /// Deliberately unlike [`Self::arm`], the grant counter is **not**
    /// reset: stall grants stay bounded to [`MAX_STALL_GRANTS`] per armed
    /// budget no matter how many credited extensions happen in between, so
    /// host-call credit cannot be used to mint fresh stall allowances.
    pub fn rebase(&mut self, ticks: u64) {
        self.rebase_at(
            ticks,
            thread::current().id(),
            thread_cpu_ns(),
            Instant::now(),
        );
    }

    fn rebase_at(&mut self, ticks: u64, thread: ThreadId, cpu_ns: Option<u64>, wall: Instant) {
        self.baseline = Some(Baseline {
            thread,
            cpu_ns,
            wall,
            ticks,
        });
    }

    /// Extensions handed out for the currently armed budget.
    pub fn grants(&self) -> u32 {
        self.grants
    }

    /// Ticks of the current baseline segment; test-only introspection.
    #[cfg(test)]
    pub(crate) fn baseline_ticks(&self) -> Option<u64> {
        self.baseline.map(|b| b.ticks)
    }

    /// Decide what to do about a deadline hit that has no host-call credit,
    /// measured on the current thread right now.
    pub fn on_deadline(&mut self) -> EpochGraceEvent {
        self.decide(thread_cpu_ns(), thread::current().id(), Instant::now())
    }

    /// The decision proper, with the clocks passed in so it can be tested
    /// without sleeping or spinning.
    pub fn decide(
        &mut self,
        now_cpu_ns: Option<u64>,
        now_thread: ThreadId,
        now: Instant,
    ) -> EpochGraceEvent {
        let Some(baseline) = self.baseline.as_mut() else {
            return EpochGraceEvent::Denied(DenyReason::NotArmed);
        };
        if self.grants >= MAX_STALL_GRANTS {
            return EpochGraceEvent::Denied(DenyReason::GrantsExhausted);
        }
        if now_thread != baseline.thread {
            return EpochGraceEvent::Denied(DenyReason::ThreadMigrated);
        }
        let (Some(now_cpu_ns), Some(armed_cpu_ns)) = (now_cpu_ns, baseline.cpu_ns) else {
            return EpochGraceEvent::Denied(DenyReason::CpuClockUnavailable);
        };

        let cpu_used_ms = now_cpu_ns.saturating_sub(armed_cpu_ns) / 1_000_000;
        let budget_ms = baseline.ticks.saturating_mul(self.tick_ms);
        if cpu_used_ms.saturating_mul(STALL_CPU_DIVISOR) >= budget_ms {
            return EpochGraceEvent::Denied(DenyReason::CpuExhausted {
                cpu_used_ms,
                budget_ms,
            });
        }

        // Parked. Hand back the part of the budget the guest never got to use,
        // rounded up to whole ticks so it is never under-refunded, and start
        // measuring the next segment from here.
        let remaining_ms = budget_ms - cpu_used_ms;
        let ticks = remaining_ms.div_ceil(self.tick_ms.max(1)).max(1);
        let wall_ms = now.saturating_duration_since(baseline.wall).as_millis() as u64;
        baseline.cpu_ns = Some(now_cpu_ns);
        baseline.wall = now;
        baseline.ticks = ticks;
        self.grants += 1;
        EpochGraceEvent::Granted {
            ticks,
            cpu_used_ms,
            wall_ms,
        }
    }
}

/// CPU time consumed by the calling thread, in nanoseconds.
///
/// `CLOCK_THREAD_CPUTIME_ID` is what makes the whole scheme work: it advances
/// only while this thread is on a CPU, so a thread parked in the kernel reads
/// the same value before and after the park.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn thread_cpu_ns() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable `timespec`, and the clock id is a
    // constant supported on both targets this is compiled for.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    (rc == 0).then(|| (ts.tv_sec as u64).saturating_mul(1_000_000_000) + ts.tv_nsec as u64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn thread_cpu_ns() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TICK_MS: u64 = 10;
    const NS_PER_MS: u64 = 1_000_000;

    fn armed(ticks: u64, cpu_ms: u64) -> (EpochGrace, ThreadId, Instant) {
        let tid = thread::current().id();
        let t0 = Instant::now();
        let mut g = EpochGrace::new(TICK_MS);
        g.arm_at(ticks, tid, Some(cpu_ms * NS_PER_MS), t0);
        (g, tid, t0)
    }

    #[test]
    fn deadline_before_any_arm_is_denied() {
        let mut g = EpochGrace::new(TICK_MS);
        assert_eq!(
            g.decide(Some(0), thread::current().id(), Instant::now()),
            EpochGraceEvent::Denied(DenyReason::NotArmed)
        );
    }

    #[test]
    fn parked_thread_is_granted_the_unused_budget() {
        // 100 ticks × 10 ms = 1 s of wall budget; the thread used 50 ms of CPU.
        let (mut g, tid, t0) = armed(100, 1_000);
        let ev = g.decide(
            Some((1_000 + 50) * NS_PER_MS),
            tid,
            t0 + Duration::from_millis(1_000),
        );
        assert_eq!(
            ev,
            EpochGraceEvent::Granted {
                ticks: 95, // (1000 ms - 50 ms) / 10 ms
                cpu_used_ms: 50,
                wall_ms: 1_000,
            }
        );
        assert_eq!(g.grants(), 1);
    }

    #[test]
    fn busy_thread_is_denied() {
        let (mut g, tid, t0) = armed(100, 1_000);
        // 600 ms of CPU out of a 1 s budget: it really ran.
        let ev = g.decide(Some((1_000 + 600) * NS_PER_MS), tid, t0);
        assert_eq!(
            ev,
            EpochGraceEvent::Denied(DenyReason::CpuExhausted {
                cpu_used_ms: 600,
                budget_ms: 1_000,
            })
        );
        assert_eq!(g.grants(), 0);
    }

    #[test]
    fn exactly_half_the_budget_is_denied() {
        // The boundary belongs to "busy": STALL_CPU_DIVISOR is a strict bound.
        let (mut g, tid, t0) = armed(100, 0);
        let ev = g.decide(Some(500 * NS_PER_MS), tid, t0);
        assert!(matches!(
            ev,
            EpochGraceEvent::Denied(DenyReason::CpuExhausted { .. })
        ));
    }

    #[test]
    fn migrated_thread_is_denied_even_when_idle() {
        let (mut g, _tid, t0) = armed(100, 1_000);
        let other = thread::spawn(|| thread::current().id()).join().unwrap();
        // Zero CPU consumed would look like a park — but the delta is across
        // two different threads' clocks, so it must not be trusted.
        assert_eq!(
            g.decide(Some(1_000 * NS_PER_MS), other, t0),
            EpochGraceEvent::Denied(DenyReason::ThreadMigrated)
        );
    }

    #[test]
    fn grants_are_bounded_per_armed_budget() {
        let (mut g, tid, t0) = armed(100, 1_000);
        for i in 0..MAX_STALL_GRANTS {
            let ev = g.decide(Some(1_000 * NS_PER_MS), tid, t0);
            assert!(
                matches!(ev, EpochGraceEvent::Granted { .. }),
                "grant #{i} expected, got {ev:?}"
            );
        }
        assert_eq!(
            g.decide(Some(1_000 * NS_PER_MS), tid, t0),
            EpochGraceEvent::Denied(DenyReason::GrantsExhausted)
        );
    }

    #[test]
    fn rebase_measures_the_next_segment_from_the_rebase() {
        // 1 s armed budget; the guest burnt 900 ms of CPU, then a credited
        // extension rebased it to a fresh 50-tick (500 ms) segment. A later
        // hit that used 100 ms *within the new segment* must be a grant —
        // without the rebase it would read 1 000 ms against the stale 1 s
        // budget and be denied as CpuExhausted.
        let (mut g, tid, t0) = armed(100, 1_000);
        let t1 = t0 + Duration::from_millis(2_000);
        g.rebase_at(50, tid, Some((1_000 + 900) * NS_PER_MS), t1);
        let ev = g.decide(
            Some((1_000 + 900 + 100) * NS_PER_MS),
            tid,
            t1 + Duration::from_millis(500),
        );
        assert_eq!(
            ev,
            EpochGraceEvent::Granted {
                ticks: 40, // (500 ms - 100 ms) / 10 ms
                cpu_used_ms: 100,
                wall_ms: 500,
            }
        );
    }

    #[test]
    fn rebase_does_not_reset_the_grant_allowance() {
        let (mut g, tid, t0) = armed(100, 1_000);
        for _ in 0..MAX_STALL_GRANTS {
            g.decide(Some(1_000 * NS_PER_MS), tid, t0);
        }
        g.rebase_at(100, tid, Some(1_000 * NS_PER_MS), t0);
        assert_eq!(g.grants(), MAX_STALL_GRANTS);
        assert_eq!(
            g.decide(Some(1_000 * NS_PER_MS), tid, t0),
            EpochGraceEvent::Denied(DenyReason::GrantsExhausted)
        );
    }

    #[test]
    fn re_arming_resets_the_grant_allowance() {
        let (mut g, tid, t0) = armed(100, 1_000);
        for _ in 0..MAX_STALL_GRANTS {
            g.decide(Some(1_000 * NS_PER_MS), tid, t0);
        }
        g.arm_at(100, tid, Some(1_000 * NS_PER_MS), t0);
        assert_eq!(g.grants(), 0);
        assert!(matches!(
            g.decide(Some(1_000 * NS_PER_MS), tid, t0),
            EpochGraceEvent::Granted { .. }
        ));
    }

    #[test]
    fn a_grant_measures_the_next_segment_from_the_grant() {
        // After a grant, CPU consumed *before* it must not count against the
        // new, smaller budget.
        let (mut g, tid, t0) = armed(100, 1_000);
        let ev = g.decide(Some((1_000 + 100) * NS_PER_MS), tid, t0);
        let EpochGraceEvent::Granted { ticks, .. } = ev else {
            panic!("{ev:?}")
        };
        assert_eq!(ticks, 90);
        // Another 100 ms of CPU against the new 900 ms budget: still parked.
        let ev = g.decide(Some((1_000 + 200) * NS_PER_MS), tid, t0);
        assert_eq!(
            ev,
            EpochGraceEvent::Granted {
                ticks: 80,
                cpu_used_ms: 100,
                wall_ms: 0,
            }
        );
    }

    #[test]
    fn missing_cpu_clock_is_denied() {
        let tid = thread::current().id();
        let mut g = EpochGrace::new(TICK_MS);
        g.arm_at(100, tid, None, Instant::now());
        assert_eq!(
            g.decide(Some(0), tid, Instant::now()),
            EpochGraceEvent::Denied(DenyReason::CpuClockUnavailable)
        );
        g.arm_at(100, tid, Some(0), Instant::now());
        assert_eq!(
            g.decide(None, tid, Instant::now()),
            EpochGraceEvent::Denied(DenyReason::CpuClockUnavailable)
        );
    }

    #[test]
    fn every_outcome_has_a_pre_initialised_label() {
        let events = [
            EpochGraceEvent::Granted {
                ticks: 1,
                cpu_used_ms: 0,
                wall_ms: 0,
            },
            EpochGraceEvent::Denied(DenyReason::NotArmed),
            EpochGraceEvent::Denied(DenyReason::GrantsExhausted),
            EpochGraceEvent::Denied(DenyReason::ThreadMigrated),
            EpochGraceEvent::Denied(DenyReason::CpuClockUnavailable),
            EpochGraceEvent::Denied(DenyReason::CpuExhausted {
                cpu_used_ms: 1,
                budget_ms: 1,
            }),
        ];
        assert_eq!(events.len(), EpochGraceEvent::OUTCOME_LABELS.len());
        for ev in events {
            assert!(
                EpochGraceEvent::OUTCOME_LABELS.contains(&ev.outcome_label()),
                "{ev:?} -> {}",
                ev.outcome_label()
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn thread_cpu_clock_advances_with_work_and_not_with_sleep() {
        let a = thread_cpu_ns().expect("thread CPU clock");
        thread::sleep(Duration::from_millis(20));
        let b = thread_cpu_ns().unwrap();
        // Sleeping costs (close to) nothing.
        assert!(b - a < 5 * NS_PER_MS, "sleep charged {} ns of CPU", b - a);
        // Burn ~2 ms of real CPU; `black_box` keeps the loop from being
        // optimised away.
        let mut x = 0u64;
        while thread_cpu_ns().unwrap() - b < 2 * NS_PER_MS {
            x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        std::hint::black_box(x);
        assert!(thread_cpu_ns().unwrap() - b >= 2 * NS_PER_MS);
    }
}
