// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Whole-phase deadlines (plan 2-25 D3).
//!
//! A socket deadline (`SO_RCVTIMEO`, `SO_SNDTIMEO`) bounds one wait: a peer
//! that sends a byte every so often starts it over each time, so a handshake
//! read one byte at a time never ends. A [`PhaseDeadline`] bounds a phase as a
//! whole. When it passes, the deadline runs the connection's cut
//! ([`RpcTransport::shutdown_handle`](super::RpcTransport::shutdown_handle)),
//! which shuts the socket down: the read or write the phase is blocked in
//! fails, and the code that armed the deadline asks [`PhaseDeadline::fired`]
//! to report that failure as the deadline's.
//!
//! Each armed deadline has a thread of its own, which sleeps until the
//! deadline or the phase's end, whichever comes first, and then exits. The
//! RPC stack keeps no process-wide state (`rpc_server`'s
//! `rpc_stack_has_no_globals`), so there is no shared timer to hand it to. If
//! the thread cannot be started the deadline does nothing and says so in the
//! log: the socket deadlines still bound each wait.
//!
//! The cut runs under the deadline's lock, and disarming takes the same lock,
//! so once [`PhaseDeadline::disarm`] has returned `true` the cut never runs.
//! Dropping a deadline disarms it, so an early return ends the phase too.
//!
//! [`PhaseDeadline::arm_with`] makes the cut only when a duration is set,
//! since making one duplicates the socket's fd. A duration too long for an
//! [`Instant`] to hold (`Duration::MAX`, which the timeout setters accept)
//! arms no deadline, as `SO_RCVTIMEO` saturates such a value to "never".

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What a deadline runs when it passes: shut the connection down.
pub(crate) type Cut = Box<dyn FnOnce() + Send>;

enum State {
    Armed(Cut),
    Disarmed,
    Fired,
}

struct Shared {
    state: Mutex<State>,
    ended: Condvar,
}

impl Shared {
    /// The deadline thread: wait for the phase's end until `at`, then cut if it has not ended.
    fn watch(&self, at: Instant) {
        let mut state = self.state.lock().expect("deadline state poisoned");
        loop {
            if !matches!(*state, State::Armed(_)) {
                return;
            }
            let Some(left) = at
                .checked_duration_since(Instant::now())
                .filter(|l| !l.is_zero())
            else {
                break;
            };
            state = self
                .ended
                .wait_timeout(state, left)
                .expect("deadline state poisoned")
                .0;
        }
        if let State::Armed(cut) = std::mem::replace(&mut *state, State::Fired) {
            cut();
        }
    }
}

/// A deadline on a whole phase of one connection, disarmed on drop; see the module doc.
pub(crate) struct PhaseDeadline {
    shared: Option<Arc<Shared>>,
}

impl PhaseDeadline {
    /// No deadline: `disarm` is `true`, `fired` is `false`.
    pub(crate) fn none() -> Self {
        PhaseDeadline { shared: None }
    }

    /// Run `cut` once `after` has passed, unless the phase is disarmed first.
    pub(crate) fn arm(after: Duration, cut: Cut) -> Self {
        // Beyond what an `Instant` holds: never passes (module doc).
        let Some(at) = Instant::now().checked_add(after) else {
            return Self::none();
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State::Armed(cut)),
            ended: Condvar::new(),
        });
        let watching = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("rpc-deadline".into())
            .spawn(move || watching.watch(at));
        match spawned {
            Ok(_) => PhaseDeadline {
                shared: Some(shared),
            },
            Err(e) => {
                log::warn!(
                    "rsbinder RPC: cannot start a deadline thread ({e}); this phase is bounded \
                     per read only"
                );
                Self::none()
            }
        }
    }

    /// `after` with the cut `cut` makes, or none if either is missing; `cut` runs only on `after`.
    pub(crate) fn arm_with(after: Option<Duration>, cut: impl FnOnce() -> Option<Cut>) -> Self {
        match after.and_then(|after| cut().map(|cut| (after, cut))) {
            Some((after, cut)) => Self::arm(after, cut),
            None => Self::none(),
        }
    }

    /// End the phase: `true` if it ended in time, `false` if the deadline already cut it.
    pub(crate) fn disarm(&mut self) -> bool {
        let Some(shared) = self.shared.take() else {
            return true;
        };
        let mut state = shared.state.lock().expect("deadline state poisoned");
        if let State::Fired = *state {
            return false;
        }
        *state = State::Disarmed;
        drop(state);
        shared.ended.notify_one();
        true
    }

    /// Whether the deadline passed and cut the connection (before `disarm`).
    pub(crate) fn fired(&self) -> bool {
        self.shared.as_ref().is_some_and(|shared| {
            let state = shared.state.lock().expect("deadline state poisoned");
            matches!(*state, State::Fired)
        })
    }
}

impl Drop for PhaseDeadline {
    fn drop(&mut self) {
        self.disarm();
    }
}

impl std::fmt::Debug for PhaseDeadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhaseDeadline")
            .field("armed", &self.shared.is_some())
            .field("fired", &self.fired())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counting() -> (Arc<AtomicUsize>, Cut) {
        let cuts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&cuts);
        (
            cuts,
            Box::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    fn wait_for(f: impl Fn() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        f()
    }

    #[test]
    fn a_deadline_that_passes_cuts_once() {
        let (cuts, cut) = counting();
        let mut d = PhaseDeadline::arm(Duration::from_millis(30), cut);
        assert!(wait_for(|| d.fired()), "the deadline never fired");
        assert_eq!(cuts.load(Ordering::SeqCst), 1);
        assert!(!d.disarm(), "a phase the deadline cut did not end in time");
    }

    #[test]
    fn a_phase_that_ends_in_time_is_never_cut() {
        let (cuts, cut) = counting();
        // Inside the sleep below, so a disarm that leaves the cut armed fails the test.
        let mut d = PhaseDeadline::arm(Duration::from_millis(300), cut);
        assert!(d.disarm());
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(cuts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dropping_the_deadline_disarms_it() {
        let (cuts, cut) = counting();
        drop(PhaseDeadline::arm(Duration::from_millis(300), cut));
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(cuts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_deadline_past_what_an_instant_holds_is_none() {
        let (cuts, cut) = counting();
        let mut d = PhaseDeadline::arm(Duration::MAX, cut);
        assert!(!d.fired());
        assert!(d.disarm());
        assert_eq!(cuts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn no_cut_means_no_deadline() {
        let mut d = PhaseDeadline::arm_with(Some(Duration::from_millis(1)), || None);
        std::thread::sleep(Duration::from_millis(20));
        assert!(!d.fired());
        assert!(d.disarm());
    }
}
