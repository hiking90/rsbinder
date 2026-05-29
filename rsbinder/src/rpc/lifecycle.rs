// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Typed session lifecycle.
//!
//! Replaces the prior `live_conns: AtomicUsize` + `obituary_sent:
//! AtomicBool` pair on [`super::session::SharedSession`] with a single
//! atomic-backed state machine:
//!
//! ```text
//!         try_bump_live ────┐
//!                           ▼
//!     ┌─── new() ──→  Live(n: NonZeroUsize) ───drop_connection (n==1)──→  Dying ──mark_dead──→ Dead
//!                           │  ▲
//!                           │  └─── drop_connection (n>1) decrements
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
//! a reviewer can grep [`SessionLifecycleSnapshot`] variants for every
//! point that branches on them.
//!
//! **Default single-connection sessions** never leave `Live(1)` until
//! teardown. The `is_torn_down` snapshot can return `true` in
//! the `Dying` window *before* the obituary completes — a strict
//! improvement (a `RpcProxy::drop` reaper that races the dying founding
//! worker skips its best-effort `DEC_STRONG` slightly earlier, never
//! deadlocking on an empty pool; the prior `obituary_sent.load` only
//! flipped after the obituary callback returned).

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};

/// Top byte holds the state tag; lower 56 bits hold the `Live` count
/// (0 in non-`Live` states). 56 bits is enough for any plausible live
/// connection count and leaves a clear margin against the `Live`-tag
/// `+1` overflow path; the wider type is `u64` to keep the encoded
/// state in a single lock-free word on every supported target.
const STATE_SHIFT: u32 = 56;
const COUNT_MASK: u64 = (1u64 << STATE_SHIFT) - 1;
const STATE_LIVE_TAG: u64 = 0;
const STATE_DYING_TAG: u64 = 1;
const STATE_DEAD_TAG: u64 = 2;

/// A read-only snapshot of [`SessionLifecycle`] — the type the rest of
/// the RPC crate matches against. Production code rarely needs every
/// variant individually; the convenience helpers on `SessionLifecycle`
/// ([`SessionLifecycle::is_torn_down`], [`SessionLifecycle::live_count`])
/// cover the common cases. The unit tests below match against every
/// variant explicitly.
#[allow(dead_code)]
// The hot path uses the boolean/count helpers; the typed snapshot is the grep-friendly exhaustive-match surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionLifecycleSnapshot {
    /// At least one connection is still driving the session. `n` is the
    /// current live connection count.
    Live(NonZeroUsize),
    /// The last `Live` connection dropped; the founding worker is
    /// firing session obituaries. Transient — moves to `Dead` once the
    /// obituary callback returns.
    Dying,
    /// Obituary callbacks have completed. A subsequent
    /// [`SessionLifecycle::try_bump_live`] never succeeds (anti-
    /// resurrection — a session whose obituaries fired must never
    /// acquire another driver, else `binder_died` would be silently
    /// lost for any `DeathRecipient` linked through the attaching
    /// connection).
    Dead,
}

/// Single atomic-backed session lifecycle. Stores state + live count in
/// one `AtomicU64`; every method is lock-free.
pub(crate) struct SessionLifecycle {
    inner: AtomicU64,
}

impl SessionLifecycle {
    /// Initial state for a newly-minted session — `Live(1)` (the
    /// founding connection).
    pub(crate) fn new() -> Self {
        Self {
            inner: AtomicU64::new(encode_live(1)),
        }
    }

    /// Lock-free typed snapshot. Production code prefers
    /// [`is_torn_down`](Self::is_torn_down) (boolean) or
    /// [`live_count`](Self::live_count) (numeric) for the hot path;
    /// `snapshot` is the test surface that exhaustively matches every
    /// variant.
    #[allow(dead_code)]
    pub(crate) fn snapshot(&self) -> SessionLifecycleSnapshot {
        decode(self.inner.load(Ordering::SeqCst))
    }

    /// Hot-path "is this session past its `Live` window?" check (used
    /// by `RpcProxy::drop`, the reaper, and `add_callback_slot`).
    /// `true` in both `Dying` and `Dead` — neither admits new work.
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

    /// **Anti-resurrection primitive.** Atomically bump the live
    /// count *if* the session is still `Live`. Returns `false` from
    /// `Dying` or `Dead` (never attaches to a session whose obituaries
    /// fired / are firing).
    ///
    /// CAS-loop rather than optimistic-bump-then-rollback — the latter
    /// closes the *single-attacker* race but not the
    /// **multi-attacker** one: two concurrent
    /// attackers A and B both bumping after the founding `fetch_sub`'d
    /// to 0 could let B see A's transient `prev=1` before A rolls
    /// back, attaching to a dying session. Value-decision CAS closes
    /// it.
    pub(crate) fn try_bump_live(&self) -> bool {
        let mut v = self.inner.load(Ordering::SeqCst);
        loop {
            if v >> STATE_SHIFT != STATE_LIVE_TAG {
                return false;
            }
            let new_v = v + 1;
            // The +1 stays inside the count bits unless the count
            // overflowed 56-bit range. Defensive — never reached in
            // practice (a session would need 2^56 live connections).
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

    /// Connection-teardown counterpart of [`try_bump_live`]. Returns
    /// `true` iff *this* call observed the `1→0` edge (the last
    /// connection); the caller is responsible for firing session
    /// obituaries and then calling [`mark_dead`](Self::mark_dead).
    ///
    /// In a `Live(n)` state with `n > 1` this CAS-decrements and
    /// returns `false`. In `Live(1)` it CAS-transitions to `Dying` and
    /// returns `true`. Calling from `Dying`/`Dead` is a contract
    /// violation (only one founding worker observes the `1→0` edge);
    /// a `debug_assert` catches it.
    pub(crate) fn drop_connection(&self) -> bool {
        let mut v = self.inner.load(Ordering::SeqCst);
        loop {
            debug_assert_eq!(
                v >> STATE_SHIFT,
                STATE_LIVE_TAG,
                "drop_connection called from non-Live state"
            );
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

    /// Transition `Dying → Dead`. The caller MUST have just fired
    /// session obituaries (the only path that reaches `Dying`).
    pub(crate) fn mark_dead(&self) {
        let prev = self.inner.swap(encode_dead(), Ordering::SeqCst);
        debug_assert_eq!(
            prev >> STATE_SHIFT,
            STATE_DYING_TAG,
            "mark_dead called from non-Dying state"
        );
    }
}

fn encode_live(n: usize) -> u64 {
    debug_assert!(n >= 1 && (n as u64) <= COUNT_MASK);
    // STATE_LIVE_TAG == 0 so no shift needed; the count occupies the
    // lower 56 bits unchanged.
    n as u64
}

fn encode_dying() -> u64 {
    STATE_DYING_TAG << STATE_SHIFT
}

fn encode_dead() -> u64 {
    STATE_DEAD_TAG << STATE_SHIFT
}

#[allow(dead_code)] // used by snapshot(), itself test/grep surface
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

    /// Multi-attacker hole, hermetic regression. A naive
    /// optimistic-bump-then-rollback shape would let one of N
    /// concurrent attackers attach to a dying session by observing
    /// another's transient bump. The CAS-loop primitive — which the
    /// typed enum here preserves — must keep `Dying`/`Dead` invariant
    /// under any interleaving.
    #[test]
    fn concurrent_bumpers_against_drop_never_attach_to_dying() {
        for _ in 0..32 {
            let lc = Arc::new(SessionLifecycle::new());
            // Pre-bump so the founding's drop transitions Live(2) →
            // Live(1) (not directly to Dying), giving N attackers a
            // wider race window across the eventual 1→0 edge.
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
                        // Whoever bumped must un-bump (preserve the
                        // arithmetic so the test asserts the final state
                        // from `Dying` is reachable). Concurrent unbump
                        // from Dying/Dead is also forbidden — only one
                        // dropper here.
                        if ok {
                            // Re-validate: count must still be Live.
                            assert!(matches!(lc.snapshot(), SessionLifecycleSnapshot::Live(_)));
                            let _ = lc.drop_connection();
                        }
                        ok
                    })
                })
                .collect();

            dropper.join().unwrap();
            for a in attackers {
                let _ = a.join().unwrap();
            }

            // Final state is Dead OR Live (depending on whether any
            // attacker raced past the dropper) — but never `Live(0)`
            // and never a `try_bump_live` *succeeded against `Dying`*.
            match lc.snapshot() {
                SessionLifecycleSnapshot::Dead => {}
                SessionLifecycleSnapshot::Live(_) => {
                    // An attacker won the race; the session is genuinely
                    // alive again (NOT a resurrection of a dead session
                    // — the attacker bumped while still Live, then the
                    // dropper saw a higher count and didn't 1→0).
                }
                SessionLifecycleSnapshot::Dying => {
                    panic!("Dying is transient — should have moved to Dead via mark_dead");
                }
            }
        }
    }

    /// Hermetic regression: a `drop_connection` from `Live(1)` must
    /// expose `is_torn_down() == true` *before* `mark_dead` is called.
    /// This is the strict improvement over the prior `obituary_sent`
    /// scheme — a racing `RpcProxy::drop` reaper now sees the
    /// `Dying` state and skips immediately instead of blocking on an
    /// empty slot pool.
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
