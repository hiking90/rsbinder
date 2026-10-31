// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The wait protocol of `wait.rs` under loom (plan 15 AC-15.2.12, AC-15.2.41): [`Local::park`] and
//! [`Local::raise`] as they are, over a model of the two shared words and of the futex. The futex
//! compares `seq` under a lock its wake takes too, as the kernel's hash bucket lock does, so a
//! wake is lost only when it comes before the sleeper takes the lock. It keeps a count of wakes
//! per bit, and a sleep ends only at a wake whose mask meets the sleeper's bit, as the kernel's
//! `FUTEX_WAIT_BITSET` does (plan 17 T-11): the bit the sleep and the raise pass is the one
//! `Local` holds. A test that does not finish is a deadlock, which loom reports, and a raise that
//! wakes until a wait no wake reaches is out exceeds loom's branch bound, which loom reports as
//! well. The flawed models show the checks are not vacuous: a publisher without its fence, a
//! waker that wakes once, a raise that wakes a bit its wait does not sleep on, a new wait that
//! does not join, an fd wait that polls without loading `seq` again, a publisher that takes as
//! woken a wait that saw its commit, a lapped wait that sleeps once `write` stays, one that loads
//! `seq` after its look in place of before it, a pool thread that runs a wait its future took
//! back, a dropped future that settles a wait a thread runs, one that keeps a wait it took back
//! out, one that does not wake the wait a thread runs, a poll that takes an interrupt. A wait
//! decides with [`decide`]
//! itself, on the record of each look as `Subscriber::settle` keeps it, and a receive ends as
//! cancelled by [`Local::cancelled`] itself. A wait handed to the pool is `Handed` in its `Hand`,
//! run by `serve` and taken back or woken by the drop of `GiveBack`, as `on_pool` does. The
//! publisher wakes each wait once, as `Link::wake` does. The fd model (plan 15 §5.5a) counts the
//! bytes on the wake socket in place of the futex: the publisher's, and those this process sends
//! on its duplicate of the publisher's end. A poll starts with [`Local::prepare_poll`], as
//! `Subscriber::prepare_wait` does, and waits for a byte or, as at its timeout, for none; a
//! reactor's wait starts with [`Local::arm_reactor`], as `Subscriber::recv_on_reactor` does.
//! Run: see `Cargo.toml` (`--cfg loom`).

use std::num::NonZeroU32;
// The std `Arc`: one of loom's dropped while a deadlock unwinds calls into loom and aborts.
use std::sync::Arc;

use loom::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};
use loom::sync::{Condvar, Mutex};
use loom::thread;

use super::*;
use crate::layout::{seen_field, CLOSED, WAITING};

/// What a model leaves out of the protocol.
#[derive(Clone, Copy, PartialEq)]
enum Flaw {
    None,
    /// The publisher loads `state` after bumping `seq` with no `SeqCst` fence between.
    NoFence,
    /// A waker in this process wakes once instead of until the wait is out.
    WakeOnce,
    /// A raise in this process wakes the bit after its wait's, as one that named another
    /// subscriber's bit would; the publisher's wake of every bit is as it is.
    OtherBit,
    /// A new wait starts without joining the one a dropped future left on the pool.
    NoJoin,
    /// An fd wait polls without loading `seq` once more after raising `WAITING`.
    NoSecondLook,
    /// The publisher wakes, and takes as woken, a wait that loaded its commit's `seq`.
    WakesWhatSawIt,
    /// A wait takes `write` unmoved since its last turn, not `seq`, as a look that finds nothing.
    IdleOnWrite,
    /// A look that gave up records the `seq` loaded after it, not the one loaded before.
    SeqAfterLook,
    /// The pool thread runs the wait also when the future's drop took it back.
    RunsTakenBack,
    /// The future's drop settles the wait a pool thread runs, as well as waking it.
    SettlesRunning,
    /// The future's drop takes the wait back and does not settle it.
    KeepsBusy,
    /// The future's drop does not raise `ABANDON` for the wait a pool thread runs.
    NoRaise,
    /// A poll's wait takes an interrupt, as a receive's wait does, instead of leaving it up.
    PollTakesInterrupt,
}

/// `seq`, `state`, `write` for the ring, and the futex on `seq` or, with `fd`, the two fds.
struct Model {
    seq: AtomicU32,
    state: AtomicU32,
    write: AtomicU32,
    /// With `lapped`, where the reader resumes: stored after `write`, as `Writer::put_raw` does.
    half: AtomicU32,
    lapped: bool,
    /// `Subscriber::looked`, kept across receives as the field is.
    looked: std::sync::Mutex<Looked>,
    /// `Link::woken`: the `state` of the wait the publisher last woke.
    last_woken: AtomicU32,
    /// Sends from this process that queue no byte, as an `ENOBUFS` does, before they queue one.
    failing_sends: AtomicU32,
    /// The publisher's sends that queue no byte (`Sent::Failed`), before they queue one.
    failing_publishes: AtomicU32,
    futex: Mutex<Fds>,
    woken: Condvar,
    fd: bool,
    flaw: Flaw,
}

#[derive(Default)]
struct Fds {
    bytes: u32,
    count: u32,
    /// The futex wakes that named each bit: a sleep ends when the count of its bit moves.
    wakes: [u32; 32],
}

impl Fds {
    fn wakes_of(&self, bit: NonZeroU32) -> u32 {
        self.wakes[bit.trailing_zeros() as usize]
    }
}

impl Words for Arc<Model> {
    fn seq(&self) -> u32 {
        self.seq.load(Ordering::Acquire)
    }

    fn begin_wait(&self, seen: u32) {
        let mut old = self.state.load(Ordering::Relaxed);
        while let Err(now) = self.state.compare_exchange_weak(
            old,
            old & CLOSED | seen_field(seen) | WAITING,
            Ordering::SeqCst,
            Ordering::Relaxed,
        ) {
            old = now;
        }
    }

    fn end_wait(&self) -> bool {
        let mut fds = self.futex.lock().unwrap();
        (fds.bytes, fds.count) = (0, 0);
        drop(fds);
        self.state.fetch_and(!WAITING, Ordering::SeqCst);
        false
    }

    fn sleep(&self, seen: u32, _: Option<&Until>, bit: NonZeroU32) -> bool {
        if self.fd {
            if self.flaw != Flaw::NoSecondLook && self.seq() != seen {
                return true;
            }
            let mut fds = self.futex.lock().unwrap();
            while fds.bytes == 0 && fds.count == 0 {
                fds = self.woken.wait(fds).unwrap();
            }
            return true;
        }
        let mut futex = self.futex.lock().unwrap();
        if self.seq.load(Ordering::SeqCst) == seen {
            let before = futex.wakes_of(bit);
            while futex.wakes_of(bit) == before {
                futex = self.woken.wait(futex).unwrap();
            }
        }
        true
    }

    fn wake(&self, mask: NonZeroU32) -> bool {
        let mut fds = self.futex.lock().unwrap();
        if self.fd {
            if self.failing_sends.load(Ordering::Relaxed) > 0 {
                self.failing_sends.fetch_sub(1, Ordering::Relaxed);
                return false;
            }
            fds.count += 1;
        } else {
            let mask = match self.flaw {
                Flaw::OtherBit if mask != NonZeroU32::MAX => {
                    NonZeroU32::new(mask.get().rotate_left(1)).unwrap()
                }
                _ => mask,
            };
            for (at, wakes) in fds.wakes.iter_mut().enumerate() {
                if mask.get() & 1 << at != 0 {
                    *wakes = wakes.wrapping_add(1);
                }
            }
        }
        self.woken.notify_all();
        true
    }

    fn latched(&self) -> bool {
        self.fd
    }
}

fn model(flaw: Flaw) -> (Arc<Model>, Arc<Local<Arc<Model>>>) {
    built(flaw, false, false)
}

fn fd_model(flaw: Flaw) -> (Arc<Model>, Arc<Local<Arc<Model>>>) {
    built(flaw, true, false)
}

/// A reader the publisher lapped: it finds the record it resumes at once `half` is stored.
fn lapped_model(flaw: Flaw) -> (Arc<Model>, Arc<Local<Arc<Model>>>) {
    built(flaw, false, true)
}

fn built(flaw: Flaw, fd: bool, lapped: bool) -> (Arc<Model>, Arc<Local<Arc<Model>>>) {
    let model = Arc::new(Model {
        seq: AtomicU32::new(0),
        state: AtomicU32::new(0),
        write: AtomicU32::new(0),
        half: AtomicU32::new(0),
        lapped,
        looked: std::sync::Mutex::new(Looked::Exact),
        last_woken: AtomicU32::new(0),
        failing_sends: AtomicU32::new(0),
        failing_publishes: AtomicU32::new(0),
        futex: Mutex::new(Fds::default()),
        woken: Condvar::new(),
        fd,
        flaw,
    });
    let local = Arc::new(Local::new());
    local.set_words(model.clone());
    (model, local)
}

/// `State::announce` after a commit, against one subscriber, with `Link::wake`'s check.
fn publish(model: &Arc<Model>) {
    model.write.fetch_add(1, Ordering::Release);
    if model.lapped {
        model.half.fetch_add(1, Ordering::Release);
    }
    let seq = model.seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    if model.flaw != Flaw::NoFence {
        fence(Ordering::SeqCst);
    }
    let state = model.state.load(Ordering::Relaxed);
    if state & WAITING == 0 {
        model.last_woken.store(0, Ordering::Relaxed);
        return;
    }
    if state == model.last_woken.load(Ordering::Relaxed) {
        return;
    }
    let saw_it = state & !(WAITING | CLOSED) == seen_field(seq);
    if saw_it && model.flaw != Flaw::WakesWhatSawIt {
        return;
    }
    // `Sent::Failed`: not taken as woken, so the next commit sends again.
    if model.fd && model.failing_publishes.load(Ordering::Relaxed) > 0 {
        model.failing_publishes.fetch_sub(1, Ordering::Relaxed);
        return;
    }
    model.last_woken.store(state, Ordering::Relaxed);
    if model.fd {
        // `send_wake`: a byte on the socket, which stays until the wait drains it.
        model.futex.lock().unwrap().bytes += 1;
        model.woken.notify_all();
    } else {
        // `wake_seq`: every bit.
        model.wake(NonZeroU32::MAX);
    }
}

/// `Local::raise`, or with `WakeOnce` the waker that wakes once and returns.
fn raise(model: &Arc<Model>, local: &Local<Arc<Model>>, bit: u32) {
    if model.flaw != Flaw::WakeOnce {
        local.raise(bit);
        return;
    }
    local.stop.fetch_or(bit, Ordering::SeqCst);
    fence(Ordering::SeqCst);
    if local.parked.load(Ordering::SeqCst) {
        model.wake(local.bit);
    }
}

#[derive(Debug, PartialEq)]
enum Received {
    Record,
    Cancelled,
    Died,
    Evicted,
}

/// `Subscriber::wait` past the join, with the ring as `write` and a reader at `read`.
fn receive(model: &Arc<Model>, local: &Local<Arc<Model>>, read: &mut u32) -> Received {
    // What `decide` compares: `seq`, or with `IdleOnWrite` the `write` that flaw goes by.
    let mark = || match model.flaw {
        Flaw::IdleOnWrite => model.write.load(Ordering::Acquire),
        _ => local.words().seq(),
    };
    loop {
        // `Subscriber::look`: taken out, it ends before the ring.
        if local.evicted() {
            *model.looked.lock().unwrap() = Looked::Exact;
            return Received::Evicted;
        }
        // `Subscriber::before_look`.
        let after_one_gave_up = matches!(*model.looked.lock().unwrap(), Looked::GaveUp(_));
        let mut before = (after_one_gave_up && model.flaw != Flaw::SeqAfterLook).then(mark);
        let died = local.died();
        let write = model.write.load(Ordering::Acquire);
        if write != *read && (!model.lapped || model.half.load(Ordering::Acquire) == write) {
            *read += 1;
            *model.looked.lock().unwrap() = Looked::Exact;
            return Received::Record;
        }
        if after_one_gave_up && model.flaw == Flaw::SeqAfterLook {
            before = Some(mark());
        }
        // `Subscriber::settle`: a record past `read` it could not reach is a look that gave up.
        let looked = match write != *read {
            true => Looked::GaveUp(before),
            false => Looked::Exact,
        };
        *model.looked.lock().unwrap() = looked;
        if died {
            return Received::Died;
        }
        if local.cancelled() {
            return Received::Cancelled;
        }
        local.take(ABANDON);
        let seen = local.words().seq();
        let pending = || model.write.load(Ordering::Acquire) != *read;
        let decided = match model.flaw {
            Flaw::IdleOnWrite => decide(looked, mark(), local.stop_bits(), ALL, pending),
            _ => decide(looked, seen, local.stop_bits(), ALL, pending),
        };
        if decided == Next::Look {
            continue;
        }
        local.park(seen, None);
    }
}

fn check(test: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.check(test);
}

fn a_commit_ends_a_wait(flaw: Flaw) {
    check(move || {
        let (model, local) = model(flaw);
        let publisher = {
            let model = model.clone();
            thread::spawn(move || publish(&model))
        };
        assert_eq!(receive(&model, &local, &mut 0), Received::Record);
        publisher.join().unwrap();
    });
}

#[test]
fn loom_wait_a_commit_ends_a_wait() {
    a_commit_ends_a_wait(Flaw::None);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_a_publisher_without_its_fence_sleeps_through() {
    a_commit_ends_a_wait(Flaw::NoFence);
}

/// A reader the publisher lapped, a commit, and looks that find nothing until `half` is stored.
fn a_commit_ends_a_lapped_wait(flaw: Flaw) {
    check(move || {
        let (model, local) = lapped_model(flaw);
        let publisher = {
            let model = model.clone();
            thread::spawn(move || publish(&model))
        };
        assert_eq!(receive(&model, &local, &mut 0), Received::Record);
        publisher.join().unwrap();
    });
}

#[test]
fn loom_wait_a_lapped_wait_sleeps_only_on_a_seq_a_look_came_after() {
    a_commit_ends_a_lapped_wait(Flaw::None);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_a_lapped_wait_judged_by_write_sleeps_through() {
    a_commit_ends_a_lapped_wait(Flaw::IdleOnWrite);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_a_lapped_wait_on_the_seq_loaded_after_its_look_sleeps_through() {
    a_commit_ends_a_lapped_wait(Flaw::SeqAfterLook);
}

fn a_bit_from_this_process_ends_a_wait(flaw: Flaw, bit: u32, ends: Received) {
    check(move || {
        let (model, local) = model(flaw);
        let waker = {
            let (model, local) = (model.clone(), local.clone());
            thread::spawn(move || raise(&model, &local, bit))
        };
        assert_eq!(receive(&model, &local, &mut 0), ends);
        waker.join().unwrap();
    });
}

#[test]
fn loom_wait_an_interrupt_ends_a_wait() {
    a_bit_from_this_process_ends_a_wait(Flaw::None, INTERRUPTED, Received::Cancelled);
}

#[test]
fn loom_wait_a_death_ends_a_wait() {
    a_bit_from_this_process_ends_a_wait(Flaw::None, DIED, Received::Died);
}

/// Left up by the wait: `raise` ends once the wait is out of its sleep, not at a take of the bit.
#[test]
fn loom_wait_a_notice_ends_a_wait() {
    a_bit_from_this_process_ends_a_wait(Flaw::None, NOTICE, Received::Cancelled);
}

#[test]
fn loom_wait_an_eviction_ends_a_wait() {
    a_bit_from_this_process_ends_a_wait(Flaw::None, EVICTED, Received::Evicted);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_a_waker_that_wakes_once_loses_the_wait() {
    a_bit_from_this_process_ends_a_wait(Flaw::WakeOnce, INTERRUPTED, Received::Cancelled);
}

/// No wake reaches the wait, which sleeps on; the raise wakes for as long, past loom's bound.
#[test]
#[should_panic(expected = "maximum number of branches")]
fn loom_wait_a_raise_that_wakes_another_bit_loses_the_wait() {
    a_bit_from_this_process_ends_a_wait(Flaw::OtherBit, INTERRUPTED, Received::Cancelled);
}

/// `Leftover`: whether a wait is out on the pool, the condvar its end signals, and who settled.
struct Out {
    busy: AtomicBool,
    idle_lock: Mutex<()>,
    idle: Condvar,
    /// The thread that receives, and so drops the future; any other is the pool's.
    receiver: thread::ThreadId,
    /// Not loom's: counts read once every thread is joined, not a part of the protocol.
    by_drop: std::sync::atomic::AtomicU32,
    by_pool: std::sync::atomic::AtomicU32,
}

impl Settle for Out {
    fn settle(&self) {
        {
            let _idle = self.idle_lock.lock().unwrap();
            self.busy.store(false, Ordering::Release);
        }
        self.idle.notify_all();
        let by = match thread::current().id() == self.receiver {
            true => &self.by_drop,
            false => &self.by_pool,
        };
        by.fetch_add(1, Ordering::Relaxed);
    }
}

impl Out {
    /// Settles by the future's drop, and by the pool thread.
    fn settles(&self) -> (u32, u32) {
        let load = |count: &std::sync::atomic::AtomicU32| count.load(Ordering::Relaxed);
        (load(&self.by_drop), load(&self.by_pool))
    }
}

/// A wait out on the pool, recorded on the thread that will receive.
fn out() -> Arc<Out> {
    Arc::new(Out {
        busy: AtomicBool::new(true),
        idle_lock: Mutex::new(()),
        idle: Condvar::new(),
        receiver: thread::current().id(),
        by_drop: Default::default(),
        by_pool: Default::default(),
    })
}

/// `Leftover::join` with no deadline, behind the look at `busy` that `wait_with` takes first.
fn join(out: &Out) {
    if !out.busy.load(Ordering::Acquire) {
        return;
    }
    let mut idle = out.idle_lock.lock().unwrap();
    while out.busy.load(Ordering::Acquire) {
        idle = out.idle.wait(idle).unwrap();
    }
}

/// A wait handed to the pool, as `on_pool` makes it.
type Waiting = Handed<Arc<Model>, Out>;

/// The wait on `seq` now, in its `Hand` as `on_pool` hands it over.
fn handed(local: &Arc<Local<Arc<Model>>>, out: &Arc<Out>) -> Arc<Hand<Waiting>> {
    Arc::new(Hand::new(Handed {
        local: local.clone(),
        leftover: out.clone(),
        seen: local.words().seq(),
    }))
}

/// The pool thread's closure: `serve`, or with `RunsTakenBack` a wait whether taken back or not.
fn on_pool_thread(flaw: Flaw, hand: &Hand<Waiting>, local: Arc<Local<Arc<Model>>>, out: Arc<Out>) {
    if flaw != Flaw::RunsTakenBack {
        serve(hand);
        return;
    }
    // What a closure that holds the wait itself would sleep on: the `seq` at the hand-off, 0.
    let seen = 0;
    match hand.claim() {
        Some(handed) => handed.run(),
        None => Handed {
            local,
            leftover: out,
            seen,
        }
        .run(),
    }
}

/// The future's drop: `GiveBack` as it is, or the flaw's copy of it.
fn drop_future(flaw: Flaw, mut give_back: GiveBack<'_, Arc<Model>, Waiting>, out: &Out) {
    if !matches!(flaw, Flaw::SettlesRunning | Flaw::KeepsBusy | Flaw::NoRaise) {
        drop(give_back);
        return;
    }
    give_back.armed = false;
    let Some(hand) = give_back.hand.upgrade() else {
        return;
    };
    match hand.claim() {
        Some(unrun) if flaw == Flaw::KeepsBusy => std::mem::forget(unrun),
        Some(unrun) => drop(unrun),
        None if flaw == Flaw::NoRaise => {}
        None => {
            give_back.local.raise(ABANDON);
            if flaw == Flaw::SettlesRunning {
                out.settle();
            }
        }
    }
}

/// A dropped future and the pool thread race for its wait; the next receive gets a commit.
fn a_dropped_future_and_the_pool_race_for_its_wait(flaw: Flaw) {
    check(move || {
        let (model, local) = model(flaw);
        let out = out();
        let hand = handed(&local, &out);
        let give_back = GiveBack::new(&hand, &local);
        let pool = {
            let (local, out) = (local.clone(), out.clone());
            thread::spawn(move || on_pool_thread(flaw, &hand, local, out))
        };
        let publisher = {
            let model = model.clone();
            thread::spawn(move || publish(&model))
        };
        drop_future(flaw, give_back, &out);
        join(&out);
        local.take(ABANDON);
        assert_eq!(receive(&model, &local, &mut 0), Received::Record);
        publisher.join().unwrap();
        pool.join().unwrap();
        let (by_drop, by_pool) = out.settles();
        let both = by_drop > 0 && by_pool > 0;
        assert!(!both, "settled by the drop and the pool");
        assert_eq!(by_drop + by_pool, 1, "settles");
    });
}

#[test]
fn loom_wait_a_dropped_future_and_the_pool_settle_its_wait_once() {
    a_dropped_future_and_the_pool_race_for_its_wait(Flaw::None);
}

#[test]
#[should_panic(expected = "settled by the drop and the pool")]
fn loom_wait_a_pool_thread_that_runs_a_wait_taken_back_settles_it_twice() {
    a_dropped_future_and_the_pool_race_for_its_wait(Flaw::RunsTakenBack);
}

#[test]
#[should_panic(expected = "settled by the drop and the pool")]
fn loom_wait_a_drop_that_settles_a_wait_a_thread_runs_settles_it_twice() {
    a_dropped_future_and_the_pool_race_for_its_wait(Flaw::SettlesRunning);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_a_drop_that_keeps_a_wait_it_took_back_out_never_joins() {
    a_dropped_future_and_the_pool_race_for_its_wait(Flaw::KeepsBusy);
}

/// A dropped future and the pool thread race for its wait, with no commit: the drop ends it.
fn a_dropped_future_ends_the_wait_the_pool_runs(flaw: Flaw) {
    check(move || {
        let (_, local) = model(flaw);
        let out = out();
        let hand = handed(&local, &out);
        let give_back = GiveBack::new(&hand, &local);
        let pool = {
            let (local, out) = (local.clone(), out.clone());
            thread::spawn(move || on_pool_thread(flaw, &hand, local, out))
        };
        drop_future(flaw, give_back, &out);
        join(&out);
        pool.join().unwrap();
        let (by_drop, by_pool) = out.settles();
        assert_eq!(by_drop + by_pool, 1, "settles");
    });
}

#[test]
fn loom_wait_a_dropped_future_ends_the_wait_the_pool_runs() {
    a_dropped_future_ends_the_wait_the_pool_runs(Flaw::None);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_a_drop_that_does_not_raise_abandon_leaves_the_wait_asleep() {
    a_dropped_future_ends_the_wait_the_pool_runs(Flaw::NoRaise);
}

/// A pool thread held until the receive is done: `bit`, or else a commit, `ends` it unjoined.
fn a_receive_after_a_wait_the_pool_has_not_started(flaw: Flaw, bit: Option<u32>, ends: Received) {
    check(move || {
        let (model, local) = model(flaw);
        let out = out();
        let hand = handed(&local, &out);
        let give_back = GiveBack::new(&hand, &local);
        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let pool = {
            let (local, out, released) = (local.clone(), out.clone(), released.clone());
            thread::spawn(move || {
                let mut free = released.0.lock().unwrap();
                while !*free {
                    free = released.1.wait(free).unwrap();
                }
                drop(free);
                on_pool_thread(flaw, &hand, local, out);
            })
        };
        let other = {
            let (model, local) = (model.clone(), local.clone());
            thread::spawn(move || match bit {
                Some(bit) => {
                    local.raise(bit);
                }
                None => publish(&model),
            })
        };
        drop_future(flaw, give_back, &out);
        join(&out);
        local.take(ABANDON);
        assert_eq!(receive(&model, &local, &mut 0), ends);
        *released.0.lock().unwrap() = true;
        released.1.notify_all();
        other.join().unwrap();
        pool.join().unwrap();
        assert_eq!(out.settles(), (1, 0), "settled by the drop alone");
    });
}

#[test]
fn loom_wait_a_commit_ends_a_receive_after_a_wait_the_pool_has_not_started() {
    a_receive_after_a_wait_the_pool_has_not_started(Flaw::None, None, Received::Record);
}

#[test]
fn loom_wait_an_interrupt_ends_a_receive_after_a_wait_the_pool_has_not_started() {
    let ends = Received::Cancelled;
    a_receive_after_a_wait_the_pool_has_not_started(Flaw::None, Some(INTERRUPTED), ends);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_a_receive_after_a_wait_taken_back_and_kept_out_never_joins() {
    let ends = Received::Cancelled;
    a_receive_after_a_wait_the_pool_has_not_started(Flaw::KeepsBusy, Some(INTERRUPTED), ends);
}

/// A future leaked with its wait queued, then the runtime drops that wait unrun: it settles.
#[test]
fn loom_wait_a_wait_dropped_unrun_settles_though_its_future_leaked() {
    check(|| {
        let (_, local) = model(Flaw::None);
        let out = out();
        let hand = handed(&local, &out);
        std::mem::forget(GiveBack::new(&hand, &local));
        let pool = thread::spawn(move || drop(hand));
        join(&out);
        pool.join().unwrap();
        assert_eq!(out.settles(), (0, 1));
    });
}

/// A dropped future's wait on the pool, a new wait right after it, and a commit (AC-15.2.41).
fn a_new_wait_after_a_dropped_one(flaw: Flaw) {
    check(move || {
        let (model, local) = model(flaw);
        let out = out();
        // The wait the future left: `Handed::run`, then its drop.
        let left = {
            let (local, out) = (local.clone(), out.clone());
            let seen = local.words().seq();
            thread::spawn(move || {
                Handed {
                    local,
                    leftover: out,
                    seen,
                }
                .run()
            })
        };
        let publisher = {
            let model = model.clone();
            thread::spawn(move || publish(&model))
        };
        // The future's drop, then the next receive: join, clear what is stale, wait.
        local.raise(ABANDON);
        if flaw != Flaw::NoJoin {
            join(&out);
        }
        local.take(ABANDON);
        assert_eq!(receive(&model, &local, &mut 0), Received::Record);
        publisher.join().unwrap();
        left.join().unwrap();
    });
}

#[test]
fn loom_wait_a_new_wait_joins_the_one_a_dropped_future_left() {
    a_new_wait_after_a_dropped_one(Flaw::None);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_a_new_wait_that_does_not_join_sleeps_through() {
    a_new_wait_after_a_dropped_one(Flaw::NoJoin);
}

/// The pool's wait, then two receives across two commits: each wait gets its own wake.
fn two_commits_after_a_dropped_wait(flaw: Flaw) {
    check(move || {
        let (model, local) = model(flaw);
        let out = out();
        let left = {
            let (local, out) = (local.clone(), out.clone());
            let seen = local.words().seq();
            thread::spawn(move || {
                Handed {
                    local,
                    leftover: out,
                    seen,
                }
                .run()
            })
        };
        let publisher = {
            let model = model.clone();
            thread::spawn(move || {
                publish(&model);
                publish(&model);
            })
        };
        local.raise(ABANDON);
        join(&out);
        local.take(ABANDON);
        let mut read = 0;
        assert_eq!(receive(&model, &local, &mut read), Received::Record);
        assert_eq!(receive(&model, &local, &mut read), Received::Record);
        publisher.join().unwrap();
        left.join().unwrap();
    });
}

#[test]
fn loom_wait_each_wait_after_a_dropped_one_is_woken_once() {
    two_commits_after_a_dropped_wait(Flaw::None);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_a_wake_taken_as_one_by_a_wait_that_saw_its_commit_sleeps_through() {
    two_commits_after_a_dropped_wait(Flaw::WakesWhatSawIt);
}

fn a_commit_ends_an_fd_wait(flaw: Flaw) {
    check(move || {
        let (model, local) = fd_model(flaw);
        let publisher = {
            let model = model.clone();
            thread::spawn(move || publish(&model))
        };
        assert_eq!(receive(&model, &local, &mut 0), Received::Record);
        publisher.join().unwrap();
    });
}

#[test]
fn loom_wait_fd_a_commit_ends_a_wait() {
    a_commit_ends_an_fd_wait(Flaw::None);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_fd_a_wait_that_does_not_look_at_seq_again_sleeps_through() {
    a_commit_ends_an_fd_wait(Flaw::NoSecondLook);
}

fn a_bit_from_this_process_ends_an_fd_wait(bit: u32, ends: Received) {
    check(move || {
        let (model, local) = fd_model(Flaw::None);
        let waker = {
            let local = local.clone();
            thread::spawn(move || local.raise(bit))
        };
        assert_eq!(receive(&model, &local, &mut 0), ends);
        waker.join().unwrap();
    });
}

/// A byte while the wait is `parked`, none before it: then the wait sees the bit as it arms.
#[test]
fn loom_wait_fd_an_interrupt_ends_a_wait() {
    a_bit_from_this_process_ends_an_fd_wait(INTERRUPTED, Received::Cancelled);
}

#[test]
fn loom_wait_fd_a_death_ends_a_wait() {
    a_bit_from_this_process_ends_an_fd_wait(DIED, Received::Died);
}

#[test]
fn loom_wait_fd_a_notice_ends_a_wait() {
    a_bit_from_this_process_ends_an_fd_wait(NOTICE, Received::Cancelled);
}

#[test]
fn loom_wait_fd_an_eviction_ends_a_wait() {
    a_bit_from_this_process_ends_an_fd_wait(EVICTED, Received::Evicted);
}

/// A death whose first byte finds no memory to queue in: sent again while the wait is `parked`.
#[test]
fn loom_wait_fd_a_death_whose_byte_is_not_queued_sends_again() {
    check(move || {
        let (model, local) = fd_model(Flaw::None);
        model.failing_sends.store(1, Ordering::Relaxed);
        let waker = {
            let local = local.clone();
            thread::spawn(move || local.raise(DIED))
        };
        assert_eq!(receive(&model, &local, &mut 0), Received::Died);
        waker.join().unwrap();
    });
}

/// `Subscriber::prepare_wait`, a poll until a byte or, `timed_out`, for none, and `finish_wait`.
fn poll(model: &Arc<Model>, local: &Local<Arc<Model>>, read: u32, timed_out: bool) {
    if model.flaw == Flaw::PollTakesInterrupt {
        local.take(INTERRUPT);
    }
    let seen = local.words().seq();
    let looked = *model.looked.lock().unwrap();
    let pending = || model.write.load(Ordering::Acquire) != read;
    let next = |ends| decide(looked, seen, local.stop_bits(), ends, pending);
    if !local.prepare_poll(seen, next, unread) {
        return;
    }
    let mut fds = model.futex.lock().unwrap();
    while !timed_out && fds.bytes == 0 && fds.count == 0 {
        fds = model.woken.wait(fds).unwrap();
    }
    drop(fds);
    local.disarm();
}

/// `FdWake::unread`, whether the last drain stopped at its bound: never, a model drain empties all.
fn unread() -> bool {
    false
}

/// The bytes on the fd: the publisher's and this process's.
fn queued(model: &Model) -> u32 {
    let fds = model.futex.lock().unwrap();
    fds.bytes + fds.count
}

/// A turn of a poll loop whose `try_recv` finds nothing: whether its poll would sleep.
fn polls(model: &Arc<Model>, local: &Local<Arc<Model>>) -> bool {
    let seen = local.words().seq();
    let next = |ends| decide(Looked::Exact, seen, local.stop_bits(), ends, || false);
    if !local.prepare_poll(seen, next, unread) {
        return false;
    }
    // A byte on the fd ends the poll at once, and `finish_wait` drains it.
    let sleeps = queued(model) == 0;
    local.disarm();
    sleeps
}

/// `Subscriber::recv_on_reactor` until a record: `seq` loaded before the look, as `seen` is.
#[cfg(feature = "tokio")]
fn receive_on_reactor(model: &Arc<Model>, local: &Local<Arc<Model>>) {
    loop {
        let seen = local.words().seq();
        if model.write.load(Ordering::Acquire) != 0 {
            return;
        }
        let stop = local.stop_bits();
        if decide(Looked::Exact, seen, stop, REACTOR_ENDS, || false) == Next::Look {
            continue;
        }
        if local.arm_reactor(seen, unread()) {
            // `Reactor::readable`: a byte on the fd.
            let mut fds = model.futex.lock().unwrap();
            while fds.bytes == 0 && fds.count == 0 {
                fds = model.woken.wait(fds).unwrap();
            }
        }
        local.disarm();
    }
}

/// A commit as a reactor's wait arms: the arm sees `seq` moved, or the commit's byte ends it.
#[cfg(feature = "tokio")]
#[test]
fn loom_wait_fd_a_commit_ends_a_reactor_wait() {
    check(move || {
        let (model, local) = fd_model(Flaw::None);
        let publisher = {
            let model = model.clone();
            thread::spawn(move || publish(&model))
        };
        receive_on_reactor(&model, &local);
        publisher.join().unwrap();
    });
}

/// A bit raised during a poll ends it and the receive after; the first `failing` sends fail.
fn a_bit_from_this_process_ends_a_poll(flaw: Flaw, bit: u32, ends: Received, failing: u32) {
    check(move || {
        let (model, local) = fd_model(flaw);
        model.failing_sends.store(failing, Ordering::Relaxed);
        let waker = {
            let local = local.clone();
            thread::spawn(move || local.raise(bit))
        };
        poll(&model, &local, 0, false);
        waker.join().unwrap();
        assert_eq!(receive(&model, &local, &mut 0), ends);
    });
}

/// An interrupt in a poll loop ends a poll, then one turn of the loop after the raise is over.
fn an_interrupt_ends_one_turn_of_a_poll_loop(failing: u32, timed_out: bool) {
    check(move || {
        let (model, local) = fd_model(Flaw::None);
        model.failing_sends.store(failing, Ordering::Relaxed);
        let waker = {
            let local = local.clone();
            thread::spawn(move || local.raise(INTERRUPTED))
        };
        poll(&model, &local, 0, timed_out);
        waker.join().unwrap();
        // The kick a turn takes, whose `disarm` drains a byte that landed after the poll's drain.
        let mut early = 0;
        while !polls(&model, &local) {
            early += 1;
            assert!(early <= 1, "a poll loop spins at one interrupt");
        }
        assert_eq!(
            local.stop_bits(),
            INTERRUPT,
            "a kick left up, or the interrupt taken"
        );
        assert_eq!(receive(&model, &local, &mut 0), Received::Cancelled);
    });
}

/// Raised as a poll loop turns, each to its timeout: it ends two turns at most, by kick and byte.
#[test]
fn loom_wait_fd_an_interrupt_ends_at_most_two_turns_of_a_poll_loop_it_runs_across() {
    check(move || {
        let (model, local) = fd_model(Flaw::None);
        let waker = {
            let local = local.clone();
            thread::spawn(move || local.raise(INTERRUPTED))
        };
        // Its byte may land after the drain of any of these turns.
        let mut early = (0..TURNS).filter(|_| !polls(&model, &local)).count();
        waker.join().unwrap();
        while !polls(&model, &local) {
            early += 1;
            assert!(early <= 2, "a poll loop spins at one interrupt");
        }
        assert!(early <= 2, "{early} turns ended by one interrupt");
        assert_eq!(receive(&model, &local, &mut 0), Received::Cancelled);
    });
}

/// The turns of a poll loop that run across a raise.
const TURNS: usize = 3;

#[test]
fn loom_wait_fd_an_interrupt_ends_a_poll_and_the_receive_after_it() {
    an_interrupt_ends_one_turn_of_a_poll_loop(0, false);
}

/// A first send that queues nothing, while the poll is `parked`: sent again until one is.
#[test]
fn loom_wait_fd_an_interrupt_whose_byte_is_not_queued_ends_a_poll() {
    an_interrupt_ends_one_turn_of_a_poll_loop(1, false);
}

/// The poll returned at its timeout, about when the interrupt came: the next turn ends.
#[test]
fn loom_wait_fd_an_interrupt_as_a_poll_times_out_ends_the_next_turn() {
    an_interrupt_ends_one_turn_of_a_poll_loop(0, true);
}

/// Raised after the poll returned, or `concurrent` across its `finish_wait`: the next poll ends.
fn an_interrupt_after_the_poll_returned_ends_the_next_poll(concurrent: bool) {
    check(move || {
        let (model, local) = fd_model(Flaw::None);
        let seen = local.words().seq();
        let next = |ends| decide(Looked::Exact, seen, local.stop_bits(), ends, || false);
        assert!(local.prepare_poll(seen, next, || false));
        if !concurrent {
            local.raise(INTERRUPTED);
            local.disarm();
            poll(&model, &local, 0, false);
        } else {
            let waker = {
                let local = local.clone();
                thread::spawn(move || local.raise(INTERRUPTED))
            };
            local.disarm();
            poll(&model, &local, 0, false);
            waker.join().unwrap();
        }
        assert_eq!(receive(&model, &local, &mut 0), Received::Cancelled);
    });
}

#[test]
fn loom_wait_fd_an_interrupt_after_a_poll_returned_ends_the_next() {
    an_interrupt_after_the_poll_returned_ends_the_next_poll(false);
}

#[test]
fn loom_wait_fd_an_interrupt_from_another_thread_as_a_poll_returns_ends_the_next() {
    an_interrupt_after_the_poll_returned_ends_the_next_poll(true);
}

/// A receive cancelled by an interrupt takes its kick too: no poll after it ends for it.
#[test]
fn loom_wait_fd_a_receive_cancelled_leaves_no_kick_to_a_poll() {
    check(move || {
        let (model, local) = fd_model(Flaw::None);
        let waker = {
            let local = local.clone();
            thread::spawn(move || local.raise(INTERRUPTED))
        };
        assert_eq!(receive(&model, &local, &mut 0), Received::Cancelled);
        waker.join().unwrap();
        assert_eq!(local.stop_bits(), 0);
        // A byte sent while the receive slept may have landed after its drain: one turn ends.
        let mut early = 0;
        while !polls(&model, &local) {
            early += 1;
            assert!(early <= 1, "a poll ends at an interrupt a receive returned");
        }
    });
}

/// With no wait `parked`, no byte is sent: the wait that arms next sees the bit.
#[test]
fn loom_wait_fd_a_raise_with_no_wait_sends_nothing() {
    check(move || {
        let (model, local) = fd_model(Flaw::None);
        assert_eq!(local.raise(INTERRUPTED), 0);
        assert_eq!(queued(&model), 0);
        assert!(!polls(&model, &local), "the kick taken");
        assert!(polls(&model, &local));
    });
}

/// Every send failing, a raise ends once a poll took its kick, though the next poll is `parked`.
#[test]
fn loom_wait_fd_a_raise_ends_once_a_poll_took_its_kick() {
    check(move || {
        let (model, local) = fd_model(Flaw::None);
        model.failing_sends.store(u32::MAX, Ordering::Relaxed);
        let waker = {
            let local = local.clone();
            thread::spawn(move || local.raise(INTERRUPTED))
        };
        while local.stop_bits() & KICK == 0 {
            thread::yield_now();
        }
        let seen = local.words().seq();
        let next = |ends| decide(Looked::Exact, seen, local.stop_bits(), ends, || false);
        assert!(!local.prepare_poll(seen, next, unread), "the kick taken");
        let next = |ends| decide(Looked::Exact, seen, local.stop_bits(), ends, || false);
        assert!(local.prepare_poll(seen, next, unread));
        // Parked, with the interrupt up and no kick: the raise ends all the same.
        waker.join().unwrap();
        local.disarm();
        assert_eq!(local.stop_bits(), INTERRUPT);
    });
}

#[test]
fn loom_wait_fd_a_death_ends_a_poll() {
    a_bit_from_this_process_ends_a_poll(Flaw::None, DIED, Received::Died, 0);
}

#[test]
fn loom_wait_fd_a_death_whose_byte_is_not_queued_ends_a_poll() {
    a_bit_from_this_process_ends_a_poll(Flaw::None, DIED, Received::Died, 1);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_fd_a_poll_that_takes_an_interrupt_leaves_the_receive_after_it_asleep() {
    let flaw = Flaw::PollTakesInterrupt;
    a_bit_from_this_process_ends_a_poll(flaw, INTERRUPTED, Received::Cancelled, 0);
}

#[test]
fn loom_wait_fd_a_commit_ends_a_poll() {
    check(move || {
        let (model, local) = fd_model(Flaw::None);
        let publisher = {
            let model = model.clone();
            thread::spawn(move || publish(&model))
        };
        poll(&model, &local, 0, false);
        publisher.join().unwrap();
        assert_eq!(receive(&model, &local, &mut 0), Received::Record);
    });
}

/// A commit whose byte is not queued takes no wait as woken: the next commit's byte ends it.
#[test]
fn loom_wait_fd_a_commit_whose_byte_is_not_queued_leaves_the_wake_to_the_next() {
    check(move || {
        let (model, local) = fd_model(Flaw::None);
        model.failing_publishes.store(1, Ordering::Relaxed);
        let publisher = {
            let model = model.clone();
            thread::spawn(move || {
                publish(&model);
                publish(&model);
            })
        };
        assert_eq!(receive(&model, &local, &mut 0), Received::Record);
        publisher.join().unwrap();
    });
}
