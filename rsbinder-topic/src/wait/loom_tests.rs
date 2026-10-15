// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The wait protocol of `wait.rs` under loom (plan 15 AC-15.2.12, AC-15.2.41): [`Local::park`] and
//! [`Local::raise`] as they are, over a model of the two shared words and of the futex. The futex
//! compares `seq` under a lock its wake takes too, as the kernel's hash bucket lock does, so a
//! wake is lost only when it comes before the sleeper takes the lock. A test that does not
//! finish is a deadlock, which loom reports. The flawed models show the checks are not vacuous:
//! a publisher without its fence, a waker that wakes once, a new wait that does not join.
//! Run: see `Cargo.toml` (`--cfg loom`).

// The std `Arc`: one of loom's dropped while a deadlock unwinds calls into loom and aborts.
use std::sync::Arc;

use loom::sync::atomic::{fence, AtomicU32, Ordering};
use loom::sync::{Condvar, Mutex};
use loom::thread;

use super::*;
use crate::layout::{WAITING, WAIT_COUNT};

/// What a model leaves out of the protocol.
#[derive(Clone, Copy, PartialEq)]
enum Flaw {
    None,
    /// The publisher loads `state` after bumping `seq` with no `SeqCst` fence between.
    NoFence,
    /// A waker in this process wakes once instead of until the wait is out.
    WakeOnce,
    /// A new wait starts without joining the one a dropped future left on the pool.
    NoJoin,
}

/// `seq`, `state`, the futex on `seq`, and `write` for the ring.
struct Model {
    seq: AtomicU32,
    state: AtomicU32,
    write: AtomicU32,
    futex: Mutex<()>,
    woken: Condvar,
    flaw: Flaw,
}

impl Words for Arc<Model> {
    fn seq(&self) -> u32 {
        self.seq.load(Ordering::Acquire)
    }

    fn begin_wait(&self) {
        self.state.fetch_add(WAIT_COUNT, Ordering::SeqCst);
        self.state.fetch_or(WAITING, Ordering::SeqCst);
    }

    fn end_wait(&self) {
        self.state.fetch_and(!WAITING, Ordering::SeqCst);
    }

    fn sleep(&self, seen: u32, _: Option<&Deadline>) -> bool {
        let futex = self.futex.lock().unwrap();
        if self.seq.load(Ordering::SeqCst) == seen {
            drop(self.woken.wait(futex).unwrap());
        }
        true
    }

    fn wake(&self) {
        let _futex = self.futex.lock().unwrap();
        self.woken.notify_all();
    }
}

fn model(flaw: Flaw) -> (Arc<Model>, Arc<Local<Arc<Model>>>) {
    let model = Arc::new(Model {
        seq: AtomicU32::new(0),
        state: AtomicU32::new(0),
        write: AtomicU32::new(0),
        futex: Mutex::new(()),
        woken: Condvar::new(),
        flaw,
    });
    let local = Arc::new(Local::new());
    local.set_words(model.clone());
    (model, local)
}

/// `State::announce` after a commit, against one subscriber.
fn publish(model: &Arc<Model>) {
    model.write.fetch_add(1, Ordering::Release);
    model.seq.fetch_add(1, Ordering::SeqCst);
    if model.flaw != Flaw::NoFence {
        fence(Ordering::SeqCst);
    }
    if model.state.load(Ordering::Relaxed) & WAITING != 0 {
        model.wake();
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
        model.wake();
    }
}

#[derive(Debug, PartialEq)]
enum Received {
    Record,
    Cancelled,
    Died,
}

/// `Subscriber::wait` past the join, with the ring as `write` and a reader at `read`.
fn receive(model: &Arc<Model>, local: &Local<Arc<Model>>, read: &mut u32) -> Received {
    loop {
        let died = local.died();
        if model.write.load(Ordering::Acquire) != *read {
            *read += 1;
            return Received::Record;
        }
        if died {
            return Received::Died;
        }
        if local.take(INTERRUPT) {
            return Received::Cancelled;
        }
        local.take(ABANDON);
        let seen = local.words().seq();
        if model.write.load(Ordering::Acquire) != *read || local.stopped() {
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
    a_bit_from_this_process_ends_a_wait(Flaw::None, INTERRUPT, Received::Cancelled);
}

#[test]
fn loom_wait_a_death_ends_a_wait() {
    a_bit_from_this_process_ends_a_wait(Flaw::None, DIED, Received::Died);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_wait_a_waker_that_wakes_once_loses_the_wait() {
    a_bit_from_this_process_ends_a_wait(Flaw::WakeOnce, INTERRUPT, Received::Cancelled);
}

/// `Leftover`: whether a wait is out on the pool, and the condvar its end signals.
struct Out {
    busy: Mutex<bool>,
    idle: Condvar,
}

/// A dropped future's wait on the pool, a new wait right after it, and a commit (AC-15.2.41).
fn a_new_wait_after_a_dropped_one(flaw: Flaw) {
    check(move || {
        let (model, local) = model(flaw);
        let out = Arc::new(Out {
            busy: Mutex::new(true),
            idle: Condvar::new(),
        });
        // The wait the future left: `Handed::run`, then its drop.
        let left = {
            let (local, out) = (local.clone(), out.clone());
            let seen = local.words().seq();
            thread::spawn(move || {
                local.park(seen, None);
                local.take(ABANDON);
                *out.busy.lock().unwrap() = false;
                out.idle.notify_all();
            })
        };
        let publisher = {
            let model = model.clone();
            thread::spawn(move || publish(&model))
        };
        // The future's drop, then the next receive: join, clear what is stale, wait.
        local.raise(ABANDON);
        if flaw != Flaw::NoJoin {
            let mut busy = out.busy.lock().unwrap();
            while *busy {
                busy = out.idle.wait(busy).unwrap();
            }
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
