// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Loom model of `EventFlag::wake_lazy` against a waiter.
//!
//! Gated on `cfg(loom)`; empty in normal builds. Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p rsbinder-fmq --test loom_event_flag --release
//! ```
//!
//! This re-implements the protocol with `loom::sync` types; it does not run
//! the crate's code (the word lives in an mmap and the futex is a syscall).
//! Each step names the function in `src/event_flag.rs` it stands for. The
//! futex is a mutex and a condvar: the kernel checks the word and queues the
//! waiter under the hash-bucket lock that `FUTEX_WAKE` also takes, which is
//! the ordering a mutex gives.
//!
//! The question: a waker that stores the counter and then finds the bit
//! still standing skips its wake. Can the waiter then sleep on a counter it
//! read before that store? The bit starts set (a wake the waiter has not
//! consumed), the only state in which the waker skips. A lost wake shows as
//! a deadlock, which loom reports by panicking.

#![cfg(loom)]

use loom::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

const BIT: u32 = 0x2;

struct Model {
    word: AtomicU32,
    /// The write counter a consumer checks (`MessageQueue::available_to_read`).
    counter: AtomicU64,
    futex: Mutex<()>,
    sleepers: Condvar,
    waker_fence: bool,
    waiter_fence: bool,
}

impl Model {
    fn new(waker_fence: bool, waiter_fence: bool) -> Self {
        Model {
            word: AtomicU32::new(BIT),
            counter: AtomicU64::new(0),
            futex: Mutex::new(()),
            sleepers: Condvar::new(),
            waker_fence,
            waiter_fence,
        }
    }

    /// `sys::futex_wait`: sleep only while the word holds `expected`.
    fn futex_wait(&self, expected: u32) {
        let guard = self.futex.lock().unwrap();
        if self.word.load(Ordering::SeqCst) != expected {
            return;
        }
        drop(self.sleepers.wait(guard).unwrap());
    }

    /// `sys::futex_wake`.
    fn futex_wake(&self) {
        let _guard = self.futex.lock().unwrap();
        self.sleepers.notify_all();
    }

    /// `EventFlag::wake`.
    fn wake(&self, bits: u32) {
        let old = self.word.fetch_or(bits, Ordering::SeqCst);
        if !old & bits != 0 {
            self.futex_wake();
        }
    }

    /// `EventFlag::wake_lazy`.
    fn wake_lazy(&self, bits: u32) {
        if self.waker_fence {
            fence(Ordering::SeqCst);
        }
        if self.word.load(Ordering::Relaxed) & bits == bits {
            return;
        }
        self.wake(bits);
    }

    /// `EventFlag::wait_until`, no deadline.
    fn wait(&self, bits: u32) {
        loop {
            let old = self.word.fetch_and(!bits, Ordering::SeqCst);
            if old & bits != 0 {
                if self.waiter_fence {
                    fence(Ordering::SeqCst);
                }
                return;
            }
            self.futex_wait(old & !bits);
        }
    }
}

/// A producer commits one record and wakes lazily; the consumer looks and waits until it sees it.
fn one_record(waker_fence: bool, waiter_fence: bool) {
    loom::model(move || {
        let model = Arc::new(Model::new(waker_fence, waiter_fence));
        let producer = {
            let model = model.clone();
            thread::spawn(move || {
                // `MessageQueue::commit_write_cached`, then the wake after the commit.
                model.counter.store(1, Ordering::Release);
                model.wake_lazy(BIT);
            })
        };
        // `Consumer::next`: look, wait while there is nothing.
        while model.counter.load(Ordering::Acquire) == 0 {
            model.wait(BIT);
        }
        producer.join().unwrap();
    });
}

#[test]
fn no_wake_is_lost_with_both_fences() {
    one_record(true, true);
}

#[test]
#[should_panic(expected = "deadlock")]
fn a_wake_is_lost_without_the_wakers_fence() {
    one_record(false, true);
}

#[test]
#[should_panic(expected = "deadlock")]
fn a_wake_is_lost_without_the_waiters_fence() {
    one_record(true, false);
}
