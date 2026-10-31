// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::num::NonZeroU32;
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::shm::{self, Deadline, Mapping};

/// libfmq `FMQ_NOT_FULL`: the reader sets it after consuming, the writer
/// waits on it when the ring is full.
pub const NOT_FULL: u32 = 0x01;
/// libfmq `FMQ_NOT_EMPTY`: the writer sets it after producing, the reader
/// waits on it when the ring is empty.
pub const NOT_EMPTY: u32 = 0x02;

struct Inner {
    mapping: Mapping,
}

/// A handle on the queue's 32-bit EventFlag word: the futex both peers wait
/// on and wake through (libfmq `EventFlag`, `EventFlag.cpp`).
///
/// The handle is a clone-able, `Send + Sync` reference to the mapping; it
/// keeps the word mapped after the [`MessageQueue`](crate::MessageQueue) it
/// came from is dropped, so a thread other than the one reading or writing
/// can wake a waiter (a death notification, for instance). That waiter is
/// one that calls [`wait`](Self::wait) itself and looks at the bits it
/// returns; the queue's blocking calls end only at the transfer or the
/// deadline (see [`MessageQueue::write_blocking`](crate::MessageQueue::write_blocking)).
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
        debug_assert_eq!(
            mapping.as_ptr().as_ptr() as usize % std::mem::align_of::<AtomicU32>(),
            0
        );
        Self {
            inner: Arc::new(Inner { mapping }),
        }
    }

    fn word(&self) -> &AtomicU32 {
        // SAFETY: in `inner`'s mapping, 4-aligned (grantor offsets are 8-aligned), atomic-only.
        unsafe { self.inner.mapping.as_ptr().cast::<AtomicU32>().as_ref() }
    }

    pub(crate) fn reset(&self) {
        self.word().store(0, Ordering::SeqCst);
    }

    /// Set `bits` and wake every waiter whose mask intersects the ones that
    /// were clear. `bits == 0` is a no-op.
    pub fn wake(&self, bits: u32) -> Result<()> {
        wake(self.word(), bits)
    }

    /// [`wake`](Self::wake) for a waker that has just stored what the
    /// waiter checks (a counter, after a commit): when every bit of `bits`
    /// already stands, return without writing the word.
    ///
    /// A standing bit means the waiter has not cleared it yet, and a waiter
    /// clears its bits before it sleeps, so it will see this one and not
    /// sleep. Skipping the `fetch_or` keeps the word's cache line off this
    /// core while the bit stands, which is most of the time once the waiter
    /// stops parking.
    ///
    /// The ordering is the store→load pattern on both sides. This side
    /// stores the counter, then `fence(SeqCst)`, then loads the word; a
    /// waiter clears the word with a `SeqCst` `fetch_and`, then, before it
    /// returns, `fence(SeqCst)` ([`wait`](Self::wait) does this), then loads
    /// the counter. The two fences make at least one side see the other's
    /// store: either this load sees the bit cleared and wakes, or the
    /// waiter sees the counter and does not sleep. A loom test
    /// (`src/event_flag/loom_tests.rs`) runs this code and `wait` against a
    /// modeled word, and checks that a wake is lost without either fence.
    ///
    /// A libfmq waiter (C++) has no fence between its `fetch_and` and its
    /// counter loads, so the C++ memory model alone does not promise the
    /// pattern with it. The code compilers emit for that waiter does, on the
    /// targets checked: x86 and x86-64 (the locked RMW is a full barrier),
    /// AArch64 (the RMW's release store is not reordered with a later
    /// acquire load), 32-bit ARM (`dmb ish` after the `ldrex`/`strex` loop)
    /// and RISC-V 64 (`amoand.w.aqrl`, sequentially consistent). Other
    /// targets are not checked; POWER's mapping (`hwsync; lwarx/stwcx.;
    /// isync`) does not order the RMW's store before a later load. Use
    /// [`wake`](Self::wake) where such a waiter may run.
    pub fn wake_lazy(&self, bits: u32) -> Result<()> {
        wake_lazy(self.word(), bits)
    }

    /// Wait until any bit of `bits` is set, clear those bits and return them.
    /// Returns at once when some already are. `timeout == None` waits
    /// indefinitely; otherwise [`Error::TimedOut`]
    /// once it elapses. `bits == 0` is rejected.
    pub fn wait(&self, bits: u32, timeout: Option<Duration>) -> Result<u32> {
        self.wait_until(bits, timeout.and_then(Deadline::after))
    }

    /// [`wait`](Self::wait) against an absolute deadline, so a retry loop keeps one deadline.
    pub(crate) fn wait_until(&self, bits: u32, deadline: Option<Deadline>) -> Result<u32> {
        wait_until(self.word(), bits, deadline.as_ref())
    }

    /// The word as it is now, without clearing anything.
    pub fn peek(&self) -> u32 {
        self.word().load(Ordering::SeqCst)
    }
}

/// Word and futex access for the protocol below, which picks every ordering; loom models it.
pub(crate) trait FlagWord {
    fn fetch_or(&self, bits: u32, order: Ordering) -> u32;
    fn fetch_and(&self, bits: u32, order: Ordering) -> u32;
    fn load(&self, order: Ordering) -> u32;
    fn fence(&self, order: Ordering);
    /// Sleep while the word holds `expected`, until a wake on `bits` or `deadline`.
    fn futex_wait(
        &self,
        expected: u32,
        bits: NonZeroU32,
        deadline: Option<&Deadline>,
    ) -> Result<()>;
    fn futex_wake(&self, bits: NonZeroU32) -> Result<()>;
}

impl FlagWord for AtomicU32 {
    fn fetch_or(&self, bits: u32, order: Ordering) -> u32 {
        AtomicU32::fetch_or(self, bits, order)
    }
    fn fetch_and(&self, bits: u32, order: Ordering) -> u32 {
        AtomicU32::fetch_and(self, bits, order)
    }
    fn load(&self, order: Ordering) -> u32 {
        AtomicU32::load(self, order)
    }
    fn fence(&self, order: Ordering) {
        fence(order);
    }
    fn futex_wait(
        &self,
        expected: u32,
        bits: NonZeroU32,
        deadline: Option<&Deadline>,
    ) -> Result<()> {
        shm::futex_wait(self, expected, bits, deadline)
    }
    fn futex_wake(&self, bits: NonZeroU32) -> Result<()> {
        shm::futex_wake(self, bits)
    }
}

/// [`EventFlag::wake`].
pub(crate) fn wake<W: FlagWord + ?Sized>(word: &W, bits: u32) -> Result<()> {
    let Some(mask) = NonZeroU32::new(bits) else {
        return Ok(());
    };
    let old = word.fetch_or(bits, Ordering::SeqCst);
    if !old & bits != 0 {
        word.futex_wake(mask)?;
    }
    Ok(())
}

/// [`EventFlag::wake_lazy`].
pub(crate) fn wake_lazy<W: FlagWord + ?Sized>(word: &W, bits: u32) -> Result<()> {
    word.fence(Ordering::SeqCst);
    if bits != 0 && word.load(Ordering::Relaxed) & bits == bits {
        return Ok(());
    }
    wake(word, bits)
}

/// [`EventFlag::wait_until`].
pub(crate) fn wait_until<W: FlagWord + ?Sized>(
    word: &W,
    bits: u32,
    deadline: Option<&Deadline>,
) -> Result<u32> {
    let mask = NonZeroU32::new(bits).ok_or(Error::BadValue("empty bit mask"))?;
    loop {
        let old = word.fetch_and(!bits, Ordering::SeqCst);
        let set = old & bits;
        if set != 0 {
            // Pairs with `wake_lazy`'s fence: the caller's next counter load sees its store.
            word.fence(Ordering::SeqCst);
            return Ok(set);
        }
        // Sleep only if the word is unchanged since the clear: a wake in between is kept.
        word.futex_wait(old & !bits, mask, deadline)?;
    }
}

#[cfg(all(test, loom))]
mod loom_tests;

impl std::fmt::Debug for EventFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventFlag")
            .field("bits", &self.peek())
            .finish()
    }
}
