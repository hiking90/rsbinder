// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! How long a side of a shared-memory channel looks at the memory before it parks on a futex.
//!
//! A park costs a futex wake on the peer's side and a wake-up on this one. When the peer is
//! running and about to write, looking for a few microseconds finds the change sooner and
//! spares both. When it is not, the look burns a core for nothing. This module is the policy
//! `rsbinder`'s `stream` ring measured and settled on (plan 10-7c D13), kept in one place so
//! every channel in a process shares its limits:
//!
//! * a look lasts at most [`SPIN`], and none on a single-core machine ([`spin_budget`]);
//! * at most half the process's cores' worth of threads look at once, past a first short
//!   round that touches nothing shared ([`spin_until`]);
//! * a side whose recent looks all came up empty stops looking, probes now and then, and
//!   starts again once a look finds something or a park shows one would have ([`SpinGauge`]).
//!
//! The limit on concurrent looks is one counter per process. A crate that copied this code
//! would have a counter of its own and double the limit, which is why it lives here.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long one look lasts at most.
///
/// Measured on `stream`'s ring (plan 10-7c D13): long enough to catch a peer that is mid-write,
/// short against the futex round trip it replaces.
pub const SPIN: Duration = Duration::from_micros(20);

/// Looks between clock reads in [`spin_until`]: a clock read costs more than a look.
pub const SPIN_CLOCK_EVERY: u32 = 32;

/// Full looks in a row that found nothing, after which a [`SpinGauge`] turns its side's look off.
const WASTED_SPINS: u32 = 64;

/// While a gauge is off, every this many waits look in full to see whether looking pays again.
const PROBE_EVERY: u32 = 8;

/// The look for a wait that ends at `deadline`: [`SPIN`], cut to what is left before
/// `deadline`, and zero on a single-core machine, where looking only holds up the peer.
#[inline]
pub fn spin_budget(deadline: Option<Instant>) -> Duration {
    static MULTICORE: OnceLock<bool> = OnceLock::new();
    let multicore =
        *MULTICORE.get_or_init(|| std::thread::available_parallelism().is_ok_and(|n| n.get() > 1));
    match deadline {
        _ if !multicore => Duration::ZERO,
        Some(deadline) => SPIN.min(deadline.saturating_duration_since(Instant::now())),
        None => SPIN,
    }
}

/// One of this process's concurrent looks: a look pays only while its peer runs, so half the cores.
struct SpinSlot;

static SPINNERS: AtomicUsize = AtomicUsize::new(0);

impl SpinSlot {
    #[inline]
    fn take() -> Option<SpinSlot> {
        static MAX: OnceLock<usize> = OnceLock::new();
        let max = *MAX.get_or_init(|| {
            std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1))
        });
        if SPINNERS.fetch_add(1, Ordering::Relaxed) >= max {
            SPINNERS.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(SpinSlot)
    }
}

impl Drop for SpinSlot {
    #[inline]
    fn drop(&mut self) {
        SPINNERS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Looks until `done` returns `true` or `budget` has passed.
///
/// Returns whether `done` held, and the budget the look ran under: `budget` itself, or zero
/// when the look ended early because the process already had its share of threads looking.
/// Pass that second value to [`SpinGauge::spun`].
///
/// The first [`SPIN_CLOCK_EVERY`] looks touch nothing shared and read no clock, and on a busy
/// channel most looks end there. Only past them does the call take one of the process's look
/// slots — half its cores' worth, counted for every caller of this function in the process —
/// and read the clock every [`SPIN_CLOCK_EVERY`] looks. The caller has looked once already
/// before calling: a zero `budget` returns `(false, Duration::ZERO)` without calling `done`.
#[inline]
pub fn spin_until(budget: Duration, mut done: impl FnMut() -> bool) -> (bool, Duration) {
    if budget.is_zero() {
        return (false, budget);
    }
    for _ in 0..SPIN_CLOCK_EVERY {
        std::hint::spin_loop();
        if done() {
            return (true, budget);
        }
    }
    let Some(_slot) = SpinSlot::take() else {
        return (false, Duration::ZERO);
    };
    let start = Instant::now();
    loop {
        for _ in 0..SPIN_CLOCK_EVERY {
            std::hint::spin_loop();
            if done() {
                return (true, budget);
            }
        }
        if start.elapsed() >= budget {
            return (false, budget);
        }
    }
}

/// Whether one side of a channel looks before it parks, from how its recent looks went.
///
/// A side whose peer writes less often than [`SPIN`] finds nothing in any look, and every look
/// burns [`SPIN`] of a core. The gauge counts full looks that found nothing; after 64 in a row
/// [`budget`](Self::budget) returns zero, so the side parks at once. While off, every eighth
/// wait looks in full as a probe. The look comes back on when a probe finds something, or when
/// a park that no look preceded ends within [`SPIN`] — a look would have caught that one.
///
/// One gauge per side and channel. Each wait goes:
///
/// ```
/// # use rsbinder_fmq::shm::spin::{spin_until, SpinGauge};
/// # use std::time::Instant;
/// # let gauge = SpinGauge::default();
/// # let ready = || true;
/// # let park = || ();
/// let budget = gauge.budget(None);
/// let (found, spun) = spin_until(budget, ready);
/// gauge.spun(spun, found);
/// if !found {
///     let parked_at = Instant::now();
///     park();
///     gauge.parked(parked_at.elapsed());
/// }
/// ```
///
/// The counts are `Relaxed` atomics, so a gauge shared by threads stays a heuristic and never
/// a data race.
#[derive(Debug, Default)]
pub struct SpinGauge {
    wasted: AtomicU32,
    /// Waits parked without a look since the last probe.
    skipped: AtomicU32,
}

impl SpinGauge {
    /// The look for this wait: [`spin_budget`], or zero while the gauge is off and this wait
    /// is not a probe. A wait that returns zero is counted towards the next probe.
    #[inline]
    pub fn budget(&self, deadline: Option<Instant>) -> Duration {
        if self.is_off() {
            let skipped = self.skipped.load(Ordering::Relaxed) + 1;
            if skipped < PROBE_EVERY {
                self.skipped.store(skipped, Ordering::Relaxed);
                return Duration::ZERO;
            }
            self.skipped.store(0, Ordering::Relaxed);
        }
        spin_budget(deadline)
    }

    /// Whether looking is off: the last 64 full looks found nothing.
    #[inline]
    pub fn is_off(&self) -> bool {
        self.wasted.load(Ordering::Relaxed) >= WASTED_SPINS
    }

    /// A look of `budget` ended; `found` is whether it found what it looked for.
    ///
    /// A find turns the look on. A miss counts only when `budget` was a full [`SPIN`]: a look
    /// a near deadline cut short, or one that got no slot, says nothing about the peer.
    #[inline]
    pub fn spun(&self, budget: Duration, found: bool) {
        if found {
            self.turn_on();
        } else if budget >= SPIN {
            let wasted = self.wasted.load(Ordering::Relaxed);
            self.wasted.store(
                wasted.saturating_add(1).min(WASTED_SPINS),
                Ordering::Relaxed,
            );
        }
    }

    /// A park ended after `took`, woken by the peer.
    ///
    /// Turns the look back on when the gauge is off, this wait parked without looking, and the
    /// wake came within [`SPIN`]. After a full look, a quick wake is that look's miss and
    /// changes nothing. Leave out a park that did not sleep — one whose wake was already
    /// pending — as it says nothing about how soon the peer writes.
    #[inline]
    pub fn parked(&self, took: Duration) {
        if self.is_off() && self.skipped.load(Ordering::Relaxed) > 0 && took < SPIN {
            self.turn_on();
        }
    }

    #[inline]
    fn turn_on(&self) {
        self.wasted.store(0, Ordering::Relaxed);
        self.skipped.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With every spin slot taken, a spin ends after its first round and reports no spin.
    #[test]
    fn a_spin_with_no_free_slot_reports_no_spin() {
        if spin_budget(None) < SPIN {
            return;
        }
        // Other tests' waits only skip their spins meanwhile.
        SPINNERS.fetch_add(1 << 20, Ordering::Relaxed);
        let unspun = spin_until(SPIN, || false);
        let mut looks = 0;
        let caught = spin_until(SPIN, || {
            looks += 1;
            looks == 3
        });
        SPINNERS.fetch_sub(1 << 20, Ordering::Relaxed);
        assert_eq!(unspun, (false, Duration::ZERO));
        assert_eq!(caught, (true, SPIN), "the first round needs no slot");
    }

    /// Plan 10-7c D13: a spin a deadline cut short and a slow wake change nothing.
    #[test]
    fn the_spin_turns_off_after_wasted_spins_and_back_on_after_a_quick_wake() {
        if spin_budget(None) < SPIN {
            return; // One core: no spin to turn off.
        }
        let gauge = SpinGauge::default();
        for _ in 0..WASTED_SPINS - 1 {
            gauge.spun(SPIN, false);
        }
        gauge.spun(SPIN / 2, false);
        gauge.parked(Duration::from_millis(1));
        assert_eq!(
            gauge.budget(None),
            SPIN,
            "one wasted full spin short of the limit"
        );
        gauge.spun(SPIN, false);
        gauge.parked(Duration::from_micros(1));
        assert_eq!(
            gauge.budget(None),
            Duration::ZERO,
            "a quick wake after a wasted full spin keeps it off"
        );
        gauge.spun(Duration::ZERO, false);
        gauge.parked(Duration::from_millis(1));
        assert_eq!(
            gauge.budget(None),
            Duration::ZERO,
            "a slow wake keeps it off"
        );
        gauge.parked(Duration::from_micros(1));
        assert_eq!(gauge.budget(None), SPIN, "a wake a spin would have caught");
        for _ in 0..WASTED_SPINS - 1 {
            gauge.spun(SPIN, false);
        }
        gauge.spun(SPIN, true);
        gauge.spun(SPIN, false);
        assert_eq!(gauge.budget(None), SPIN, "a find starts the count over");
    }

    /// Plan 10-7c D13: no quick wake needed, which an emulator's wakes never are.
    #[test]
    fn a_spin_turned_off_is_probed_and_a_probe_that_finds_turns_it_on() {
        if spin_budget(None) < SPIN {
            return;
        }
        let gauge = SpinGauge::default();
        for _ in 0..WASTED_SPINS {
            gauge.spun(SPIN, false);
        }
        for round in 0..2 {
            for wait in 1..PROBE_EVERY {
                let budget = gauge.budget(None);
                assert_eq!(budget, Duration::ZERO, "round {round}, wait {wait}");
                gauge.spun(budget, false);
                gauge.parked(Duration::from_millis(1));
            }
            assert_eq!(gauge.budget(None), SPIN, "round {round}: the probe");
            if round == 0 {
                gauge.spun(SPIN, false);
                gauge.parked(Duration::from_micros(1));
                assert!(gauge.is_off(), "a probe that finds nothing keeps it off");
            }
        }
        gauge.spun(SPIN, true);
        assert!(!gauge.is_off());
        assert_eq!(gauge.budget(None), SPIN);
    }
}
