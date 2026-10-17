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
//! The publisher wakes each wait once (`Link::wake`), and `WAITING` carries the `seq` the wait
//! sleeps on, so it can tell which wake ends the wait. A wake from a commit whose `seq` is the
//! one the wait loaded ends nothing: the wait has seen that commit, and if the wake lands before
//! it enters the kernel, it sleeps on that `seq` all the same. Such a wait is neither woken nor
//! taken as woken; the next commit wakes it. A wake from any later commit always ends the wait,
//! in the kernel or at the compare, so after it the publisher sends nothing more to that wait.
//!
//! The other party is in this process: a death notice, an [`Interrupter`](crate::Interrupter), a
//! dropped `recv_async` future, a dropped subscriber. None of them can change `seq`, so a wake
//! that lands before the waiter enters the kernel would be lost. Each raises a bit in `stop`
//! instead, and while that bit is up and the waiter is `parked`, wakes again. The waiter raises
//! `parked` and then loads `stop`, so either it sees the bit and does not sleep, or the waker
//! sees `parked` and keeps waking until the waiter is out.
//!
//! A subscription woken through an fd (plan 15 §5.5a) keeps the protocol and changes what sleeps
//! and what wakes: the waiter loads `seq` once more after the fence and then polls its socket, and
//! a wake from this process is a byte sent to that socket, which stays until the wait drains it,
//! so it is sent once and never repeated.
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
use std::time::Instant;

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
    /// Put `seen`, the `seq` the wait sleeps on, in `state` and raise `WAITING`: one `SeqCst` RMW.
    fn begin_wait(&self, seen: u32);
    /// Drain an fd wake, then lower `WAITING` alone; `true` when the publisher's socket hung up.
    fn end_wait(&self) -> bool;
    /// Sleep while `seq` holds `seen`; `false` when `deadline` passed first.
    fn sleep(&self, seen: u32, deadline: Option<Instant>) -> bool;
    /// Wake the sleeper.
    fn wake(&self);
    /// Whether a wake sent before the sleep still ends it (a byte on the wake socket).
    fn latched(&self) -> bool;
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
    /// A test's pause after a wait raises `WAITING` and finds no bit up, before it sleeps.
    #[cfg(all(test, not(loom)))]
    pub(crate) before_sleep: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// The waits [`arm`](Self::arm) started, for a test that counts them.
    #[cfg(all(test, not(loom)))]
    pub(crate) arms: std::sync::atomic::AtomicUsize,
}

impl<W: Words> Local<W> {
    pub(crate) fn new() -> Self {
        Self {
            parked: AtomicBool::new(false),
            stop: AtomicU32::new(0),
            words: OnceLock::new(),
            #[cfg(all(test, not(loom)))]
            after_sleep: std::sync::Mutex::new(None),
            #[cfg(all(test, not(loom)))]
            before_sleep: std::sync::Mutex::new(None),
            #[cfg(all(test, not(loom)))]
            arms: std::sync::atomic::AtomicUsize::new(0),
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
    pub(crate) fn park(&self, seen: u32, deadline: Option<Instant>) -> Slept {
        let stopped = self.arm(seen) != 0;
        #[cfg(all(test, not(loom)))]
        if !stopped {
            if let Some(pause) = self.before_sleep.lock().expect("lock").as_ref() {
                pause();
            }
        }
        let slept = if stopped {
            Slept::Stopped
        } else if self.words().sleep(seen, deadline) {
            Slept::Woke
        } else {
            Slept::TimedOut
        };
        self.disarm();
        slept
    }

    /// Start a wait on `seen`: raise `WAITING` with it and `parked`; the bits up in `stop` after.
    pub(crate) fn arm(&self, seen: u32) -> u32 {
        #[cfg(all(test, not(loom)))]
        self.arms.fetch_add(1, Ordering::Relaxed);
        self.words().begin_wait(seen);
        self.parked.store(true, Ordering::SeqCst);
        // Against the publisher's fence after `seq` and a waker's after `stop`: each side sees one.
        fence(Ordering::SeqCst);
        self.stop.load(Ordering::SeqCst)
    }

    /// End the wait [`arm`](Self::arm) started, whether it slept or not.
    pub(crate) fn disarm(&self) {
        // Also when it did not sleep: a waker of `DIED` loops until it sees this.
        self.parked.store(false, Ordering::SeqCst);
        #[cfg(all(test, not(loom)))]
        if let Some(pause) = self.after_sleep.lock().expect("lock").as_ref() {
            pause();
        }
        if self.words().end_wait() {
            // The publisher closes its end only as its process dies or after the end record.
            self.stop.fetch_or(DIED, Ordering::SeqCst);
        }
    }

    /// Raise `bit` and wake the wait until it is out or the bit is cleared; the wakes it sent.
    pub(crate) fn raise(&self, bit: u32) -> usize {
        self.stop.fetch_or(bit, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let Some(words) = self.words.get() else {
            return 0;
        };
        if words.latched() {
            words.wake();
            return 1;
        }
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
    use std::time::Instant;

    use rsbinder::fmq::shm::{Deadline, Mapping};
    use rsbinder::fmq::Error;
    use rsbinder::{DeathRecipient, WIBinder};

    use super::{Local, Words, DIED};
    use crate::layout::{seen_field, ControlPage, Seq, CLOSED, WAITING};
    use crate::wake::FdWake;

    /// The words of one subscription: `seq` in the topic memory, `state` in the control page.
    pub(crate) struct Parts {
        pub(crate) seq: Seq<Arc<Mapping>>,
        pub(crate) control: Arc<ControlPage>,
        /// The fds of a subscription the publisher wakes through its socket.
        pub(crate) fd: Option<FdWake>,
    }

    impl Words for Parts {
        fn seq(&self) -> u32 {
            self.seq.word().load_acquire()
        }

        fn begin_wait(&self, seen: u32) {
            let state = self.control.state();
            let mut old = state.load(Ordering::Relaxed);
            // `CLOSED` is the other bit raised meanwhile, by the subscriber's drop.
            while let Err(now) = state.compare_exchange_weak(
                old,
                old & CLOSED | seen_field(seen) | WAITING,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                old = now;
            }
        }

        fn end_wait(&self) -> bool {
            // A byte sent for this wait after the drain makes the next wait look once for nothing.
            let hung_up = self.fd.as_ref().is_some_and(FdWake::drain);
            self.control.state().fetch_and(!WAITING, Ordering::SeqCst);
            hung_up
        }

        fn sleep(&self, seen: u32, deadline: Option<Instant>) -> bool {
            if let Some(fd) = &self.fd {
                // What the futex's compare does in the kernel: after `WAITING`, `seq` once more.
                return self.seq() != seen || fd.sleep(deadline);
            }
            let deadline = match deadline {
                None => None,
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    Deadline::after(left)
                }
            };
            // Any other error is a sleep that did not happen: the caller looks and comes back.
            !matches!(
                self.seq.word().wait(seen, u32::MAX, deadline.as_ref()),
                Err(Error::TimedOut)
            )
        }

        fn wake(&self) {
            match &self.fd {
                Some(fd) => fd.poke(),
                // Fails only where the system call does not exist, and then nobody sleeps on it.
                None => {
                    let _ = self.seq.word().wake(u32::MAX);
                }
            }
        }

        fn latched(&self) -> bool {
            self.fd.is_some()
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
