// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A subscriber's wait for a record, and the wakes that end it (plan 15 §5.5).
//!
//! Two parties end a wait. The publisher, in another process or this one, after a commit: it
//! bumps `seq`, then loads the subscriber's `state`, and wakes if `WAITING` is up. The waiter
//! loads `seq`, looks at the ring once more, raises `WAITING` and sleeps while `seq` holds what it
//! loaded. Both sides change their word and then look at the other's with `SeqCst`, so one of
//! them sees the other: the publisher sees `WAITING`, or the kernel's compare sees the new `seq`.
//!
//! The other party is in this process: a death notice, an [`Interrupter`](crate::Interrupter), a
//! dropped `recv_async` future, a dropped subscriber. None of them can change `seq`, so a wake
//! that lands before the waiter enters the kernel would be lost. Each raises a bit in `stop`
//! instead, and while that bit is up and the waiter is `parked`, wakes again. The waiter raises
//! `parked` and then loads `stop`, so either it sees the bit and does not sleep, or the waker
//! sees `parked` and keeps waking until the waiter is out.
//!
//! One wait at a time per subscriber: the code that waits joins a wait left on the blocking
//! pool before it looks at the ring (`Leftover`), so `WAITING` and `parked` have one owner.

#[cfg(loom)]
use loom::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};
#[cfg(loom)]
use loom::thread::yield_now;
#[cfg(not(loom))]
use std::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;
#[cfg(not(loom))]
use std::thread::yield_now;

use rsbinder::fmq::shm::Deadline;

/// `stop`: an [`Interrupter`](crate::Interrupter) asked; the wait that sees it clears it.
pub(crate) const INTERRUPT: u32 = 1;
/// `stop`: the wait on the pool has nobody to report to; that wait, or the next join, clears it.
pub(crate) const ABANDON: u32 = 2;
/// `stop`: the publisher's process died. Never cleared, and no wait sleeps while it is up.
pub(crate) const DIED: u32 = 4;

/// Yields a waker looks before waking again: a wake is lost only before the waiter's syscall.
#[cfg(not(loom))]
const RECHECKS: u32 = 64;
/// Under loom one: every yield is a branch point of the model, and one is enough to reach both.
#[cfg(loom)]
const RECHECKS: u32 = 1;

/// What a wait sleeps on: the topic's `seq` and the subscriber's `state`.
pub(crate) trait Words: Send + Sync {
    /// `seq`, ordered before the caller's next look at the ring.
    fn seq(&self) -> u32;
    /// Count one more wait in `state`, then raise `WAITING`; both `SeqCst`.
    fn begin_wait(&self);
    /// Lower `WAITING` alone: `CLOSED` and the count stay as they are.
    fn end_wait(&self);
    /// Sleep while `seq` holds `seen`; `false` when `deadline` passed first.
    fn sleep(&self, seen: u32, deadline: Option<&Deadline>) -> bool;
    /// Wake every sleeper on `seq`.
    fn wake(&self);
}

/// How [`Local::park`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slept {
    /// It slept and was woken, or `seq` had moved: look at the ring again.
    Woke,
    /// The deadline passed.
    TimedOut,
    /// A bit in `stop` was up, so it did not sleep.
    Stopped,
}

/// One subscriber's waiting state in this process.
pub(crate) struct Local<W> {
    parked: AtomicBool,
    stop: AtomicU32,
    /// Set once the topic memory is mapped; a death before that has nothing to wake.
    words: OnceLock<W>,
    /// A test's pause after a wait lowers `parked`, before it lowers `WAITING`.
    #[cfg(all(test, not(loom)))]
    pub(crate) after_sleep: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl<W: Words> Local<W> {
    pub(crate) fn new() -> Self {
        Self {
            parked: AtomicBool::new(false),
            stop: AtomicU32::new(0),
            words: OnceLock::new(),
            #[cfg(all(test, not(loom)))]
            after_sleep: std::sync::Mutex::new(None),
        }
    }

    /// The words to wait on; before any wait or wake from another thread.
    pub(crate) fn set_words(&self, words: W) {
        assert!(self.words.set(words).is_ok(), "words set twice");
    }

    pub(crate) fn words(&self) -> &W {
        self.words.get().expect("words set before a wait")
    }

    /// One wait: sleep on `seq` while it holds `seen`, unless a bit in `stop` is up.
    pub(crate) fn park(&self, seen: u32, deadline: Option<&Deadline>) -> Slept {
        let words = self.words();
        words.begin_wait();
        self.parked.store(true, Ordering::SeqCst);
        // Against the publisher's fence after `seq` and a waker's after `stop`: each side sees one.
        fence(Ordering::SeqCst);
        let slept = if self.stop.load(Ordering::SeqCst) != 0 {
            Slept::Stopped
        } else if words.sleep(seen, deadline) {
            Slept::Woke
        } else {
            Slept::TimedOut
        };
        // Also when it did not sleep: a waker of `DIED` loops until it sees this.
        self.parked.store(false, Ordering::SeqCst);
        #[cfg(all(test, not(loom)))]
        if let Some(pause) = self.after_sleep.lock().expect("lock").as_ref() {
            pause();
        }
        words.end_wait();
        slept
    }

    /// Raise `bit` and wake the wait until it is out or the bit is cleared; the wakes it sent.
    pub(crate) fn raise(&self, bit: u32) -> usize {
        self.stop.fetch_or(bit, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let Some(words) = self.words.get() else {
            return 0;
        };
        let waiting =
            || self.stop.load(Ordering::SeqCst) & bit != 0 && self.parked.load(Ordering::SeqCst);
        let mut wakes = 0;
        while waiting() {
            words.wake();
            wakes += 1;
            // A woken thread takes a while to run; wake again only if this one may have been lost.
            for _ in 0..RECHECKS {
                yield_now();
                if !waiting() {
                    return wakes;
                }
            }
        }
        wakes
    }

    /// Clear `bit`; whether it was up.
    pub(crate) fn take(&self, bit: u32) -> bool {
        self.stop.fetch_and(!bit, Ordering::SeqCst) & bit != 0
    }

    /// Whether a wait is between raising `parked` and lowering it.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn is_parked(&self) -> bool {
        self.parked.load(Ordering::SeqCst)
    }

    /// Whether any bit is up: a spin before a wait stops at it.
    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed) != 0
    }

    /// Whether the publisher's process died. Loaded before the ring: the read finds its commits.
    pub(crate) fn died(&self) -> bool {
        self.stop.load(Ordering::Acquire) & DIED != 0
    }
}

pub(crate) use real::{Died, Parts};

/// The words in shared memory, and the death notice. Compiled under loom too, never run there.
mod real {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use rsbinder::fmq::shm::{Deadline, Mapping};
    use rsbinder::fmq::Error;
    use rsbinder::{DeathRecipient, WIBinder};

    use super::{Local, Words, DIED};
    use crate::layout::{ControlPage, Seq, WAITING, WAIT_COUNT};

    /// The words of one subscription: `seq` in the topic memory, `state` in the control page.
    pub(crate) struct Parts {
        pub(crate) seq: Seq<Arc<Mapping>>,
        pub(crate) control: Arc<ControlPage>,
    }

    impl Words for Parts {
        fn seq(&self) -> u32 {
            self.seq.word().load_acquire()
        }

        fn begin_wait(&self) {
            let state = self.control.state();
            state.fetch_add(WAIT_COUNT, Ordering::SeqCst);
            state.fetch_or(WAITING, Ordering::SeqCst);
        }

        fn end_wait(&self) {
            self.control.state().fetch_and(!WAITING, Ordering::SeqCst);
        }

        fn sleep(&self, seen: u32, deadline: Option<&Deadline>) -> bool {
            // Any other error is a sleep that did not happen: the caller looks and comes back.
            !matches!(
                self.seq.word().wait(seen, u32::MAX, deadline),
                Err(Error::TimedOut)
            )
        }

        fn wake(&self) {
            // Fails only where the system call does not exist, and then nobody sleeps on it.
            let _ = self.seq.word().wake(u32::MAX);
        }
    }

    /// The recipient on the publisher's binder: raises `DIED` and wakes the wait.
    pub(crate) struct Died(pub(crate) Arc<Local<Parts>>);

    impl DeathRecipient for Died {
        fn binder_died(&self, _: &WIBinder) {
            self.0.raise(DIED);
        }
    }
}

#[cfg(feature = "tokio")]
pub(crate) use pool::{on_pool, Leftover};

/// A wait handed to the blocking pool, and the record of it the next wait joins.
#[cfg(feature = "tokio")]
mod pool {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    use rsbinder::{BinderAsyncPool, BoxFuture, StatusCode};

    use super::{Local, Parts, ABANDON};

    /// Whether a wait is out on the pool: at most one, which the next wait joins first.
    #[derive(Default)]
    pub(crate) struct Leftover {
        busy: Mutex<bool>,
        idle: Condvar,
        notify: tokio::sync::Notify,
    }

    impl Leftover {
        fn lock(&self) -> std::sync::MutexGuard<'_, bool> {
            self.busy.lock().unwrap_or_else(|e| e.into_inner())
        }

        pub(crate) fn busy(&self) -> bool {
            *self.lock()
        }

        fn begin(&self) {
            *self.lock() = true;
        }

        fn settle(&self) {
            *self.lock() = false;
            self.idle.notify_all();
            self.notify.notify_waiters();
        }

        /// Block until no wait is out; `false` if `deadline` passes first.
        pub(crate) fn join(&self, deadline: Option<Instant>) -> bool {
            let mut busy = self.lock();
            while *busy {
                let Some(deadline) = deadline else {
                    busy = self.idle.wait(busy).unwrap_or_else(|e| e.into_inner());
                    continue;
                };
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return false;
                }
                busy = self
                    .idle
                    .wait_timeout(busy, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            true
        }

        pub(crate) async fn join_async(&self) {
            loop {
                // Made before the check: a `Notified` gets `notify_waiters` from its creation on.
                let notified = self.notify.notified();
                if !self.busy() {
                    return;
                }
                notified.await;
            }
        }
    }

    /// The wait the pool runs; it settles the record when dropped, run or not.
    struct Handed {
        local: Arc<Local<Parts>>,
        leftover: Arc<Leftover>,
        seen: u32,
    }

    impl Handed {
        fn run(self) {
            self.local.park(self.seen, None);
            // Its own, if the future was dropped meanwhile: ends the dropper's wakes at once.
            self.local.take(ABANDON);
        }
    }

    impl Drop for Handed {
        fn drop(&mut self) {
            self.leftover.settle();
        }
    }

    /// Takes the wait back when a hand-off queues it and then unwinds, as `spawn_blocking` can.
    struct TakeBack {
        slot: Arc<Mutex<Option<Handed>>>,
        armed: bool,
    }

    impl Drop for TakeBack {
        fn drop(&mut self) {
            if self.armed {
                let handed = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                drop(handed);
            }
        }
    }

    /// One wait for `seen` on the pool, handed over now; `leftover` records it until it is done.
    pub(crate) fn on_pool(
        local: &Arc<Local<Parts>>,
        leftover: &Arc<Leftover>,
        seen: u32,
    ) -> BoxFuture<'static, Result<(), StatusCode>> {
        leftover.begin();
        let slot = Arc::new(Mutex::new(Some(Handed {
            local: local.clone(),
            leftover: leftover.clone(),
            seen,
        })));
        let mut take_back = TakeBack {
            slot: slot.clone(),
            armed: true,
        };
        let handed = <rsbinder::Tokio as BinderAsyncPool>::spawn(
            move || {
                let handed = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(handed) = handed {
                    handed.run();
                }
            },
            |()| async { Ok(()) },
        );
        take_back.armed = false;
        handed
    }
}

#[cfg(all(test, loom))]
mod loom_tests;
