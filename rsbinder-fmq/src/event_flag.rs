// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::sys::{self, Mapping, Timespec};

/// libfmq `FMQ_NOT_FULL`: the reader sets it after consuming, the writer
/// waits on it when the ring is full.
pub const NOT_FULL: u32 = 0x01;
/// libfmq `FMQ_NOT_EMPTY`: the writer sets it after producing, the reader
/// waits on it when the ring is empty.
pub const NOT_EMPTY: u32 = 0x02;

struct Inner {
    _mapping: Mapping,
    word: NonNull<AtomicU32>,
}

// SAFETY: `word` points into `_mapping`, which lives as long as `Inner`, and
// the word is only ever touched through `AtomicU32`.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

/// A handle on the queue's 32-bit EventFlag word: the futex both peers wait
/// on and wake through (libfmq `EventFlag`, `EventFlag.cpp`).
///
/// The handle is a clone-able, `Send + Sync` reference to the mapping; it
/// keeps the word mapped after the [`MessageQueue`](crate::MessageQueue) it
/// came from is dropped, so a thread other than the one reading or writing
/// can wake a waiter (a death notification, for instance).
///
/// The protocol is libfmq's exactly, which is what makes a Rust waiter and a
/// C++ waker interoperate:
///
/// * [`wake`](Self::wake) ORs the bits into the word and issues
///   `FUTEX_WAKE_BITSET` only for bits that were clear — a bit set twice is
///   one wake.
/// * [`wait`](Self::wait) first clears the bits of its mask and returns those
///   that were set (a deferred wake); only when none were does it sleep in
///   `FUTEX_WAIT_BITSET`, then clears again.
///
/// Because a wait *consumes* the bits in its mask, a bit belongs to the side
/// that waits on it: a waiter that puts a bit it set itself into its own
/// mask erases its own signal.
#[derive(Clone)]
pub struct EventFlag {
    inner: Arc<Inner>,
}

impl EventFlag {
    pub(crate) fn from_mapping(mapping: Mapping) -> Self {
        let word = mapping.ptr().cast::<AtomicU32>();
        debug_assert_eq!(
            word.as_ptr() as usize % std::mem::align_of::<AtomicU32>(),
            0
        );
        Self {
            inner: Arc::new(Inner {
                _mapping: mapping,
                word,
            }),
        }
    }

    fn word(&self) -> &AtomicU32 {
        // SAFETY: the word lies inside the mapping `inner` owns, is 4-aligned
        // (every grantor offset is a multiple of 8 and the mapping is
        // page-aligned), and is accessed only atomically by every peer.
        unsafe { self.inner.word.as_ref() }
    }

    pub(crate) fn reset(&self) {
        self.word().store(0, Ordering::SeqCst);
    }

    /// Set `bits` and wake every waiter whose mask intersects the ones that
    /// were clear. `bits == 0` is a no-op.
    pub fn wake(&self, bits: u32) -> Result<()> {
        if bits == 0 {
            return Ok(());
        }
        let old = self.word().fetch_or(bits, Ordering::SeqCst);
        if !old & bits != 0 {
            sys::futex_wake(self.word(), bits)?;
        }
        Ok(())
    }

    /// Wait until any bit of `bits` is set, clear those bits and return them.
    /// Returns at once when some already are. `timeout == None` waits
    /// indefinitely; otherwise [`Error::TimedOut`](crate::Error::TimedOut)
    /// once it elapses. `bits == 0` is rejected.
    pub fn wait(&self, bits: u32, timeout: Option<Duration>) -> Result<u32> {
        self.wait_until(bits, sys::deadline_after(timeout)?)
    }

    /// [`wait`](Self::wait) against an absolute deadline, so a loop that
    /// waits repeatedly keeps one deadline.
    pub(crate) fn wait_until(&self, bits: u32, deadline: Option<Timespec>) -> Result<u32> {
        if bits == 0 {
            return Err(Error::BadValue("empty bit mask"));
        }
        loop {
            let old = self.word().fetch_and(!bits, Ordering::SeqCst);
            let set = old & bits;
            if set != 0 {
                return Ok(set);
            }
            // Sleep only while the word still reads as it did with our bits
            // clear; a change in between means a wake we would otherwise miss.
            sys::futex_wait(self.word(), old & !bits, bits, deadline.as_ref())?;
        }
    }

    /// The word as it is now, without clearing anything.
    pub fn peek(&self) -> u32 {
        self.word().load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for EventFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventFlag")
            .field("bits", &self.peek())
            .finish()
    }
}
