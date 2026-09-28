// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Typed session lifecycle.
//!
//! The live connection count and the obituary state of
//! [`super::session::SharedSession`] share a single atomic-backed state
//! machine:
//!
//! ```text
//!         try_bump_live ────┐
//!                           ▼
//!     ┌─── new() ──→  Live(n: NonZeroUsize) ───drop_connection (n==1)──→  Dying ──mark_dead──→ Dead
//!                           │  ▲
//!                           │  └─── drop_connection (n>1) decrements
//!                           │       force_dying (any n) ─────────────→  Dying   (explicit shutdown)
//!                           │
//!                           └─── try_bump_live increments
//! ```
//!
//! Encoding (single `AtomicU64`, lock-free on every supported target):
//! the top byte is a state tag, the lower 56 bits are the `Live` count.
//! `Dying`/`Dead` carry no count. Every transition is a CAS — `0` is
//! never transiently visible as `≥ 1` from one observer's point of view
//! between another's bump-and-rollback (the two-attacker hole the
//! CAS-loop closes against a naive optimistic-bump shape). The typed
//! enum makes the four reachable states explicit:
//! a reviewer can grep `SessionLifecycleSnapshot` variants for every
//! point that branches on them.
//!
//! **Default single-connection sessions** never leave `Live(1)` until
//! teardown. The `is_torn_down` snapshot can return `true` in
//! the `Dying` window *before* the obituary completes, so a
//! `RpcProxy::drop` reaper that races the dying founding worker skips its
//! best-effort `DEC_STRONG` without waiting for the obituary callback to
//! return, and never deadlocks on an empty pool.
//!
//! # Encoding
//!
//! 56 count bits exceed any plausible live connection count and leave a
//! margin against the `Live`-tag `+1` overflow path; the word is `u64` so
//! the encoded state stays one lock-free word on every supported target.
//!
//! # Anti-resurrection
//!
//! Once obituaries fired, `try_bump_live` never succeeds: a session whose
//! obituaries fired must never acquire another driver, else `binder_died`
//! is silently lost for any `DeathRecipient` linked through the attaching
//! connection. `try_bump_live` is a CAS loop rather than
//! optimistic-bump-then-rollback: the latter closes the single-attacker
//! race but not the multi-attacker one — two attackers A and B bumping
//! after the founding connection's decrement to 0 could let B see A's
//! transient `prev=1` before A rolls back, attaching to a dying session.
//! Deciding on the loaded value inside the CAS closes it.
//!
//! # Death edge
//!
//! Three transitions leave `Live`, and across all of them at most one
//! caller observes the edge to `Dying`, which is what the obituary contract
//! needs. The caller that gets `true` fires session obituaries and then
//! calls `mark_dead`; every other caller finds a settled state and gets a
//! quiet `false`.
//!
//! - `drop_connection` (connection teardown): `Live(n>1)` CAS-decrements
//!   and returns `false`; `Live(1) → Dying` returns `true`. From
//!   `Dying`/`Dead` it returns `false`: death was already declared by
//!   another connection's exit, by `try_drop_sole_connection` when a
//!   failing transaction emptied the slot pool first, or by `force_dying`.
//! - `try_drop_sole_connection` (client-side death detection):
//!   `Live(1) → Dying` only when this is the sole connection and the
//!   session is still `Live`; `false` with no assert from any other state.
//!   Used when a session with no serve worker loses its last pool slot (a
//!   client whose peer went away) so the same obituary + clear sequence
//!   runs there. It races a serve worker's own `drop_connection` and either
//!   may win.
//! - `force_dying` (explicit shutdown): `Live(n) → Dying` from any `n`.
//!   The live count is discarded on purpose — the caller is shutting those
//!   connections down, and their workers' later `drop_connection` calls
//!   return `false` quietly.

#[cfg(test)]
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};

/// State tag in the top byte, `Live` count (0 otherwise) below; see module doc "Encoding".
const STATE_SHIFT: u32 = 56;
const COUNT_MASK: u64 = (1u64 << STATE_SHIFT) - 1;
const STATE_LIVE_TAG: u64 = 0;
const STATE_DYING_TAG: u64 = 1;
const STATE_DEAD_TAG: u64 = 2;

/// Test-only exhaustive-match snapshot; production uses `is_torn_down` / `live_count`.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionLifecycleSnapshot {
    /// At least one connection drives the session; `n` is the live connection count.
    Live(NonZeroUsize),
    /// Last `Live` connection dropped; obituaries are firing. Becomes `Dead` when they return.
    Dying,
    /// Obituaries completed; `try_bump_live` never succeeds (module doc "Anti-resurrection").
    Dead,
}

/// Session state + live count in one `AtomicU64`; every method is lock-free.
pub(crate) struct SessionLifecycle {
    inner: AtomicU64,
}

impl SessionLifecycle {
    /// `Live(1)`: the founding connection.
    pub(crate) fn new() -> Self {
        Self {
            inner: AtomicU64::new(encode_live(1)),
        }
    }

    /// Typed snapshot for tests; the hot path uses `is_torn_down` / `live_count`.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> SessionLifecycleSnapshot {
        decode(self.inner.load(Ordering::SeqCst))
    }

    /// `true` in `Dying`/`Dead` (no new work); read by `RpcProxy::drop`, `add_callback_slot`.
    pub(crate) fn is_torn_down(&self) -> bool {
        self.inner.load(Ordering::Acquire) >> STATE_SHIFT != STATE_LIVE_TAG
    }

    /// Current live count (`Live(n) → n`, `Dying`/`Dead → 0`).
    pub(crate) fn live_count(&self) -> usize {
        let v = self.inner.load(Ordering::SeqCst);
        if v >> STATE_SHIFT == STATE_LIVE_TAG {
            (v & COUNT_MASK) as usize
        } else {
            0
        }
    }

    /// Bumps the count iff still `Live`, else `false`; CAS loop per module doc "Anti-resurrection".
    pub(crate) fn try_bump_live(&self) -> bool {
        let mut v = self.inner.load(Ordering::SeqCst);
        loop {
            if v >> STATE_SHIFT != STATE_LIVE_TAG {
                return false;
            }
            let new_v = v + 1;
            // Only 2^56 live connections could carry the +1 into the tag bits.
            debug_assert!(
                new_v >> STATE_SHIFT == STATE_LIVE_TAG,
                "SessionLifecycle live-count overflow"
            );
            match self
                .inner
                .compare_exchange_weak(v, new_v, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return true,
                Err(actual) => v = actual,
            }
        }
    }

    /// `true` iff this call took the `1→0` edge to `Dying`; see module doc "Death edge".
    pub(crate) fn drop_connection(&self) -> bool {
        let mut v = self.inner.load(Ordering::SeqCst);
        loop {
            if v >> STATE_SHIFT != STATE_LIVE_TAG {
                // Death declared: `v - 1` would underflow the tag and resurrect the session.
                return false;
            }
            let count = v & COUNT_MASK;
            debug_assert!(count >= 1, "Live state with count == 0");
            let (new_v, was_last) = if count == 1 {
                (encode_dying(), true)
            } else {
                (v - 1, false)
            };
            match self
                .inner
                .compare_exchange_weak(v, new_v, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return was_last,
                Err(actual) => v = actual,
            }
        }
    }

    /// `Live(1) → Dying` only, for a client losing its last slot; see module doc "Death edge".
    pub(crate) fn try_drop_sole_connection(&self) -> bool {
        self.inner
            .compare_exchange(
                encode_live(1),
                encode_dying(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Explicit shutdown: `Live(n) → Dying` from any `n`, once; see module doc "Death edge".
    pub(crate) fn force_dying(&self) -> bool {
        let mut v = self.inner.load(Ordering::SeqCst);
        loop {
            if v >> STATE_SHIFT != STATE_LIVE_TAG {
                return false;
            }
            match self.inner.compare_exchange_weak(
                v,
                encode_dying(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => v = actual,
            }
        }
    }

    /// `Dying → Dead`; the caller MUST have just fired the session obituaries.
    pub(crate) fn mark_dead(&self) {
        // CAS, not `swap`: an out-of-order call must not kill a `Live` session in release.
        let res = self.inner.compare_exchange(
            encode_dying(),
            encode_dead(),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        debug_assert!(res.is_ok(), "mark_dead called from non-Dying state");
    }
}

fn encode_live(n: usize) -> u64 {
    debug_assert!(n >= 1 && (n as u64) <= COUNT_MASK);
    // STATE_LIVE_TAG == 0, so the count alone is the encoding.
    n as u64
}

fn encode_dying() -> u64 {
    STATE_DYING_TAG << STATE_SHIFT
}

fn encode_dead() -> u64 {
    STATE_DEAD_TAG << STATE_SHIFT
}

#[cfg(test)] // used by snapshot(), itself the test exhaustive-match surface
fn decode(v: u64) -> SessionLifecycleSnapshot {
    match v >> STATE_SHIFT {
        STATE_LIVE_TAG => {
            let count = (v & COUNT_MASK) as usize;
            SessionLifecycleSnapshot::Live(
                NonZeroUsize::new(count).expect("Live state must have count >= 1"),
            )
        }
        STATE_DYING_TAG => SessionLifecycleSnapshot::Dying,
        STATE_DEAD_TAG => SessionLifecycleSnapshot::Dead,
        other => unreachable!("invalid SessionLifecycle state tag: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn new_session_is_live_one() {
        let lc = SessionLifecycle::new();
        assert_eq!(lc.snapshot(), SessionLifecycleSnapshot::Live(nz(1)));
        assert_eq!(lc.live_count(), 1);
        assert!(!lc.is_torn_down());
    }

    #[test]
    fn bump_then_decrement_back_to_one_stays_live() {
        let lc = SessionLifecycle::new();
        assert!(lc.try_bump_live()); // 1 → 2
        assert_eq!(lc.live_count(), 2);
        assert!(!lc.drop_connection()); // 2 → 1, not the last edge
        assert_eq!(lc.live_count(), 1);
        assert!(!lc.is_torn_down());
    }

    #[test]
    fn last_drop_transitions_through_dying_to_dead() {
        let lc = SessionLifecycle::new();
        assert!(lc.drop_connection()); // 1 → Dying, was_last
        assert_eq!(lc.snapshot(), SessionLifecycleSnapshot::Dying);
        assert_eq!(lc.live_count(), 0);
        assert!(lc.is_torn_down());

        lc.mark_dead();
        assert_eq!(lc.snapshot(), SessionLifecycleSnapshot::Dead);
        assert!(lc.is_torn_down());
    }

    #[test]
    fn try_bump_after_dying_or_dead_refuses() {
        let lc = SessionLifecycle::new();
        assert!(lc.drop_connection()); // → Dying
        assert!(!lc.try_bump_live(), "Dying must refuse new attach");
        lc.mark_dead();
        assert!(!lc.try_bump_live(), "Dead must refuse new attach");
    }

    /// Multi-attacker race: a bump-then-rollback `try_bump_live` lets an attacker reach `Dying`.
    #[test]
    fn concurrent_bumpers_against_drop_never_attach_to_dying() {
        for _ in 0..32 {
            let lc = Arc::new(SessionLifecycle::new());
            // Start at Live(2) to widen the attackers' window across the eventual 1→0 edge.
            assert!(lc.try_bump_live());
            assert_eq!(lc.live_count(), 2);

            let dropper = {
                let lc = Arc::clone(&lc);
                thread::spawn(move || {
                    // Drop twice: 2 → 1 → Dying.
                    let _ = lc.drop_connection();
                    let was_last = lc.drop_connection();
                    if was_last {
                        lc.mark_dead();
                    }
                })
            };

            let attackers: Vec<_> = (0..8)
                .map(|_| {
                    let lc = Arc::clone(&lc);
                    thread::spawn(move || {
                        let ok = lc.try_bump_live();
                        if ok {
                            // This attacker holds its own bump, so the state is still Live.
                            assert!(matches!(lc.snapshot(), SessionLifecycleSnapshot::Live(_)));
                            // Whoever observes the 1→0 edge, dropper or attacker, marks dead.
                            if lc.drop_connection() {
                                lc.mark_dead();
                            }
                        }
                        ok
                    })
                })
                .collect();

            dropper.join().unwrap();
            for a in attackers {
                let _ = a.join().unwrap();
            }

            // All bumps are repaid by join: `Live` = unpaid bump, `Dying` = skipped `mark_dead`.
            assert_eq!(lc.snapshot(), SessionLifecycleSnapshot::Dead);
        }
    }

    /// A worker's exit `drop_connection` after an emptied pool declared death is a quiet `false`.
    #[test]
    fn drop_connection_after_death_declared_elsewhere_is_quiet() {
        let lc = SessionLifecycle::new();
        assert!(lc.try_drop_sole_connection(), "Live(1) -> Dying");
        lc.mark_dead();
        assert!(
            !lc.drop_connection(),
            "the serve worker's exit must not report a second obituary edge"
        );
        assert!(lc.is_torn_down());
        // The reverse order stays intact: the worker wins, the pool hook loses.
        let lc = SessionLifecycle::new();
        assert!(lc.drop_connection(), "Live(1) -> Dying");
        assert!(!lc.try_drop_sole_connection());
        lc.mark_dead();
    }

    /// `force_dying` takes the edge from any live count, once; later `drop_connection`s are quiet.
    #[test]
    fn force_dying_from_live_n_takes_the_edge_exactly_once() {
        let lc = SessionLifecycle::new();
        assert!(lc.try_bump_live());
        assert!(lc.try_bump_live());
        assert_eq!(lc.live_count(), 3);
        assert!(lc.force_dying(), "Live(3) -> Dying");
        assert_eq!(lc.snapshot(), SessionLifecycleSnapshot::Dying);
        assert!(
            !lc.force_dying(),
            "a second caller does not own the death sequence"
        );
        assert!(
            !lc.drop_connection(),
            "the shut-down workers' exits are quiet"
        );
        assert!(!lc.try_bump_live(), "no attach after an explicit shutdown");
        lc.mark_dead();
        assert!(!lc.force_dying());
        assert_eq!(lc.snapshot(), SessionLifecycleSnapshot::Dead);
    }

    /// `drop_connection` from `Live(1)` shows `is_torn_down()` before `mark_dead` (reaper skip).
    #[test]
    fn dying_window_is_observable_to_hot_path_checks() {
        let lc = SessionLifecycle::new();
        assert!(!lc.is_torn_down());
        assert!(lc.drop_connection());
        // Now in Dying — caller hasn't called mark_dead yet.
        assert!(
            lc.is_torn_down(),
            "Dying must surface as is_torn_down() == true \
             before mark_dead, so drop-path reapers skip"
        );
        lc.mark_dead();
        assert!(lc.is_torn_down());
    }
}
