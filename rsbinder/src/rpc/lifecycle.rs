// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Typed session lifecycle.
//!
//! The live connection count and the obituary state of
//! [`super::session::SharedSession`] share a single atomic-backed state
//! machine:
//!
//! ```text
//!     new() ──→  Live(n: NonZeroUsize) ──force_dying (any n)──→  Dying ──mark_dead──→ Dead
//!                  │  ▲
//!                  └──┘ try_bump_live increments
//! ```
//!
//! Encoding (single `AtomicU64`, lock-free on every supported target):
//! the top byte is a state tag, the lower 56 bits are the `Live` count.
//! `Dying`/`Dead` carry no count. Every transition is a CAS. The typed
//! enum makes the three reachable states explicit:
//! a reviewer can grep `SessionLifecycleSnapshot` variants for every
//! point that branches on them.
//!
//! The count is the number of serve-driven connections: the founding one
//! and every server attach. It only grows while the session is `Live`,
//! because no connection leaves a live session: a fault on any connection
//! ends the whole session (`session` module doc "Session end"). The
//! `is_torn_down` snapshot can return `true` in the `Dying` window
//! *before* the obituary completes, so a `RpcProxy::drop` that races the
//! death sequence skips its best-effort `DEC_STRONG` without waiting for
//! the obituary callback to return, and never deadlocks on an empty pool.
//!
//! # Encoding
//!
//! 56 count bits exceed any plausible live connection count and leave a
//! margin against the `Live`-tag `+1` overflow path; the word is `u64` so
//! the encoded state stays one lock-free word on every supported target.
//!
//! # Anti-resurrection
//!
//! Once the session left `Live`, `try_bump_live` never succeeds: a session
//! whose obituaries fired must never acquire another driver, else
//! `binder_died` is silently lost for any `DeathRecipient` linked through
//! the attaching connection. `try_bump_live` decides on the loaded value
//! inside its CAS, so a bump racing `force_dying` either lands on `Live`
//! (and the death sequence shuts its connection down with the rest) or is
//! refused.
//!
//! # Death edge
//!
//! `force_dying` is the one transition out of `Live`, `Live(n) → Dying`
//! from any `n`, and exactly one caller gets `true` from it, which is what
//! the obituary contract needs. That caller fires session obituaries and
//! then calls `mark_dead`; every other caller finds a settled state and
//! gets a quiet `false`. The live count is discarded on purpose: the death
//! sequence shuts every connection down.

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
    /// The session is up; `n` counts its serve-driven connections.
    Live(NonZeroUsize),
    /// The session is ending; obituaries are firing. Becomes `Dead` when they return.
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

    /// Session end: `Live(n) → Dying` from any `n`, once; see module doc "Death edge".
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
    fn try_bump_after_dying_or_dead_refuses() {
        let lc = SessionLifecycle::new();
        assert!(lc.force_dying()); // → Dying
        assert!(!lc.try_bump_live(), "Dying must refuse new attach");
        lc.mark_dead();
        assert!(!lc.try_bump_live(), "Dead must refuse new attach");
    }

    /// Bumps racing `force_dying` land on `Live` or are refused; exactly one caller owns the edge.
    #[test]
    fn concurrent_bumpers_against_force_dying_never_attach_to_dying() {
        for _ in 0..32 {
            let lc = Arc::new(SessionLifecycle::new());
            let ender = {
                let lc = Arc::clone(&lc);
                thread::spawn(move || lc.force_dying())
            };
            let attackers: Vec<_> = (0..8)
                .map(|_| {
                    let lc = Arc::clone(&lc);
                    thread::spawn(move || {
                        let ok = lc.try_bump_live();
                        // A refused bump means the session had already left `Live`.
                        assert!(ok || lc.is_torn_down());
                        lc.force_dying()
                    })
                })
                .collect();
            let mut edges = usize::from(ender.join().unwrap());
            for a in attackers {
                edges += usize::from(a.join().unwrap());
            }
            assert_eq!(edges, 1, "exactly one caller runs the death sequence");
            assert!(!lc.try_bump_live());
            lc.mark_dead();
            assert_eq!(lc.snapshot(), SessionLifecycleSnapshot::Dead);
        }
    }

    /// `force_dying` takes the edge from any live count, once.
    #[test]
    fn force_dying_from_live_n_takes_the_edge_exactly_once() {
        let lc = SessionLifecycle::new();
        assert!(lc.try_bump_live());
        assert!(lc.try_bump_live());
        assert_eq!(lc.live_count(), 3);
        assert!(lc.force_dying(), "Live(3) -> Dying");
        assert_eq!(lc.snapshot(), SessionLifecycleSnapshot::Dying);
        assert_eq!(lc.live_count(), 0);
        assert!(
            !lc.force_dying(),
            "a second caller does not own the death sequence"
        );
        assert!(!lc.try_bump_live(), "no attach after the session ended");
        lc.mark_dead();
        assert!(!lc.force_dying());
        assert_eq!(lc.snapshot(), SessionLifecycleSnapshot::Dead);
    }

    /// `force_dying` shows `is_torn_down()` before `mark_dead` (reaper skip).
    #[test]
    fn dying_window_is_observable_to_hot_path_checks() {
        let lc = SessionLifecycle::new();
        assert!(!lc.is_torn_down());
        assert!(lc.force_dying());
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
