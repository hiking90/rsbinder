// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The two memory objects of a subscription, byte for byte, and every `unsafe` block of the
//! crate.
//!
//! Both are sealed memfds. Each side maps the object it made read-write and the peer's
//! read-only, and `F_SEAL_FUTURE_WRITE` makes the kernel refuse the peer any other mapping. The
//! offsets and sizes are fixed in bytes and do not depend on the page size or on the width of a
//! pointer, since a 32-bit and a 64-bit process may share one object.
//!
//! # Topic memory
//!
//! The publisher makes one per topic and every subscriber maps it.
//!
//! | offset | size | word |
//! |---|---|---|
//! | `0x00` | 8 | `region_end`: the position the write in progress reaches |
//! | `0x08` | 8 | `write`: bytes committed |
//! | `0x10` | 4 | magic, the bytes `RSTP` |
//! | `0x14` | 4 | layout version, `major << 16 \| minor` |
//! | `0x18` | 4 | `seq`: commits announced; the word a subscriber sleeps on |
//! | `0x1c` | 4 | zero |
//! | `0x20` | 8 | `tail`: the start of the oldest record not overwritten |
//! | `0x28` | 8 | `last`: the start of the newest record |
//! | `0x30` | 8 | `half`: the start of the oldest record in the newer half of the ring |
//! | `0x38` | 8 | `frontier`: how far the chunks of the record being written reach |
//! | `0x40` | capacity | the ring (`ring.rs`) |
//!
//! The first two words and the ring are where libfmq puts the write region end, the write
//! counter and the data of an unsynchronized queue of bytes, so an `MQDescriptor` can describe
//! them. The words between are this crate's and no grantor names them. The file is exactly
//! `0x40 + capacity` bytes, every page written once at creation so that it holds data
//! (`shm::verify_received` asks that of a received memfd).
//!
//! # Control page
//!
//! A subscriber makes one per subscription and the publisher maps it. It is the only shared
//! memory a subscriber writes.
//!
//! | offset | size | word |
//! |---|---|---|
//! | `0x00` | 4 | magic, the bytes `RSTC` |
//! | `0x04` | 4 | layout version |
//! | `0x08` | 4 | `state`: bit 0 waiting, bit 1 closed, bits 2–31 the low 30 bits of the `seq` a wait sleeps on |
//! | `0x0c` | 4 | `pins`: how many pin words follow the header |
//! | `0x40` | 8 | `read`: the subscriber's read position, for statistics, lends and backpressure |
//! | `0x48` | 8 | `lost`: records it reported as lost, for statistics |
//! | `0x80` | 4 × pins | pin words (plan 15, loans) |
//!
//! The rest of the first `0x80` bytes is zero. `state` has a cache line of its own because the
//! publisher loads it at every commit, while the subscriber stores `read` at every record. A pin
//! word is 0 or a slot's number plus one (`pool.rs`).
//!
//! # Pool memory
//!
//! A publisher whose policy has a pool makes one per topic and every subscriber maps it
//! (`pool.rs`).
//!
//! | offset | size | word |
//! |---|---|---|
//! | `0x00` | 4 | magic, the bytes `RSTL` |
//! | `0x04` | 4 | layout version |
//! | `0x08` | 4 | `slots` |
//! | `0x0c` | 4 | `slot_bytes`: a multiple of 64, at least 64 |
//! | `0x40` | 8 × slots | each slot's generation |
//! | `area` | slots × slot_bytes | the slots; `area` is `0x40 + 8 × slots` rounded up to 64 |
//!
//! The file is exactly `area + slots × slot_bytes` bytes and at most [`MAX_POOL`], every page
//! written once at creation. A slot starts on a multiple of 64, so no two slots share a cache line.
//!
//! # Making and mapping
//!
//! The six entry points — [`create_topic`], [`map_topic`], [`create_pool`], [`map_pool`],
//! [`ControlPage::create`] and [`ControlView::map`] — return `Unsupported` before they look at an
//! argument where topics cannot work: off Linux and Android, and where `shm::RoWord64::PRESENT` is
//! `false` (no 8-byte load of a read-only mapping is known not to write there). The three that
//! make a memfd return it as well on a kernel without `F_SEAL_FUTURE_WRITE`; there is no way to
//! make any of the objects without that seal.
//!
//! The three that map check the fd first (`shm::verify_received`: a sealed memfd with every page
//! present), then its size, then the words that say what it is. For topic memory the size gives
//! the ring's capacity, and the caller's `max_ring` bounds it; for pool memory the endpoint gives
//! the slots, the size has to match them, and the caller's `max_pool` bounds it. Both bound the
//! address space a peer can make this process map.
//!
//! # The views
//!
//! [`Stored`] and [`Loaded`] are topic memory as its writer and as a reader reach it. Both are an
//! address and a capacity; the constructor's contract is that `0x40 + capacity` bytes at an
//! 8-aligned address stay alive with the owner the view holds, are initialized, and are accessed
//! through atomics only. The code here adds what makes the accesses consistent in size: the
//! 8-byte and 4-byte words lie at the offsets above and nowhere else, and a ring copy starts on a
//! multiple of 8 and is a multiple of 8 long, so every byte of the ring is always part of the
//! same `usize`. That holds when two views share one allocation, as they do in the tests that
//! run on heap memory.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use rsbinder::fmq::shm::{
    self, Access, CreateOptions, Mapping, RoRun, RoWord32, RoWord64, Run, Seals,
};
use rsbinder::fmq::{Descriptor, Error, Flavor, Grantor, Result};

use crate::pool::{GenerationLoads, Generations, LendError, Pin, Pins, Slots};
use crate::ring::{Loads, Stores};

/// The layout version this crate reads and writes: 0.3. A subscriber stores its `read` with
/// `Release` and answers `ISubscription.evicted` (plan 15 §6.5), and the publisher stores
/// `frontier` before each chunk of a record, which a subscriber judges its copies by (`ring.rs`).
/// A peer of another version is refused.
pub(crate) const VERSION: u32 = 3;

/// The smallest ring. An end record with its message has to fit half of it.
pub(crate) const MIN_RING: usize = 4 << 10;

/// The largest ring. A record's length has 30 bits, and half of this fits them.
pub(crate) const MAX_RING: usize = 1 << 30;

const TOPIC_MAGIC: u32 = u32::from_ne_bytes(*b"RSTP");
const CONTROL_MAGIC: u32 = u32::from_ne_bytes(*b"RSTC");

/// Offsets in topic memory.
mod topic {
    pub(super) const REGION_END: usize = 0x00;
    pub(super) const WRITE: usize = 0x08;
    pub(super) const MAGIC: usize = 0x10;
    pub(super) const VERSION: usize = 0x14;
    pub(super) const SEQ: usize = 0x18;
    pub(super) const TAIL: usize = 0x20;
    pub(super) const LAST: usize = 0x28;
    pub(super) const HALF: usize = 0x30;
    pub(super) const FRONTIER: usize = 0x38;
    pub(super) const RING: usize = 0x40;
}

/// Offsets in a control page.
mod control {
    pub(super) const MAGIC: usize = 0x00;
    pub(super) const VERSION: usize = 0x04;
    pub(super) const STATE: usize = 0x08;
    pub(super) const PINS: usize = 0x0c;
    pub(super) const READ: usize = 0x40;
    pub(super) const LOST: usize = 0x48;
    pub(super) const PIN: u64 = 0x80;
}

/// Offsets in pool memory.
mod pool {
    pub(super) const MAGIC: usize = 0x00;
    pub(super) const VERSION: usize = 0x04;
    pub(super) const SLOTS: usize = 0x08;
    pub(super) const SLOT_BYTES: usize = 0x0c;
    pub(super) const GENERATIONS: usize = 0x40;
}

const POOL_MAGIC: u32 = u32::from_ne_bytes(*b"RSTL");

/// A slot's size is a multiple of this, and at least this: a cache line.
pub(crate) const SLOT_GRAIN: u32 = 64;

/// The largest pool memory, header included.
pub(crate) const MAX_POOL: u64 = 4 << 30;

/// `state` bit 0: the subscriber is about to sleep, or sleeps, on `seq`.
pub(crate) const WAITING: u32 = 1;

/// `state` bit 1: the subscriber has dropped the subscription.
pub(crate) const CLOSED: u32 = 2;

/// `state` bits 2–31: the low 30 bits of the `seq` the wait sleeps on (plan 15 §5.5). They tell
/// one wait from the next, and the publisher wakes no wait that has seen its commit.
pub(crate) const SEEN_SHIFT: u32 = 2;

/// The `seen` field of `state` for a wait on `seq`.
pub(crate) fn seen_field(seq: u32) -> u32 {
    seq << SEEN_SHIFT
}

/// `Unsupported` where topics cannot work: off Linux and Android, or where `RoWord64` has no value.
pub(crate) fn check_target() -> Result<()> {
    if RoWord64::PRESENT && cfg!(any(target_os = "linux", target_os = "android")) {
        Ok(())
    } else {
        Err(Error::Unsupported)
    }
}

#[cfg(test)]
thread_local! {
    /// Set by a test: `add_seals` then answers as a kernel without `F_SEAL_FUTURE_WRITE` does.
    #[cfg_attr(
        target_os = "android",
        expect(
            clippy::missing_const_for_thread_local,
            reason = "the initializer is `const`; Android's expansion of the macro hides that"
        )
    )]
    pub(crate) static NO_FUTURE_WRITE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Add `seals` to `fd`: `shm::add_seals`, behind the switch above.
fn add_seals(fd: BorrowedFd<'_>, seals: Seals) -> Result<()> {
    #[cfg(test)]
    if seals.contains(Seals::FUTURE_WRITE) && NO_FUTURE_WRITE.get() {
        return Err(Error::Os(rustix::io::Errno::INVAL));
    }
    shm::add_seals(fd, seals)
}

/// Seal a memfd this process has mapped: no resizing, no new writable mapping, no further seal.
fn seal(fd: BorrowedFd<'_>) -> Result<()> {
    let seals = Seals::GROW | Seals::SHRINK | Seals::FUTURE_WRITE | Seals::SEAL;
    match add_seals(fd, seals) {
        // The kernel applies none of a mask that holds a seal it does not know.
        Err(Error::Os(rustix::io::Errno::INVAL)) => Err(Error::Unsupported),
        other => other,
    }
}

/// The start of a ring copy and the bytes of it that precede the ring's end.
fn split(capacity: usize, position: u64, len: usize) -> (usize, usize) {
    assert!(
        position % 8 == 0 && len % 8 == 0 && len <= capacity,
        "a ring copy off the 8-byte grid or longer than the ring"
    );
    let offset = (position % capacity as u64) as usize;
    (offset, len.min(capacity - offset))
}

/// Topic memory as its one writer reaches it; `O` keeps the bytes alive.
pub(crate) struct Stored<O> {
    base: NonNull<u8>,
    capacity: usize,
    _owner: O,
}

// SAFETY: an address into memory `O` keeps alive, reached through atomics only.
unsafe impl<O: Send> Send for Stored<O> {}

impl<O> Stored<O> {
    /// # Safety: the module's "The views" contract, with the bytes valid for writes.
    unsafe fn new(base: NonNull<u8>, capacity: usize, owner: O) -> Self {
        assert!(capacity != 0 && capacity % 8 == 0, "a ring off the grid");
        Self {
            base,
            capacity,
            _owner: owner,
        }
    }

    fn word64(&self, at: usize) -> &AtomicU64 {
        debug_assert!(at % 8 == 0 && at + 8 <= topic::RING);
        // SAFETY: `new`'s contract: an aligned 8-byte word of the header, atomics only, one size.
        unsafe { self.base.add(at).cast::<AtomicU64>().as_ref() }
    }

    fn word32(&self, at: usize) -> &AtomicU32 {
        debug_assert!(at % 4 == 0 && at + 4 <= topic::RING);
        // SAFETY: `new`'s contract: an aligned 4-byte word of the header, atomics only, one size.
        unsafe { self.base.add(at).cast::<AtomicU32>().as_ref() }
    }

    fn ring(&self, offset: usize, len: usize) -> Run<'_> {
        assert!(offset <= self.capacity && len <= self.capacity - offset);
        // SAFETY: `new`'s contract and the assert: inside the ring, which `split` tiles in words.
        unsafe { Run::new(self.base.add(topic::RING + offset), len) }
    }

    /// Write the two words that say what this memory is. Before any peer has the fd.
    fn mark(&self) {
        self.word32(topic::MAGIC)
            .store(TOPIC_MAGIC, Ordering::Relaxed);
        self.word32(topic::VERSION)
            .store(VERSION, Ordering::Relaxed);
    }
}

impl<O> Stores for Stored<O> {
    fn capacity(&self) -> u64 {
        self.capacity as u64
    }

    fn store_tail(&self, position: u64) {
        self.word64(topic::TAIL).store(position, Ordering::Release);
    }

    fn store_region_end(&self, position: u64) {
        self.word64(topic::REGION_END)
            .store(position, Ordering::Release);
    }

    fn store_frontier(&self, position: u64) {
        self.word64(topic::FRONTIER)
            .store(position, Ordering::Release);
    }

    fn store_write(&self, position: u64) {
        self.word64(topic::WRITE).store(position, Ordering::Release);
    }

    fn store_last(&self, position: u64) {
        self.word64(topic::LAST).store(position, Ordering::Release);
    }

    fn store_half(&self, position: u64) {
        self.word64(topic::HALF).store(position, Ordering::Release);
    }

    fn bump_seq(&self) -> u32 {
        let seq = self.word32(topic::SEQ).fetch_add(1, Ordering::SeqCst);
        seq.wrapping_add(1)
    }

    fn wake_seq(&self) {
        // Fails only where the system call does not exist; there no subscriber can sleep either.
        let _ = shm::futex_wake(self.word32(topic::SEQ), u32::MAX);
    }

    fn store_ring(&self, position: u64, src: &[u8]) {
        let (offset, first) = split(self.capacity, position, src.len());
        let (head, wrapped) = src.split_at(first);
        self.ring(offset, head.len()).store_release(head);
        self.ring(0, wrapped.len()).store_release(wrapped);
    }
}

/// Topic memory as a reader reaches it: loads, and nothing that stores. `O` keeps it alive.
pub(crate) struct Loaded<O> {
    base: NonNull<u8>,
    capacity: usize,
    _owner: O,
}

// SAFETY: an address into memory `O` keeps alive, reached through atomic loads only.
unsafe impl<O: Send> Send for Loaded<O> {}

impl<O> Loaded<O> {
    /// # Safety: the module's "The views" contract; the bytes need not be valid for writes.
    unsafe fn new(base: NonNull<u8>, capacity: usize, owner: O) -> Self {
        assert!(capacity != 0 && capacity % 8 == 0, "a ring off the grid");
        Self {
            base,
            capacity,
            _owner: owner,
        }
    }

    fn word64(&self, at: usize) -> &RoWord64 {
        debug_assert!(at % 8 == 0 && at + 8 <= topic::RING);
        // SAFETY: `new`'s contract: an aligned 8-byte word of the header, atomics only, one size.
        unsafe { RoWord64::from_ptr(self.base.add(at)) }
    }

    fn word32(&self, at: usize) -> &RoWord32 {
        debug_assert!(at % 4 == 0 && at + 4 <= topic::RING);
        // SAFETY: `new`'s contract: an aligned 4-byte word of the header, atomics only, one size.
        unsafe { RoWord32::from_ptr(self.base.add(at)) }
    }

    fn ring(&self, offset: usize, len: usize) -> RoRun<'_> {
        assert!(offset <= self.capacity && len <= self.capacity - offset);
        // SAFETY: `new`'s contract and the assert: inside the ring, which `split` tiles in words.
        unsafe { RoRun::new(self.base.add(topic::RING + offset), len) }
    }

    /// The word a subscriber sleeps on, as a handle that keeps the memory alive on its own.
    pub(crate) fn seq(&self) -> Seq<O>
    where
        O: Clone,
    {
        Seq {
            base: self.base,
            _owner: self._owner.clone(),
        }
    }

    /// Whether the two words that say what this memory is hold what this crate writes there.
    fn marked(&self) -> Result<()> {
        if self.word32(topic::MAGIC).load() != TOPIC_MAGIC {
            return Err(Error::BadValue("received memfd is not topic memory"));
        }
        if self.word32(topic::VERSION).load() != VERSION {
            return Err(Error::BadValue("topic memory of another layout version"));
        }
        Ok(())
    }
}

impl<O> Loads for Loaded<O> {
    fn capacity(&self) -> u64 {
        self.capacity as u64
    }

    fn tail(&self) -> u64 {
        self.word64(topic::TAIL).load_acquire()
    }

    fn frontier(&self) -> u64 {
        self.word64(topic::FRONTIER).load_acquire()
    }

    fn write(&self) -> u64 {
        self.word64(topic::WRITE).load_acquire()
    }

    fn last(&self) -> u64 {
        self.word64(topic::LAST).load_acquire()
    }

    fn half(&self) -> u64 {
        self.word64(topic::HALF).load_acquire()
    }

    fn load_ring(&self, position: u64, dst: &mut [u8]) {
        let (offset, first) = split(self.capacity, position, dst.len());
        let (head, wrapped) = dst.split_at_mut(first);
        self.ring(offset, head.len()).load(head);
        self.ring(0, wrapped.len()).load(wrapped);
    }

    fn load_ring_extend(&self, position: u64, len: usize, dst: &mut Vec<u8>) {
        let (offset, first) = split(self.capacity, position, len);
        self.ring(offset, first).load_extend(dst);
        self.ring(0, len - first).load_extend(dst);
    }

    fn fence(&self) {
        // What `RoRun::load_acquire` adds to `load`.
        std::sync::atomic::fence(Ordering::Acquire);
    }
}

/// Topic memory's `seq`, apart from the reader: what a wait on another thread sleeps on and wakes.
pub(crate) struct Seq<O> {
    base: NonNull<u8>,
    _owner: O,
}

// SAFETY: an address into memory `O` keeps alive, reached through `RoWord32` only.
unsafe impl<O: Send> Send for Seq<O> {}
// SAFETY: as above; `RoWord32` is an atomic, so `&Seq` on two threads is two atomic accesses.
unsafe impl<O: Sync> Sync for Seq<O> {}

impl<O> Seq<O> {
    pub(crate) fn word(&self) -> &RoWord32 {
        // SAFETY: the `Loaded` this came from: an aligned 4-byte word of the header, one size.
        unsafe { RoWord32::from_ptr(self.base.add(topic::SEQ)) }
    }
}

/// The ring's size for a request of `ring_bytes`: rounded up to 8, within the two bounds.
fn ring_capacity(ring_bytes: usize) -> Result<usize> {
    if !(MIN_RING..=MAX_RING).contains(&ring_bytes) {
        return Err(Error::BadValue("ring size outside 4 KiB to 1 GiB"));
    }
    Ok(ring_bytes.div_ceil(8) * 8)
}

/// Make topic memory with a ring of `ring_bytes`: the fd to hand to subscribers, and the mapping.
pub(crate) fn create_topic(ring_bytes: usize) -> Result<(OwnedFd, Stored<Mapping>)> {
    check_target()?;
    let capacity = ring_capacity(ring_bytes)?;
    let size = (topic::RING + capacity) as u64;
    let options = CreateOptions::new("rsbinder-topic").populate(true);
    let fd = shm::create(size, &options)?;
    let mapping = Mapping::map(fd.as_fd(), 0, size, Access::ReadWrite)?;
    // SAFETY: a page-aligned mapping of `size` zeroed bytes, owned by the view, atomics only.
    let stored = unsafe { Stored::new(mapping.as_ptr(), capacity, mapping) };
    stored.mark();
    // After the mapping: `FUTURE_WRITE` refuses every writable mapping made from here on.
    seal(fd.as_fd())?;
    Ok((fd, stored))
}

/// Map topic memory a publisher sent, read-only, once the fd, its size and its marks pass.
pub(crate) fn map_topic(fd: BorrowedFd<'_>, max_ring: usize) -> Result<Loaded<Arc<Mapping>>> {
    check_target()?;
    let size = shm::verify_received(fd)?;
    let capacity = usize::try_from(size)
        .ok()
        .and_then(|size| size.checked_sub(topic::RING))
        .filter(|ring| ring % 8 == 0 && (MIN_RING..=MAX_RING.min(max_ring)).contains(ring))
        .ok_or(Error::BadValue("topic memory of a size outside the bounds"))?;
    // Shared, so that a `Seq` taken from the view outlives a dropped reader.
    let mapping = Arc::new(Mapping::map(fd, 0, size, Access::ReadOnly)?);
    // SAFETY: a page-aligned mapping of `size` bytes `verify_received` found present and sealed.
    let loaded = unsafe { Loaded::new(mapping.as_ptr(), capacity, mapping) };
    loaded.marked()?;
    Ok(loaded)
}

/// Where the words between the counters and the ring start: magic, version, `seq` and the rest.
pub(crate) const HEADER_OFFSET: usize = topic::MAGIC;

/// The grantors of topic memory with a ring of `capacity` bytes, as libfmq's unsynchronized
/// queue of bytes has them in one fd: write region end, write counter, ring.
pub(crate) fn grantors(capacity: u64) -> Vec<Grantor> {
    let grantor = |offset: usize, extent| Grantor {
        fd_index: 0,
        offset: offset as u32,
        extent,
    };
    vec![
        grantor(topic::REGION_END, 8),
        grantor(topic::WRITE, 8),
        grantor(topic::RING, capacity),
    ]
}

/// The descriptor of topic memory: `fd`, a duplicate of the publisher's, and the grantors.
pub(crate) fn descriptor(fd: OwnedFd, capacity: u64) -> Descriptor {
    Descriptor {
        fds: vec![fd],
        ints: Vec::new(),
        grantors: grantors(capacity),
        quantum: 1,
        flavor: Flavor::UnsynchronizedWrite,
    }
}

/// Bytes of a control page with `pins` pin words.
fn control_size(pins: u32) -> u64 {
    (control::PIN + 4 * u64::from(pins)).div_ceil(8) * 8
}

/// A subscriber's control page, mapped read-write.
pub(crate) struct ControlPage {
    mapping: Mapping,
    pins: u32,
}

impl ControlPage {
    /// Make a control page with `pins` pin words: the fd to hand to the publisher, and the page.
    /// The errors are [`create_topic`]'s.
    pub(crate) fn create(pins: u32) -> Result<(OwnedFd, Self)> {
        check_target()?;
        let size = control_size(pins);
        let options = CreateOptions::new("rsbinder-topic-control").populate(true);
        let fd = shm::create(size, &options)?;
        let mapping = Mapping::map(fd.as_fd(), 0, size, Access::ReadWrite)?;
        let page = Self { mapping, pins };
        page.word32(control::MAGIC)
            .store(CONTROL_MAGIC, Ordering::Relaxed);
        page.word32(control::VERSION)
            .store(VERSION, Ordering::Relaxed);
        page.word32(control::PINS).store(pins, Ordering::Relaxed);
        seal(fd.as_fd())?;
        Ok((fd, page))
    }

    fn word32(&self, at: usize) -> &AtomicU32 {
        debug_assert!(at % 4 == 0 && at + 4 <= control::READ);
        // SAFETY: an aligned 4-byte word of the page's first line, in a mapping `self` owns.
        unsafe { self.mapping.as_ptr().add(at).cast::<AtomicU32>().as_ref() }
    }

    fn word64(&self, at: usize) -> &AtomicU64 {
        debug_assert!(at == control::READ || at == control::LOST);
        // SAFETY: an aligned 8-byte word of the page's second line, in a mapping `self` owns.
        unsafe { self.mapping.as_ptr().add(at).cast::<AtomicU64>().as_ref() }
    }

    /// The [`WAITING`] and [`CLOSED`] bits and the `seq` a wait sleeps on. Changed by RMWs alone.
    pub(crate) fn state(&self) -> &AtomicU32 {
        self.word32(control::STATE)
    }

    /// The subscriber's read position, which the publisher reads for statistics, the pool's lends
    /// and, with backpressure, its waits.
    pub(crate) fn read(&self) -> &AtomicU64 {
        self.word64(control::READ)
    }

    /// Records the subscriber reported as lost, for statistics only.
    pub(crate) fn lost(&self) -> &AtomicU64 {
        self.word64(control::LOST)
    }

    /// Pin word `nth`: 0, or the number of the slot this subscriber reads plus one.
    fn pin(&self, nth: u32) -> &AtomicU32 {
        assert!(nth < self.pins, "a pin word the page does not have");
        let at = control::PIN as usize + 4 * nth as usize;
        // SAFETY: an aligned 4-byte word below `control_size(pins)`, in a mapping `self` owns.
        unsafe { self.mapping.as_ptr().add(at).cast::<AtomicU32>().as_ref() }
    }
}

impl Pin for ControlPage {
    fn store_pin(&self, value: u32, order: Ordering) {
        self.pin(0).store(value, order);
    }
}

/// A subscriber's control page as the publisher maps it: read-only.
pub(crate) struct ControlView {
    mapping: Mapping,
    pins: u32,
}

impl ControlView {
    /// Map a control page a subscriber sent, after checking the fd, its size and what it holds.
    pub(crate) fn map(fd: BorrowedFd<'_>, pins: u32) -> Result<Self> {
        check_target()?;
        let size = shm::verify_received(fd)?;
        if size != control_size(pins) {
            return Err(Error::BadValue("control page of another size"));
        }
        let view = Self {
            mapping: Mapping::map(fd, 0, size, Access::ReadOnly)?,
            pins,
        };
        if view.word32(control::MAGIC).load() != CONTROL_MAGIC {
            return Err(Error::BadValue("received memfd is not a control page"));
        }
        if view.word32(control::VERSION).load() != VERSION {
            return Err(Error::BadValue("control page of another layout version"));
        }
        if view.word32(control::PINS).load() != pins {
            return Err(Error::BadValue("control page with another pin count"));
        }
        Ok(view)
    }

    fn word32(&self, at: usize) -> &RoWord32 {
        debug_assert!(at % 4 == 0 && at + 4 <= control::READ);
        // SAFETY: an aligned 4-byte word of the page's first line, in a mapping `self` owns.
        unsafe { RoWord32::from_ptr(self.mapping.as_ptr().add(at)) }
    }

    fn word64(&self, at: usize) -> &RoWord64 {
        debug_assert!(at == control::READ || at == control::LOST);
        // SAFETY: an aligned 8-byte word of the page's second line, in a mapping `self` owns.
        unsafe { RoWord64::from_ptr(self.mapping.as_ptr().add(at)) }
    }

    /// The subscriber's [`WAITING`] and [`CLOSED`] bits and the `seq` its wait sleeps on.
    pub(crate) fn state(&self) -> &RoWord32 {
        self.word32(control::STATE)
    }

    /// What the subscriber says its read position is.
    pub(crate) fn read(&self) -> u64 {
        self.word64(control::READ).load_acquire()
    }

    /// How many records the subscriber says it lost.
    pub(crate) fn lost(&self) -> u64 {
        self.word64(control::LOST).load_acquire()
    }

    /// The pin words the page has.
    pub(crate) fn pins(&self) -> u32 {
        self.pins
    }

    /// Pin word `nth`, a `Relaxed` load: the caller fences (`pool.rs`).
    pub(crate) fn pin(&self, nth: u32) -> u32 {
        assert!(nth < self.pins, "a pin word the page does not have");
        let at = control::PIN as usize + 4 * nth as usize;
        // SAFETY: an aligned 4-byte word below `control_size(pins)`, in a mapping `self` owns.
        unsafe { RoWord32::from_ptr(self.mapping.as_ptr().add(at)) }.load()
    }
}

/// Bytes before the first slot: the header and the generations, rounded up to a cache line.
fn slot_area(slots: u32) -> u64 {
    (pool::GENERATIONS as u64 + 8 * u64::from(slots)).div_ceil(64) * 64
}

/// Bytes of pool memory with `slots` slots of `slot_bytes`, if those are a pool's.
pub(crate) fn pool_size(slots: u32, slot_bytes: u32) -> Option<u64> {
    if slots == 0 || slot_bytes < SLOT_GRAIN || slot_bytes % SLOT_GRAIN != 0 {
        return None;
    }
    let size = slot_area(slots).checked_add(u64::from(slots) * u64::from(slot_bytes))?;
    (size <= MAX_POOL && usize::try_from(size).is_ok()).then_some(size)
}

/// Pool memory as the publisher reaches it. Its slots are reached only through [`LentSlot`]s.
pub(crate) struct StoredPool {
    /// Shared with each [`LentSlot`], so that a loan outlives no mapping.
    mapping: Arc<Mapping>,
    slots: u32,
    slot_bytes: u32,
}

impl StoredPool {
    fn word32(&self, at: usize) -> &AtomicU32 {
        debug_assert!(at % 4 == 0 && at + 4 <= pool::GENERATIONS);
        // SAFETY: an aligned 4-byte word of the header, in a mapping `self` owns.
        unsafe { self.mapping.as_ptr().add(at).cast::<AtomicU32>().as_ref() }
    }

    fn generation(&self, slot: u32) -> &AtomicU64 {
        assert!(slot < self.slots, "a slot outside the pool");
        let at = pool::GENERATIONS + 8 * slot as usize;
        // SAFETY: an aligned 8-byte word below `slot_area(slots)`, in a mapping `self` owns.
        unsafe { self.mapping.as_ptr().add(at).cast::<AtomicU64>().as_ref() }
    }

    pub(crate) fn slot_bytes(&self) -> u32 {
        self.slot_bytes
    }

    /// Lend a slot for a value of `len` bytes, at most [`slot_bytes`](Self::slot_bytes):
    /// [`Slots::lend`] with this pool's generations, and the slot's bytes.
    pub(crate) fn lend(
        &self,
        slots: &mut Slots,
        pins: &impl Pins,
        read: u64,
        len: usize,
    ) -> std::result::Result<LentSlot, LendError> {
        assert!(len <= self.slot_bytes as usize, "a loan longer than a slot");
        let slot = slots.lend(self, pins, read)?;
        let at = slot_area(self.slots) as usize + slot as usize * self.slot_bytes as usize;
        Ok(LentSlot {
            _mapping: self.mapping.clone(),
            slot,
            // SAFETY: inside the slot, in the mapping the value keeps alive.
            at: unsafe { self.mapping.as_ptr().add(at) },
            len,
        })
    }
}

/// The bytes of a slot the publisher lent: `Slots` lends it to this value alone until the slot
/// is published or given back, which takes the value.
pub(crate) struct LentSlot {
    _mapping: Arc<Mapping>,
    slot: u32,
    at: NonNull<u8>,
    len: usize,
}

impl LentSlot {
    pub(crate) fn slot(&self) -> u32 {
        self.slot
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: a lent slot, which nothing else in this process reaches; initialized memfd pages.
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }

    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes`, and `&mut self` makes this the only reference.
        unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr(), self.len) }
    }
}

impl Generations for StoredPool {
    fn store_generation(&self, slot: u32, value: u64, order: Ordering) {
        self.generation(slot).store(value, order);
    }
}

/// Pool memory as a subscriber reaches it: loads only.
pub(crate) struct LoadedPool {
    mapping: Mapping,
    slots: u32,
    slot_bytes: u32,
}

impl LoadedPool {
    fn word32(&self, at: usize) -> &RoWord32 {
        debug_assert!(at % 4 == 0 && at + 4 <= pool::GENERATIONS);
        // SAFETY: an aligned 4-byte word of the header, in a mapping `self` owns.
        unsafe { RoWord32::from_ptr(self.mapping.as_ptr().add(at)) }
    }

    pub(crate) fn slot_bytes(&self) -> u32 {
        self.slot_bytes
    }

    /// Fill `dst` from the start of `slot`, with `Relaxed` loads: the caller pinned it.
    pub(crate) fn load_slot(&self, slot: u32, dst: &mut [u8]) {
        self.slot_run(slot, dst.len()).load(dst);
    }

    /// [`load_slot`](Self::load_slot) of `len` bytes appended to `dst`, not zeroed first.
    pub(crate) fn load_slot_extend(&self, slot: u32, len: usize, dst: &mut Vec<u8>) {
        self.slot_run(slot, len).load_extend(dst);
    }

    fn slot_run(&self, slot: u32, len: usize) -> RoRun<'_> {
        assert!(slot < self.slots && len <= self.slot_bytes as usize);
        let at = slot_area(self.slots) as usize + slot as usize * self.slot_bytes as usize;
        // SAFETY: a slot of a mapping `self` owns; no `slot_slice` slice is live (its contract).
        unsafe { RoRun::new(self.mapping.as_ptr().add(at), len) }
    }

    /// The first `len` bytes of `slot`, in place.
    ///
    /// # Safety
    ///
    /// While the slice lives, no process writes those bytes — the publisher keeps the pin
    /// protocol (`pool.rs`) and the caller holds the pin — and this process reads them through
    /// no run ([`load_slot`](Self::load_slot)).
    pub(crate) unsafe fn slot_slice(&self, slot: u32, len: usize) -> &[u8] {
        assert!(slot < self.slots && len <= self.slot_bytes as usize);
        let at = slot_area(self.slots) as usize + slot as usize * self.slot_bytes as usize;
        // SAFETY: inside a slot of a mapping `self` owns; the caller's contract keeps it unchanged.
        unsafe { std::slice::from_raw_parts(self.mapping.as_ptr().add(at).as_ptr(), len) }
    }

    /// The addresses the mapping covers, for tests that a sample lies in it.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn addresses(&self) -> std::ops::Range<usize> {
        let start = self.mapping.as_ptr().as_ptr() as usize;
        start..start + self.mapping.len()
    }
}

impl GenerationLoads for LoadedPool {
    fn slots(&self) -> u32 {
        self.slots
    }

    fn load_generation(&self, slot: u32) -> u64 {
        assert!(slot < self.slots, "a slot outside the pool");
        let at = pool::GENERATIONS + 8 * slot as usize;
        // SAFETY: an aligned 8-byte word below `slot_area(slots)`, in a mapping `self` owns.
        unsafe { RoWord64::from_ptr(self.mapping.as_ptr().add(at)) }.load()
    }
}

/// Make pool memory of `slots` slots of `slot_bytes`: the fd to hand to subscribers, and the
/// mapping. `huge` asks the kernel for transparent huge pages (`MADV_HUGEPAGE`) before the pages
/// are first written. `BadValue` for sizes [`pool_size`] refuses; the rest as [`create_topic`].
pub(crate) fn create_pool(
    slots: u32,
    slot_bytes: u32,
    huge: bool,
) -> Result<(OwnedFd, StoredPool)> {
    check_target()?;
    let size =
        pool_size(slots, slot_bytes).ok_or(Error::BadValue("pool of a size outside the bounds"))?;
    // Huge pages come with the first write through a mapping advised so, not with `pwrite`.
    let options = CreateOptions::new("rsbinder-topic-pool").populate(!huge);
    let fd = shm::create(size, &options)?;
    let mapping = Arc::new(Mapping::map(fd.as_fd(), 0, size, Access::ReadWrite)?);
    let stored = StoredPool {
        mapping,
        slots,
        slot_bytes,
    };
    if huge {
        stored.touch_huge();
    }
    stored
        .word32(pool::MAGIC)
        .store(POOL_MAGIC, Ordering::Relaxed);
    stored
        .word32(pool::VERSION)
        .store(VERSION, Ordering::Relaxed);
    stored.word32(pool::SLOTS).store(slots, Ordering::Relaxed);
    stored
        .word32(pool::SLOT_BYTES)
        .store(slot_bytes, Ordering::Relaxed);
    seal(fd.as_fd())?;
    Ok((fd, stored))
}

impl StoredPool {
    /// Advise huge pages, then write a byte of every 4 KiB so that each page holds data.
    fn touch_huge(&self) {
        let len = self.mapping.len();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // Refused without transparent huge pages, which leaves small ones: nothing to undo.
            let advice = rustix::mm::Advice::LinuxHugepage;
            let at = self.mapping.as_ptr().as_ptr().cast();
            // SAFETY: this whole mapping, and an advice that changes no byte of it.
            let _ = unsafe { rustix::mm::madvise(at, len, advice) };
        }
        for at in (0..len).step_by(4 << 10) {
            // SAFETY: a byte this mapping owns, before any peer has the fd: no access races it.
            unsafe { self.mapping.as_ptr().add(at).cast::<AtomicU8>().as_ref() }
                .store(0, Ordering::Relaxed);
        }
    }
}

/// Map pool memory a publisher sent, read-only, once the fd, its size and its marks pass: the
/// size has to be what `slots` and `slot_bytes` make, and no more than `max_pool`.
pub(crate) fn map_pool(
    fd: BorrowedFd<'_>,
    slots: u32,
    slot_bytes: u32,
    max_pool: usize,
) -> Result<LoadedPool> {
    check_target()?;
    let expected = pool_size(slots, slot_bytes)
        .filter(|&size| size <= max_pool as u64)
        .ok_or(Error::BadValue("pool memory of a size outside the bounds"))?;
    let size = shm::verify_received(fd)?;
    if size != expected {
        return Err(Error::BadValue(
            "pool memory of another size than its slots",
        ));
    }
    let pool = LoadedPool {
        mapping: Mapping::map(fd, 0, size, Access::ReadOnly)?,
        slots,
        slot_bytes,
    };
    if pool.word32(pool::MAGIC).load() != POOL_MAGIC {
        return Err(Error::BadValue("received memfd is not pool memory"));
    }
    if pool.word32(pool::VERSION).load() != VERSION {
        return Err(Error::BadValue("pool memory of another layout version"));
    }
    if pool.word32(pool::SLOTS).load() != slots
        || pool.word32(pool::SLOT_BYTES).load() != slot_bytes
    {
        return Err(Error::BadValue(
            "pool memory with other slots than its endpoint",
        ));
    }
    Ok(pool)
}

#[cfg(any(test, feature = "fuzzing"))]
pub(crate) use heap::Heap;

#[cfg(any(test, feature = "fuzzing"))]
mod heap {
    use std::sync::Arc;

    use super::*;

    /// Topic memory on the heap, for what cannot map a memfd: Miri, and a fuzzer's input.
    pub(crate) struct Heap {
        words: Arc<[AtomicU64]>,
        capacity: usize,
    }

    impl Heap {
        /// Zeroed topic memory with a ring of `capacity` bytes, marked as `create_topic` marks it.
        #[cfg(test)]
        pub(crate) fn new(capacity: usize) -> Self {
            let heap = Self::zeroed(capacity);
            heap.stored().mark();
            heap
        }

        /// Topic memory holding `image`: 64 bytes of header, then the ring, cut to a multiple of 8.
        pub(crate) fn from_image(image: &[u8]) -> Option<Self> {
            let capacity = image.len().checked_sub(topic::RING)? / 8 * 8;
            if capacity == 0 {
                return None;
            }
            // Built with `new`, not stored: Miri's weak-memory emulation takes one size per byte.
            let words = image[..topic::RING + capacity]
                .chunks_exact(8)
                .map(|bytes| {
                    let mut value = [0; 8];
                    value.copy_from_slice(bytes);
                    AtomicU64::new(u64::from_ne_bytes(value))
                });
            Some(Self {
                words: words.collect(),
                capacity,
            })
        }

        #[cfg(test)]
        fn zeroed(capacity: usize) -> Self {
            let words = (topic::RING + capacity) / 8;
            Self {
                words: (0..words).map(|_| AtomicU64::new(0)).collect(),
                capacity,
            }
        }

        fn base(&self) -> NonNull<u8> {
            NonNull::from(&*self.words).cast()
        }

        /// A writer's view. Use one at a time, as there is one publisher.
        #[cfg(test)]
        pub(crate) fn stored(&self) -> Stored<Arc<[AtomicU64]>> {
            // SAFETY: 8-aligned initialized atomics the view's `Arc` keeps alive; atomics only.
            unsafe { Stored::new(self.base(), self.capacity, self.words.clone()) }
        }

        /// A reader's view.
        pub(crate) fn loaded(&self) -> Loaded<Arc<[AtomicU64]>> {
            // SAFETY: as in `stored`; the reader only loads.
            unsafe { Loaded::new(self.base(), self.capacity, self.words.clone()) }
        }
    }
}

#[cfg(all(
    test,
    not(loom),
    not(miri),
    any(target_os = "linux", target_os = "android"),
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        all(target_arch = "arm", target_has_atomic = "64")
    )
))]
mod tests;

#[cfg(all(test, not(any(target_os = "linux", target_os = "android"))))]
mod unsupported_tests {
    use super::*;

    #[test]
    fn nothing_is_made_or_mapped_on_a_target_without_memfd() {
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        // The target's verdict comes before the arguments': a ring of zero bytes is `BadValue`.
        assert_eq!(create_topic(0).err(), Some(Error::Unsupported));
        assert_eq!(create_topic(MIN_RING).err(), Some(Error::Unsupported));
        assert_eq!(
            map_topic(file.as_fd(), MAX_RING).err(),
            Some(Error::Unsupported)
        );
        assert_eq!(ControlPage::create(0).err(), Some(Error::Unsupported));
        assert_eq!(
            ControlView::map(file.as_fd(), 0).err(),
            Some(Error::Unsupported)
        );
    }
}
