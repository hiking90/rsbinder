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
//! The other party is in this process: a death notice, the publisher's notice that it took the
//! subscription out (a binder call it makes, run here), an [`Interrupter`](crate::Interrupter), a
//! dropped `recv_async` future, a dropped subscriber, a name another publisher took from the
//! connection of a [`NamedSubscriber`](crate::NamedSubscriber). None of them can change `seq`,
//! so a wake that lands before the waiter enters the kernel would be lost. Each raises a bit in
//! `stop` instead, and while that bit is up and the waiter is `parked`, wakes again. The waiter
//! raises `parked` and then loads `stop`, so either it sees the bit and does not sleep, or the
//! waker sees `parked` and keeps waking until the waiter is out.
//!
//! A subscription woken through an fd (plan 15 §5.5a) keeps the protocol and changes what sleeps
//! and what wakes: the waiter loads `seq` once more after the fence and then polls its socket, and
//! a wake from this process is a byte sent to that socket while the bit is up (an interrupt's
//! kick, which a poll takes) and the waiter is `parked`. A send that queues no byte for want of
//! memory (`ENOBUFS`) is made again, until one is queued; one that finds the socket full
//! (`EAGAIN`) or shut down (`EPIPE`) ends the wake with no byte sent. The byte only ends a sleep
//! and stands for no event: `stop` alone says what happened, so a byte the wait drains, or one
//! sent after it, loses nothing, and with no wait `parked` none is sent. A poll ends at a bit it
//! takes or at one that stays up for good, as every wait does: an interrupt raises [`KICK`] with
//! [`INTERRUPT`], and a poll takes the kick and leaves the interrupt to the next receive.
//!
//! One wait at a time per subscriber: the code that waits joins a wait left on the blocking
//! pool before it arms (`Leftover`), so `WAITING` and `parked` have one owner. A dropped future
//! takes back a wait no pool thread has started, so the join waits only for one that runs, which
//! the drop's `ABANDON` ends. A leaked future's wait may still be queued: a look arms nothing, so
//! a wait that finds one out looks once before the join, and a bounded wait whose deadline passes
//! before the join ends looks once more and does not wait.

#[cfg(loom)]
use loom::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};
#[cfg(loom)]
use loom::sync::Mutex;
#[cfg(loom)]
use loom::thread::yield_now;
#[cfg(not(loom))]
use std::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};
#[cfg(all(feature = "tokio", not(loom)))]
use std::sync::Mutex;
use std::sync::OnceLock;
#[cfg(any(feature = "tokio", loom))]
use std::sync::{Arc, Weak};
#[cfg(not(loom))]
use std::thread::yield_now;
use std::time::{Duration, Instant};

use rsbinder::fmq::shm::Deadline;

/// `stop`: an [`Interrupter`](crate::Interrupter) asked, raised with [`KICK`]. A receive's wait
/// ends at it and takes both ([`Local::cancelled`]); a wait on the pool ends at it and leaves it to
/// the receive after; a poll does not end at it, but at the kick.
pub(crate) const INTERRUPT: u32 = 1;
/// `stop`: the wait on the pool has nobody to report to; that wait, or the next join, clears it.
pub(crate) const ABANDON: u32 = 2;
/// `stop`: the publisher's process died. Never cleared, and no wait sleeps while it is up.
pub(crate) const DIED: u32 = 4;
/// `stop`: the publisher took the subscription out (plan 15 §6.5). Never cleared, as `DIED`.
pub(crate) const EVICTED: u32 = 8;
/// `stop`: the name layer of a `NamedSubscriber` changed its state, for its connection's wait to
/// look at it: another publisher took the name, or the caller interrupted. A receive's wait and a
/// wait on the pool end at it and leave it up, and the name layer clears it, so that no interrupt
/// of the caller's is taken for it; a poll does not end at it, as the name layer's own fd does.
pub(crate) const NOTICE: u32 = 16;
/// `stop`: an interrupt, for a poll, raised with [`INTERRUPT`] and never up without it. A poll
/// ends at it and takes it ([`Local::prepare_poll`]); a receive takes it with the interrupt; a
/// wait on the pool ends at it and leaves it.
pub(crate) const KICK: u32 = 32;
/// What an [`Interrupter`](crate::Interrupter) raises.
pub(crate) const INTERRUPTED: u32 = INTERRUPT | KICK;
/// Every bit of `stop`: a receive's wait takes `INTERRUPT`, `KICK` and `ABANDON`, ends at the rest.
pub(crate) const ALL: u32 = INTERRUPT | ABANDON | DIED | EVICTED | NOTICE | KICK;
/// The bits a poll ends at and leaves up: for good, and the receive after it returns `Closed`.
pub(crate) const POLL_ENDS: u32 = DIED | EVICTED;
/// The bit a poll ends at and takes.
pub(crate) const POLL_TAKES: u32 = KICK;
/// The bits a reactor's wait ends at: all but `ABANDON`, which only the drop of a futex wake's
/// `recv_async` future and the drop of a subscriber raise.
#[cfg(feature = "tokio")]
pub(crate) const REACTOR_ENDS: u32 = ALL & !ABANDON;

/// Yields a waker looks before waking again: a wake is lost only before the waiter's syscall.
#[cfg(not(loom))]
const RECHECKS: u32 = 64;
/// Under loom one: every yield is a branch point of the model, and one is enough to reach both.
#[cfg(loom)]
const RECHECKS: u32 = 1;

/// A raise's first sleep between wakes, once a round of yields has not seen the wait out.
const FIRST_NAP: Duration = Duration::from_micros(50);
/// The longest: a wake lost at most this long ago is sent again.
const LAST_NAP: Duration = Duration::from_millis(1);

/// Yield once, then sleep `FIRST_NAP` doubling to `LAST_NAP`: a sleep frees the core, a yield may not.
fn pause(nap: &mut Duration) {
    if nap.is_zero() {
        yield_now();
        *nap = FIRST_NAP;
    } else {
        sleep(*nap);
        *nap = (*nap * 2).min(LAST_NAP);
    }
}

#[cfg(not(loom))]
fn sleep(nap: Duration) {
    std::thread::sleep(nap);
}

/// Under loom a sleep is one more branch point, as a yield is.
#[cfg(loom)]
fn sleep(_nap: Duration) {
    yield_now();
}

/// A wait's end in both forms it is read in: the `Instant` the loop and a poll compare against,
/// and the futex's `Deadline`, made once from it for the whole wait.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Until {
    pub(crate) at: Instant,
    /// `None` when the futex clock cannot express `at`, which then never arrives.
    pub(crate) futex: Option<Deadline>,
}

impl Until {
    pub(crate) fn new(at: Instant) -> Self {
        Self {
            at,
            futex: Deadline::after(at.saturating_duration_since(Instant::now())),
        }
    }
}

/// What a wait sleeps on: the topic's `seq` and the subscriber's `state`.
pub(crate) trait Words: Send + Sync {
    /// `seq`, ordered before the caller's next look at the ring.
    fn seq(&self) -> u32;
    /// Put `seen`, the `seq` the wait sleeps on, in `state` and raise `WAITING`: one `SeqCst` RMW.
    fn begin_wait(&self, seen: u32);
    /// Drain an fd wake, then lower `WAITING` alone; `true` when the publisher's socket hung up.
    fn end_wait(&self) -> bool;
    /// Sleep while `seq` holds `seen`; `false` when `deadline` passed first.
    fn sleep(&self, seen: u32, deadline: Option<&Until>) -> bool;
    /// Wake the sleeper; `false` when an fd wake queued no byte, which a later one may.
    fn wake(&self) -> bool;
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
    /// A test's pause after a wait or a poll's arm finds no bit up, before it sleeps or polls.
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
    pub(crate) fn park(&self, seen: u32, deadline: Option<&Until>) -> Slept {
        let stopped = self.arm(seen) != 0;
        let armed = Armed(self);
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
        drop(armed);
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

    /// Start a poll's wait on `seen`, which [`disarm`](Self::disarm) ends: whether the caller may
    /// poll. `next` is the ring's side of the decision ([`decide`]) for the bits of [`POLL_ENDS`],
    /// and `unread` whether bytes a drain left are on the fd; both are asked before the arm.
    ///
    /// Not when `next` says to look, and then nothing is armed; nor, armed, when a bit of
    /// `POLL_ENDS` is up, [`KICK`] is up, which this takes, `seq` moved from `seen`, or bytes were
    /// `unread`, and then it is disarmed again. The kick is taken only after `next`, so a call
    /// that returns for a record leaves it to the next one.
    pub(crate) fn prepare_poll(
        &self,
        seen: u32,
        next: impl FnOnce(u32) -> Next,
        unread: impl FnOnce() -> bool,
    ) -> bool {
        if next(POLL_ENDS) == Next::Look {
            return false;
        }
        let unread = unread();
        let stop = self.arm(seen);
        // After `parked`: a raise before it is seen here, and one after it sends a byte.
        let kicked = stop & POLL_TAKES != 0 && self.take(POLL_TAKES);
        if kicked || stop & POLL_ENDS != 0 || self.words().seq() != seen || unread {
            self.disarm();
            return false;
        }
        #[cfg(all(test, not(loom)))]
        if let Some(pause) = self.before_sleep.lock().expect("lock").as_ref() {
            pause();
        }
        true
    }

    /// Arm a reactor's wait on `seen`, which [`disarm`](Self::disarm) ends either way: whether the
    /// caller may wait for the fd. Not when a bit of [`REACTOR_ENDS`] is up, `seq` moved from
    /// `seen`, or bytes were `unread`, which the caller asks before the arm.
    #[cfg(feature = "tokio")]
    pub(crate) fn arm_reactor(&self, seen: u32, unread: bool) -> bool {
        self.arm(seen) & REACTOR_ENDS == 0 && self.words().seq() == seen && !unread
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
            // Only a publisher that shut the socket down makes it read end of file: taken as death.
            self.stop.fetch_or(DIED, Ordering::SeqCst);
        }
    }

    /// Raise `bit` and wake the wait until it is out, the bit it ends at is cleared, or, with an
    /// fd, a send ends the raise as below; the wakes it tried, those that failed included. An
    /// interrupt's is [`KICK`]: a receive takes it with [`INTERRUPT`], and a poll, which does not
    /// end at `INTERRUPT`, takes it alone.
    ///
    /// With an fd, a send that queues no byte for want of memory (`ENOBUFS`, `ENOMEM`) does not
    /// end the raise: a byte already on the fd makes no new edge for a poll registered
    /// edge-triggered, which may have taken that byte's edge already, and a byte sent makes one.
    /// So while the wait is parked and sends fail so, the raise sends again, a [`pause`] apart,
    /// until one is queued or the wait ends. A send that finds the send buffer full (`EAGAIN`) or
    /// the socket shut down (`EPIPE`) ends it with no byte sent, and so with no new edge.
    ///
    /// With a futex, the wake is sent again after `RECHECKS` yields and then a [`pause`] apart,
    /// for as long as the wait stays parked: only the waiter's thread running ends the raise.
    pub(crate) fn raise(&self, bit: u32) -> usize {
        self.stop.fetch_or(bit, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let Some(words) = self.words.get() else {
            return 0;
        };
        let watch = if bit & KICK != 0 {
            bit & !INTERRUPT
        } else {
            bit
        };
        let waiting =
            || self.stop.load(Ordering::SeqCst) & watch != 0 && self.parked.load(Ordering::SeqCst);
        let mut wakes = 0;
        let mut nap = Duration::ZERO;
        if words.latched() {
            // A byte stays until the wait, `parked` down, drains it: one that lands will do.
            while waiting() {
                wakes += 1;
                if words.wake() {
                    break;
                }
                pause(&mut nap);
            }
            return wakes;
        }
        while waiting() {
            words.wake();
            wakes += 1;
            if !nap.is_zero() {
                pause(&mut nap);
                continue;
            }
            // A woken thread takes a while to run; wake again only if this one may have been lost.
            for _ in 0..RECHECKS {
                yield_now();
                if !waiting() {
                    return wakes;
                }
            }
            nap = FIRST_NAP;
        }
        wakes
    }

    /// Clear `bit`; whether it was up.
    pub(crate) fn take(&self, bit: u32) -> bool {
        self.stop.fetch_and(!bit, Ordering::SeqCst) & bit != 0
    }

    /// Whether a receive's wait ends as cancelled: at an [`INTERRUPT`], which it takes with its
    /// [`KICK`], so that no poll ends at that interrupt after it, or at [`NOTICE`], which it
    /// leaves.
    pub(crate) fn cancelled(&self) -> bool {
        let interrupted = self.stop.fetch_and(!INTERRUPTED, Ordering::SeqCst) & INTERRUPT != 0;
        interrupted || self.stop_bits() & NOTICE != 0
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

    /// The bits up in `stop`, as [`decide`] takes them.
    pub(crate) fn stop_bits(&self) -> u32 {
        self.stop.load(Ordering::Relaxed)
    }

    /// Whether the publisher's process died. Loaded before the ring: the read finds its commits.
    pub(crate) fn died(&self) -> bool {
        self.stop.load(Ordering::Acquire) & DIED != 0
    }

    /// Whether the publisher took the subscription out.
    pub(crate) fn evicted(&self) -> bool {
        self.stop.load(Ordering::Acquire) & EVICTED != 0
    }
}

/// A wait [`Local::arm`] started; dropped, also as the sleep unwinds or the future that holds it
/// is dropped, it disarms that wait.
pub(crate) struct Armed<'a, W: Words>(pub(crate) &'a Local<W>);

impl<W: Words> Drop for Armed<'_, W> {
    fn drop(&mut self) {
        // Under loom an unwind is its deadlock report, and an atomic touched after it aborts.
        #[cfg(loom)]
        if std::thread::panicking() {
            return;
        }
        self.0.disarm();
    }
}

/// What a subscriber's latest look at the ring found, as [`decide`] needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Looked {
    /// A value, an error, or no record past the reader: `pending`, loaded after `seq`, decides.
    Exact,
    /// Lapped until it gave up; `Some(seq)` when that `seq` was loaded just before it began.
    GaveUp(Option<u32>),
}

/// What a wait that loaded `seq` does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Sleep on that `seq`: a record the latest look did not return moves it.
    Sleep,
    /// Look at the ring again first.
    Look,
}

/// Whether a wait that loaded `seen` may sleep on it: the ring's side of the decision, and the
/// bits of `stop` the wait `honors`, which are the ones it ends at or takes.
///
/// It may when none of those bits is up and either the reader caught up, `pending`
/// (`write != read`, loaded after `seen`) being false, or the latest look gave up having begun
/// right after a load of `seq` that returned `seen`. A look lapped until it gave up leaves
/// `pending` true, so only the `seq` it came after says what it could not read was announced
/// later. Every commit is announced by a bump of `seq` after its stores (`write`, `last`, `half`),
/// so in both cases a record the look did not return moves `seq` from `seen` and ends the sleep.
/// A record kept after a look that returned one would let a stale `seq` decide, so every look
/// sets `looked`.
pub(crate) fn decide(
    looked: Looked,
    seen: u32,
    stop: u32,
    honors: u32,
    pending: impl FnOnce() -> bool,
) -> Next {
    if stop & honors != 0 {
        return Next::Look;
    }
    match looked {
        Looked::GaveUp(Some(before)) if before == seen => Next::Sleep,
        Looked::GaveUp(_) => Next::Look,
        Looked::Exact if pending() => Next::Look,
        Looked::Exact => Next::Sleep,
    }
}

/// A wait handed to a pool thread: that thread or the drop of the future takes it out, once.
#[cfg(any(feature = "tokio", loom))]
struct Hand<H>(Mutex<Option<H>>);

#[cfg(any(feature = "tokio", loom))]
impl<H> Hand<H> {
    fn new(handed: H) -> Self {
        Self(Mutex::new(Some(handed)))
    }

    /// The wait, to whichever of the two asks first.
    fn claim(&self) -> Option<H> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// What a wait handed to the pool reports its end to.
#[cfg(any(feature = "tokio", loom))]
trait Settle: Send + Sync {
    fn settle(&self);
}

/// A wait for `seen` handed to a pool thread; it settles `leftover` when dropped, run or not.
#[cfg(any(feature = "tokio", loom))]
struct Handed<W: Words, L: Settle> {
    local: Arc<Local<W>>,
    leftover: Arc<L>,
    seen: u32,
}

#[cfg(any(feature = "tokio", loom))]
impl<W: Words, L: Settle> Handed<W, L> {
    fn run(self) {
        self.local.park(self.seen, None);
        // Its own, if the future was dropped meanwhile: ends the dropper's wakes at once.
        self.local.take(ABANDON);
    }
}

#[cfg(any(feature = "tokio", loom))]
impl<W: Words, L: Settle> Drop for Handed<W, L> {
    fn drop(&mut self) {
        self.leftover.settle();
    }
}

/// A pool thread's turn: the wait, unless the drop of its future took it back first.
#[cfg(any(feature = "tokio", loom))]
fn serve<W: Words, L: Settle>(hand: &Hand<Handed<W, L>>) {
    if let Some(handed) = hand.claim() {
        handed.run();
    }
}

/// Held by a future that handed a wait over; dropped armed, it settles or wakes that wait.
#[cfg(any(feature = "tokio", loom))]
struct GiveBack<'a, W: Words, H> {
    /// Not an `Arc`: a leaked future keeps nothing alive that the runtime drops unrun.
    hand: Weak<Hand<H>>,
    local: &'a Local<W>,
    armed: bool,
}

#[cfg(any(feature = "tokio", loom))]
impl<'a, W: Words, H> GiveBack<'a, W, H> {
    fn new(hand: &Arc<Hand<H>>, local: &'a Local<W>) -> Self {
        Self {
            hand: Arc::downgrade(hand),
            local,
            armed: true,
        }
    }
}

#[cfg(any(feature = "tokio", loom))]
impl<W: Words, H> Drop for GiveBack<'_, W, H> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Gone with its last `Arc`: run or dropped unrun, the wait settled as it went.
        let Some(hand) = self.hand.upgrade() else {
            return;
        };
        match hand.claim() {
            // No thread took it, so it never armed: no `WAITING`, no `parked`, nothing to wake.
            Some(unrun) => drop(unrun),
            None => {
                self.local.raise(ABANDON);
            }
        }
    }
}

pub(crate) use real::{Died, Parts};

/// The words in shared memory, and the death notice. Compiled under loom too, never run there.
mod real {
    use std::num::NonZeroU32;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Instant;

    use rsbinder::fmq::shm::Mapping;
    use rsbinder::fmq::Error;
    use rsbinder::{DeathRecipient, WIBinder};

    use super::{Local, Until, Words, DIED};
    use crate::layout::{seen_field, ControlPage, Seq, CLOSED, WAITING};
    use crate::link::Dead;
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

        fn sleep(&self, seen: u32, deadline: Option<&Until>) -> bool {
            if let Some(fd) = &self.fd {
                // What the futex's compare does in the kernel: after `WAITING`, `seq` once more.
                return self.seq() != seen || fd.sleep(deadline.map(|until| until.at));
            }
            if deadline.is_some_and(|until| until.at <= Instant::now()) {
                return false;
            }
            let futex = deadline.and_then(|until| until.futex.as_ref());
            // Any other error is a sleep that did not happen: the caller looks and comes back.
            !matches!(
                self.seq.word().wait(seen, NonZeroU32::MAX, futex),
                Err(Error::TimedOut)
            )
        }

        fn wake(&self) -> bool {
            match &self.fd {
                Some(fd) => fd.poke(),
                // Fails only where the system call does not exist, and then nobody sleeps on it.
                None => {
                    let _ = self.seq.word().wake(NonZeroU32::MAX);
                    true
                }
            }
        }

        fn latched(&self) -> bool {
            self.fd.is_some()
        }
    }

    /// The recipient on the publisher's binder: sets `dead`, then raises `DIED` and wakes the
    /// wait. `dead` is set by this notice alone; `DIED` also by a wake socket that hung up.
    pub(crate) struct Died {
        pub(crate) local: Arc<Local<Parts>>,
        pub(crate) dead: Arc<Dead>,
    }

    impl DeathRecipient for Died {
        fn binder_died(&self, who: &WIBinder) {
            self.dead.binder_died(who);
            self.local.raise(DIED);
        }
    }
}

#[cfg(feature = "tokio")]
pub(crate) use pool::{on_pool, Leftover};

/// A wait handed to the blocking pool, and the record of it the next wait joins.
#[cfg(feature = "tokio")]
mod pool {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    use rsbinder::StatusCode;

    use super::{serve, GiveBack, Hand, Handed, Local, Parts, Settle};

    /// Whether a wait handed to the pool has not ended: while its future lives, or while a pool
    /// thread runs it. At most one, which the next wait joins first.
    ///
    /// `busy` is raised by `on_pool` and lowered by `settle`, which the wait runs as it goes
    /// (`Handed`'s drop). Only the subscriber's own receive raises it, and only once no wait is
    /// out, so a `settle` never races a raise. `settle` lowers it while holding `idle_lock`, the
    /// mutex `join` holds from its look at `busy` until `Condvar::wait` releases it, so a `join`
    /// that saw it up is on the condvar before `settle` lowers it and gets the `notify_all`.
    #[derive(Default)]
    pub(crate) struct Leftover {
        busy: AtomicBool,
        idle_lock: Mutex<()>,
        idle: Condvar,
        notify: tokio::sync::Notify,
        /// A test's step as a receive begins to join, after the look before it.
        #[cfg(test)]
        pub(crate) before_join: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
        /// A test's step in a join that saw a wait out, before it sleeps on `idle`.
        #[cfg(test)]
        in_join: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    }

    impl Leftover {
        fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
            self.idle_lock.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Whether a wait is out. A `false` after a wait went to the pool is `settle`'s store,
        /// which a wait a pool thread ran makes after it lowered `parked` and `WAITING` and took
        /// its `ABANDON`: the `Acquire` pairs with that `Release`, so a wait that begins after it
        /// is their one owner. A wait taken back before it ran armed nothing.
        pub(crate) fn busy(&self) -> bool {
            self.busy.load(Ordering::Acquire)
        }

        /// No lock: no wait is out, so no `settle` and no `join` runs meanwhile.
        fn begin(&self) {
            self.busy.store(true, Ordering::Relaxed);
        }

        /// Block until no wait is out; `false` if `deadline` passes first.
        pub(crate) fn join(&self, deadline: Option<Instant>) -> bool {
            #[cfg(test)]
            if let Some(step) = self.before_join.lock().expect("lock").as_ref() {
                step();
            }
            let mut idle = self.lock();
            while self.busy() {
                #[cfg(test)]
                if let Some(step) = self.in_join.lock().expect("lock").as_ref() {
                    step();
                }
                let Some(deadline) = deadline else {
                    idle = self.idle.wait(idle).unwrap_or_else(|e| e.into_inner());
                    continue;
                };
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return false;
                }
                idle = self
                    .idle
                    .wait_timeout(idle, left)
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

    impl Settle for Leftover {
        fn settle(&self) {
            {
                let _idle = self.lock();
                self.busy.store(false, Ordering::Release);
            }
            self.idle.notify_all();
            self.notify.notify_waiters();
        }
    }

    /// One wait for `seen` on the pool, handed over at the first poll; `leftover` records it
    /// until it ends. Dropped before a pool thread takes the wait, the future takes it back and
    /// settles it, also when the hand-off unwinds, as `spawn_blocking` does outside a Tokio
    /// runtime, where its `Handle::current` panics.
    pub(crate) async fn on_pool(
        local: &Arc<Local<Parts>>,
        leftover: &Arc<Leftover>,
        seen: u32,
    ) -> Result<(), StatusCode> {
        leftover.begin();
        let hand = Arc::new(Hand::new(Handed {
            local: local.clone(),
            leftover: leftover.clone(),
            seen,
        }));
        let mut give_back = GiveBack::new(&hand, local);
        let ran = rsbinder::__spawn_transact::<rsbinder::Tokio, _, _, _, _, _, _>(
            move || serve(&hand),
            |()| async { Ok(()) },
        )
        .await;
        give_back.armed = false;
        ran
    }

    #[cfg(all(test, not(loom)))]
    mod tests {
        use std::sync::{mpsc, Arc, Mutex};
        use std::thread;
        use std::time::{Duration, Instant};

        use super::{Leftover, Settle};

        /// A wait that ends after a join saw it out, before the join sleeps, wakes that join.
        #[test]
        fn a_wait_that_ends_as_a_join_goes_to_sleep_wakes_the_join() {
            let left = Arc::new(Leftover::default());
            left.begin();
            let (at_join, joining) = mpsc::channel();
            let (ended, settled) = mpsc::channel::<()>();
            let hold = Mutex::new(Some((at_join, settled)));
            *left.in_join.lock().unwrap() = Some(Box::new(move || {
                if let Some((at_join, settled)) = hold.lock().unwrap().take() {
                    at_join.send(()).unwrap();
                    // A `settle` that takes the join's lock cannot end before the join sleeps.
                    let _ = settled.recv_timeout(Duration::from_millis(500));
                }
            }));
            let receiving = {
                let left = left.clone();
                thread::spawn(move || {
                    let started = Instant::now();
                    let joined = left.join(Some(started + Duration::from_secs(5)));
                    (joined, started.elapsed())
                })
            };
            joining.recv().unwrap();
            left.settle();
            let _ = ended.send(());
            let (joined, took) = receiving.join().unwrap();
            assert!(joined);
            assert!(took < Duration::from_secs(3), "missed the wake: {took:?}");
        }
    }
}

#[cfg(all(test, not(loom)))]
mod raise_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::{Local, Until, Words, DIED};

    /// Words whose wake reaches no one: only the waiter's own `disarm` ends a raise.
    struct Unheard(AtomicUsize);

    impl Words for Unheard {
        fn seq(&self) -> u32 {
            0
        }
        fn begin_wait(&self, _seen: u32) {}
        fn end_wait(&self) -> bool {
            false
        }
        fn sleep(&self, _seen: u32, _deadline: Option<&Until>) -> bool {
            true
        }
        fn wake(&self) -> bool {
            self.0.fetch_add(1, Ordering::Relaxed);
            true
        }
        fn latched(&self) -> bool {
            false
        }
    }

    /// A waiter that stays asleep 50 ms gets wakes a sleep apart, not one per round of yields.
    #[test]
    fn a_raise_at_a_waiter_that_does_not_run_sleeps_between_wakes() {
        let local = Arc::new(Local::new());
        local.set_words(Unheard(AtomicUsize::new(0)));
        assert_eq!(local.arm(0), 0);
        let waiter = {
            let local = local.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                local.disarm();
            })
        };
        let wakes = local.raise(DIED);
        waiter.join().unwrap();
        assert_eq!(wakes, local.words().0.load(Ordering::Relaxed));
        // 50 µs doubling to 1 ms is about 55 wakes in 50 ms; rounds of yields send hundreds.
        assert!((1..=80).contains(&wakes), "{wakes} wakes");
    }
}

#[cfg(all(test, loom))]
mod loom_tests;
