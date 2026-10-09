// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Loom runs of the real `wake_lazy`/`wait_until` against a modeled word and futex.
//! Run: see `Cargo.toml` (`--cfg loom`).

use loom::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

use super::{wait_until, wake_lazy, FlagWord};
use crate::error::Result;
use crate::sys::Timespec;

const BIT: u32 = 0x2;

/// The shared word and a futex: sleep while the word holds a value, wake all sleepers.
struct Word {
    word: AtomicU32,
    futex: Mutex<()>,
    sleepers: Condvar,
}

/// One side's view of the word; `fence: false` drops that side's fences (the mutants).
struct Side {
    word: Arc<Word>,
    fence: bool,
}

impl FlagWord for Side {
    fn fetch_or(&self, bits: u32, order: Ordering) -> u32 {
        self.word.word.fetch_or(bits, order)
    }
    fn fetch_and(&self, bits: u32, order: Ordering) -> u32 {
        self.word.word.fetch_and(bits, order)
    }
    fn load(&self, order: Ordering) -> u32 {
        self.word.word.load(order)
    }
    fn fence(&self, order: Ordering) {
        if self.fence {
            fence(order);
        }
    }
    fn futex_wait(&self, expected: u32, _bits: u32, _deadline: Option<&Timespec>) -> Result<()> {
        let guard = self.word.futex.lock().unwrap();
        if self.word.word.load(Ordering::SeqCst) == expected {
            drop(self.word.sleepers.wait(guard).unwrap());
        }
        Ok(())
    }
    fn futex_wake(&self, _bits: u32) -> Result<()> {
        let _guard = self.word.futex.lock().unwrap();
        self.word.sleepers.notify_all();
        Ok(())
    }
}

/// A producer commits one record and wakes lazily; the consumer looks and waits until it sees it.
/// The bit starts set, so the producer may skip the wake (the case the fences cover).
fn one_record(waker_fence: bool, waiter_fence: bool) {
    loom::model(move || {
        let word = Arc::new(Word {
            word: AtomicU32::new(BIT),
            futex: Mutex::new(()),
            sleepers: Condvar::new(),
        });
        // The write counter a consumer checks (`MessageQueue::available_to_read`).
        let counter = Arc::new(AtomicU64::new(0));
        let producer = {
            let side = Side {
                word: word.clone(),
                fence: waker_fence,
            };
            let counter = counter.clone();
            thread::spawn(move || {
                // `MessageQueue::commit_write_cached`, then the wake after the commit.
                counter.store(1, Ordering::Release);
                wake_lazy(&side, BIT).unwrap();
            })
        };
        let side = Side {
            word,
            fence: waiter_fence,
        };
        // `Consumer::next`: look, wait while there is nothing.
        while counter.load(Ordering::Acquire) == 0 {
            wait_until(&side, BIT, None).unwrap();
        }
        producer.join().unwrap();
    });
}

#[test]
fn loom_no_wake_is_lost_with_both_fences() {
    one_record(true, true);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_a_wake_is_lost_without_the_wakers_fence() {
    one_record(false, true);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_a_wake_is_lost_without_the_waiters_fence() {
    one_record(true, false);
}
