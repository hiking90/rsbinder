// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The three memory objects of a subscription, byte for byte, carved into the pieces of
//! `rsbinder_fmq::shm`; nothing here makes a pointer into them. The slots' bytes are the one part
//! that may be read in place: they are carved `Lendable`, and the callers that take the slices, on
//! a `SAFETY` argument of their own, are in `sample.rs` and `pool.rs`. The other `unsafe` blocks
//! of the library (outside `cfg(test)`) pass that argument on for a `Trusted` subscriber
//! (`receive.rs`, `subscriber.rs`, `named.rs`) or register a file descriptor with the reactor
//! (`subscriber.rs`).
//!
//! All three are sealed memfds. Each side maps the object it made read-write and the peer's
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
//! [`Stored`] and [`Loaded`] are topic memory as its writer and as a reader reach it: the words
//! at the offsets above and the ring, each a piece carved from one mapping, which `shm` refuses
//! to carve with two pieces over one byte. Each piece accesses its bytes at one size, so the
//! views need no rule of their own about access sizes. A ring copy starts on a multiple of 8 and
//! is a multiple of 8 long because records are laid out so (`ring.rs`), not for the copy's sake.

// Shared memory is reached through `rsbinder_fmq::shm` pieces alone (plan 12-2).
#![forbid(unsafe_code)]

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rsbinder::fmq::shm::{
    self, Bytes, Carve, CreateOptions, Lendable, Mapping, RoBytes, RoCarve, RoLendable, RoMapping,
    RoWord32, RoWord64, Seals, Word, Words,
};
use rsbinder::fmq::{Descriptor, Errno, Error, Flavor, Grantor, Result};

use crate::pool::{GenerationLoads, Generations, LendError, LentSlot, Pin, Pins, Slots};
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

/// The errno a kernel answers for a seal it does not know.
const EINVAL: Errno = Errno::from_raw(rustix::io::Errno::INVAL.raw_os_error());

/// Add `seals` to `fd`: `shm::add_seals`, behind the switch above.
fn add_seals(fd: BorrowedFd<'_>, seals: Seals) -> Result<()> {
    #[cfg(test)]
    if seals.contains(Seals::FUTURE_WRITE) && NO_FUTURE_WRITE.get() {
        return Err(Error::Os(EINVAL));
    }
    shm::add_seals(fd, seals)
}

/// Seal a memfd this process has mapped: no resizing, no new writable mapping, no further seal.
fn seal(fd: BorrowedFd<'_>) -> Result<()> {
    let seals = Seals::GROW | Seals::SHRINK | Seals::FUTURE_WRITE | Seals::SEAL;
    match add_seals(fd, seals) {
        // The kernel applies none of a mask that holds a seal it does not know.
        Err(Error::Os(EINVAL)) => Err(Error::Unsupported),
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

/// Topic memory as its one writer reaches it: the words of the header and the ring, as pieces.
#[cfg_attr(any(test, feature = "fuzzing"), derive(Clone))]
pub(crate) struct Stored {
    region_end: Word<AtomicU64>,
    write: Word<AtomicU64>,
    magic: Word<AtomicU32>,
    version: Word<AtomicU32>,
    seq: Word<AtomicU32>,
    tail: Word<AtomicU64>,
    last: Word<AtomicU64>,
    half: Word<AtomicU64>,
    frontier: Word<AtomicU64>,
    ring: Bytes,
}

impl Stored {
    /// Topic memory with a ring of `capacity` bytes, carved from a writable mapping of it.
    fn carve(mut carve: Carve, capacity: usize) -> Result<Self> {
        assert!(capacity != 0 && capacity % 8 == 0, "a ring off the grid");
        Ok(Self {
            region_end: carve.word64(topic::REGION_END)?,
            write: carve.word64(topic::WRITE)?,
            magic: carve.word32(topic::MAGIC)?,
            version: carve.word32(topic::VERSION)?,
            seq: carve.word32(topic::SEQ)?,
            tail: carve.word64(topic::TAIL)?,
            last: carve.word64(topic::LAST)?,
            half: carve.word64(topic::HALF)?,
            frontier: carve.word64(topic::FRONTIER)?,
            ring: carve.bytes(topic::RING, capacity)?,
        })
    }

    /// Write the two words that say what this memory is. Before any peer has the fd.
    fn mark(&self) {
        self.magic.store(TOPIC_MAGIC, Ordering::Relaxed);
        self.version.store(VERSION, Ordering::Relaxed);
    }
}

impl Stores for Stored {
    fn capacity(&self) -> u64 {
        self.ring.len() as u64
    }

    fn store_tail(&self, position: u64) {
        self.tail.store(position, Ordering::Release);
    }

    fn store_region_end(&self, position: u64) {
        self.region_end.store(position, Ordering::Release);
    }

    fn store_frontier(&self, position: u64) {
        self.frontier.store(position, Ordering::Release);
    }

    fn store_write(&self, position: u64) {
        self.write.store(position, Ordering::Release);
    }

    fn store_last(&self, position: u64) {
        self.last.store(position, Ordering::Release);
    }

    fn store_half(&self, position: u64) {
        self.half.store(position, Ordering::Release);
    }

    fn bump_seq(&self) -> u32 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        seq.wrapping_add(1)
    }

    fn wake_seq(&self) {
        // Fails only where the system call does not exist; there no subscriber can sleep either.
        let _ = shm::futex_wake(&self.seq, std::num::NonZeroU32::MAX);
    }

    fn store_ring(&self, position: u64, src: &[u8]) {
        let (offset, first) = split(self.ring.len(), position, src.len());
        let (head, wrapped) = src.split_at(first);
        self.ring.store_release(offset, head);
        self.ring.store_release(0, wrapped);
    }
}

/// Topic memory as a reader reaches it: loads, and nothing that stores.
pub(crate) struct Loaded {
    write: Word<RoWord64>,
    magic: Word<RoWord32>,
    version: Word<RoWord32>,
    seq: Word<RoWord32>,
    tail: Word<RoWord64>,
    last: Word<RoWord64>,
    half: Word<RoWord64>,
    frontier: Word<RoWord64>,
    ring: RoBytes,
}

impl Loaded {
    /// Topic memory with a ring of `capacity` bytes, carved from a read-only mapping of it.
    fn carve(mut carve: RoCarve, capacity: usize) -> Result<Self> {
        assert!(capacity != 0 && capacity % 8 == 0, "a ring off the grid");
        Ok(Self {
            write: carve.word64(topic::WRITE)?,
            magic: carve.word32(topic::MAGIC)?,
            version: carve.word32(topic::VERSION)?,
            seq: carve.word32(topic::SEQ)?,
            tail: carve.word64(topic::TAIL)?,
            last: carve.word64(topic::LAST)?,
            half: carve.word64(topic::HALF)?,
            frontier: carve.word64(topic::FRONTIER)?,
            ring: carve.bytes(topic::RING, capacity)?,
        })
    }

    /// The word a subscriber sleeps on, as a handle that keeps the memory alive on its own.
    pub(crate) fn seq(&self) -> Seq {
        Seq {
            word: self.seq.clone(),
        }
    }

    /// Whether the two words that say what this memory is hold what this crate writes there.
    fn marked(&self) -> Result<()> {
        if self.magic.load() != TOPIC_MAGIC {
            return Err(Error::BadValue("received memfd is not topic memory"));
        }
        if self.version.load() != VERSION {
            return Err(Error::BadValue("topic memory of another layout version"));
        }
        Ok(())
    }
}

impl Loads for Loaded {
    fn capacity(&self) -> u64 {
        self.ring.len() as u64
    }

    fn tail(&self) -> u64 {
        self.tail.load_acquire()
    }

    fn frontier(&self) -> u64 {
        self.frontier.load_acquire()
    }

    fn write(&self) -> u64 {
        self.write.load_acquire()
    }

    fn last(&self) -> u64 {
        self.last.load_acquire()
    }

    fn half(&self) -> u64 {
        self.half.load_acquire()
    }

    fn load_ring(&self, position: u64, dst: &mut [u8]) {
        let (offset, first) = split(self.ring.len(), position, dst.len());
        let (head, wrapped) = dst.split_at_mut(first);
        self.ring.load(offset, head);
        self.ring.load(0, wrapped);
    }

    fn load_ring_extend(&self, position: u64, len: usize, dst: &mut Vec<u8>) {
        let (offset, first) = split(self.ring.len(), position, len);
        self.ring.load_extend(offset, first, dst);
        self.ring.load_extend(0, len - first, dst);
    }

    fn fence(&self) {
        // What `RoBytes::load_acquire` adds to `load`.
        std::sync::atomic::fence(Ordering::Acquire);
    }
}

/// Topic memory's `seq`, apart from the reader: what a wait on another thread sleeps on and wakes.
pub(crate) struct Seq {
    word: Word<RoWord32>,
}

impl Seq {
    pub(crate) fn word(&self) -> &RoWord32 {
        &self.word
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
pub(crate) fn create_topic(ring_bytes: usize) -> Result<(OwnedFd, Stored)> {
    check_target()?;
    let capacity = ring_capacity(ring_bytes)?;
    let size = (topic::RING + capacity) as u64;
    let options = CreateOptions::new("rsbinder-topic").populate(true);
    let fd = shm::create(size, &options)?;
    let mapping = Mapping::map(fd.as_fd(), 0, size)?;
    let stored = Stored::carve(mapping.carve(), capacity)?;
    stored.mark();
    // After the mapping: `FUTURE_WRITE` refuses every writable mapping made from here on.
    seal(fd.as_fd())?;
    Ok((fd, stored))
}

/// Map topic memory a publisher sent, read-only, once the fd, its size and its marks pass.
pub(crate) fn map_topic(fd: BorrowedFd<'_>, max_ring: usize) -> Result<Loaded> {
    check_target()?;
    let size = shm::verify_received(fd)?;
    let capacity = usize::try_from(size)
        .ok()
        .and_then(|size| size.checked_sub(topic::RING))
        .filter(|ring| ring % 8 == 0 && (MIN_RING..=MAX_RING.min(max_ring)).contains(ring))
        .ok_or(Error::BadValue("topic memory of a size outside the bounds"))?;
    // The pieces share the mapping, so a `Seq` taken from the view outlives a dropped reader.
    let loaded = Loaded::carve(RoMapping::map(fd, 0, size)?.carve(), capacity)?;
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
    state: Word<AtomicU32>,
    read: Word<AtomicU64>,
    lost: Word<AtomicU64>,
    pins: Words<AtomicU32>,
}

impl ControlPage {
    /// Make a control page with `pins` pin words: the fd to hand to the publisher, and the page.
    /// The errors are [`create_topic`]'s.
    pub(crate) fn create(pins: u32) -> Result<(OwnedFd, Self)> {
        check_target()?;
        let size = control_size(pins);
        let options = CreateOptions::new("rsbinder-topic-control").populate(true);
        let fd = shm::create(size, &options)?;
        let mut carve = Mapping::map(fd.as_fd(), 0, size)?.carve();
        carve
            .word32(control::MAGIC)?
            .store(CONTROL_MAGIC, Ordering::Relaxed);
        carve
            .word32(control::VERSION)?
            .store(VERSION, Ordering::Relaxed);
        carve.word32(control::PINS)?.store(pins, Ordering::Relaxed);
        let page = Self {
            state: carve.word32(control::STATE)?,
            read: carve.word64(control::READ)?,
            lost: carve.word64(control::LOST)?,
            pins: carve.words32(control::PIN as usize, pins as usize)?,
        };
        seal(fd.as_fd())?;
        Ok((fd, page))
    }

    /// The [`WAITING`] and [`CLOSED`] bits and the `seq` a wait sleeps on. Changed by RMWs alone.
    pub(crate) fn state(&self) -> &AtomicU32 {
        &self.state
    }

    /// The subscriber's read position, which the publisher reads for statistics, the pool's lends
    /// and, with backpressure, its waits.
    pub(crate) fn read(&self) -> &AtomicU64 {
        &self.read
    }

    /// Records the subscriber reported as lost, for statistics only.
    pub(crate) fn lost(&self) -> &AtomicU64 {
        &self.lost
    }

    /// Pin word `nth`: 0, or the number of the slot this subscriber reads plus one.
    fn pin(&self, nth: u32) -> &AtomicU32 {
        assert!(
            (nth as usize) < self.pins.len(),
            "a pin word the page does not have"
        );
        self.pins.get(nth as usize)
    }
}

impl Pin for ControlPage {
    fn store_pin(&self, value: u32, order: Ordering) {
        self.pin(0).store(value, order);
    }
}

/// A subscriber's control page as the publisher maps it: read-only.
pub(crate) struct ControlView {
    state: Word<RoWord32>,
    read: Word<RoWord64>,
    lost: Word<RoWord64>,
    pins: Words<RoWord32>,
}

impl ControlView {
    /// Map a control page a subscriber sent, after checking the fd, its size and what it holds.
    pub(crate) fn map(fd: BorrowedFd<'_>, pins: u32) -> Result<Self> {
        check_target()?;
        let size = shm::verify_received(fd)?;
        if size != control_size(pins) {
            return Err(Error::BadValue("control page of another size"));
        }
        let mut carve = RoMapping::map(fd, 0, size)?.carve();
        if carve.word32(control::MAGIC)?.load() != CONTROL_MAGIC {
            return Err(Error::BadValue("received memfd is not a control page"));
        }
        if carve.word32(control::VERSION)?.load() != VERSION {
            return Err(Error::BadValue("control page of another layout version"));
        }
        if carve.word32(control::PINS)?.load() != pins {
            return Err(Error::BadValue("control page with another pin count"));
        }
        Ok(Self {
            state: carve.word32(control::STATE)?,
            read: carve.word64(control::READ)?,
            lost: carve.word64(control::LOST)?,
            pins: carve.words32(control::PIN as usize, pins as usize)?,
        })
    }

    /// The subscriber's [`WAITING`] and [`CLOSED`] bits and the `seq` its wait sleeps on.
    pub(crate) fn state(&self) -> &RoWord32 {
        &self.state
    }

    /// What the subscriber says its read position is.
    pub(crate) fn read(&self) -> u64 {
        self.read.load_acquire()
    }

    /// How many records the subscriber says it lost.
    pub(crate) fn lost(&self) -> u64 {
        self.lost.load_acquire()
    }

    /// The pin words the page has.
    pub(crate) fn pins(&self) -> u32 {
        self.pins.len() as u32
    }

    /// Pin word `nth`, a `Relaxed` load: the caller fences (`pool.rs`).
    pub(crate) fn pin(&self, nth: u32) -> u32 {
        assert!(
            (nth as usize) < self.pins.len(),
            "a pin word the page does not have"
        );
        self.pins.get(nth as usize).load()
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
    generations: Words<AtomicU64>,
    /// Every slot; each [`LentSlot`] holds a handle on it, so that a loan outlives no mapping.
    area: Lendable,
    slots: u32,
    slot_bytes: u32,
}

impl StoredPool {
    fn generation(&self, slot: u32) -> &AtomicU64 {
        assert!(slot < self.slots, "a slot outside the pool");
        self.generations.get(slot as usize)
    }

    pub(crate) fn slot_bytes(&self) -> u32 {
        self.slot_bytes
    }

    /// The address the mapping starts at, for tests that look it up in `/proc/self/smaps`.
    #[cfg(test)]
    fn mapping_start(&self) -> usize {
        self.area.as_ptr() as usize - slot_area(self.slots) as usize
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
        let at = slot as usize * self.slot_bytes as usize;
        Ok(LentSlot::new(self.area.clone(), slot, at..at + len))
    }
}

impl Generations for StoredPool {
    fn store_generation(&self, slot: u32, value: u64, order: Ordering) {
        self.generation(slot).store(value, order);
    }
}

/// Pool memory as a subscriber reaches it: loads only.
pub(crate) struct LoadedPool {
    generations: Words<RoWord64>,
    /// Every slot, loaded through atomics or, under a pin, read in place (`pool.rs`).
    area: RoLendable,
    slots: u32,
    slot_bytes: u32,
}

impl LoadedPool {
    pub(crate) fn slot_bytes(&self) -> u32 {
        self.slot_bytes
    }

    /// Fill `dst` from the start of `slot`, with `Relaxed` loads: the caller pinned it.
    pub(crate) fn load_slot(&self, slot: u32, dst: &mut [u8]) {
        let range = self.slot_range(slot, dst.len());
        self.area.load(range.start, dst);
    }

    /// [`load_slot`](Self::load_slot) of `len` bytes appended to `dst`, not zeroed first.
    pub(crate) fn load_slot_extend(&self, slot: u32, len: usize, dst: &mut Vec<u8>) {
        let range = self.slot_range(slot, len);
        self.area.load_extend(range.start, len, dst);
    }

    /// The bytes of [`area`](Self::area) the first `len` bytes of `slot` are.
    pub(crate) fn slot_range(&self, slot: u32, len: usize) -> std::ops::Range<usize> {
        assert!(slot < self.slots && len <= self.slot_bytes as usize);
        let at = slot as usize * self.slot_bytes as usize;
        at..at + len
    }

    /// Every slot of the pool, in slot order.
    pub(crate) fn area(&self) -> &RoLendable {
        &self.area
    }

    /// The addresses the slots cover, for tests that a sample lies in them.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn addresses(&self) -> std::ops::Range<usize> {
        let start = self.area.as_ptr() as usize;
        start..start + self.area.len()
    }
}

impl GenerationLoads for LoadedPool {
    fn slots(&self) -> u32 {
        self.slots
    }

    fn load_generation(&self, slot: u32) -> u64 {
        assert!(slot < self.slots, "a slot outside the pool");
        self.generations.get(slot as usize).load()
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
    let mapping = Mapping::map(fd.as_fd(), 0, size)?;
    if huge {
        mapping.populate_huge_pages();
    }
    let mut carve = mapping.carve();
    carve
        .word32(pool::MAGIC)?
        .store(POOL_MAGIC, Ordering::Relaxed);
    carve
        .word32(pool::VERSION)?
        .store(VERSION, Ordering::Relaxed);
    carve.word32(pool::SLOTS)?.store(slots, Ordering::Relaxed);
    carve
        .word32(pool::SLOT_BYTES)?
        .store(slot_bytes, Ordering::Relaxed);
    let stored = StoredPool {
        generations: carve.words64(pool::GENERATIONS, slots as usize)?,
        area: carve.lendable(
            slot_area(slots) as usize,
            slots as usize * slot_bytes as usize,
        )?,
        slots,
        slot_bytes,
    };
    seal(fd.as_fd())?;
    Ok((fd, stored))
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
    let mut carve = RoMapping::map(fd, 0, size)?.carve();
    if carve.word32(pool::MAGIC)?.load() != POOL_MAGIC {
        return Err(Error::BadValue("received memfd is not pool memory"));
    }
    if carve.word32(pool::VERSION)?.load() != VERSION {
        return Err(Error::BadValue("pool memory of another layout version"));
    }
    if carve.word32(pool::SLOTS)?.load() != slots
        || carve.word32(pool::SLOT_BYTES)?.load() != slot_bytes
    {
        return Err(Error::BadValue(
            "pool memory with other slots than its endpoint",
        ));
    }
    Ok(LoadedPool {
        generations: carve.words64(pool::GENERATIONS, slots as usize)?,
        area: carve.lendable(
            slot_area(slots) as usize,
            slots as usize * slot_bytes as usize,
        )?,
        slots,
        slot_bytes,
    })
}

#[cfg(any(test, feature = "fuzzing"))]
pub(crate) use heap::Heap;

#[cfg(any(test, feature = "fuzzing"))]
mod heap {
    use super::*;

    /// Topic memory on the heap, for what cannot map a memfd: Miri, and a fuzzer's input.
    pub(crate) struct Heap {
        stored: Stored,
    }

    impl Heap {
        /// Zeroed topic memory with a ring of `capacity` bytes, marked as `create_topic` marks it.
        #[cfg(test)]
        pub(crate) fn new(capacity: usize) -> Self {
            let heap = Self::zeroed(capacity).expect("heap memory of a valid ring");
            heap.stored.mark();
            heap
        }

        /// Topic memory holding `image`: 64 bytes of header, then the ring, cut to a multiple of 8.
        pub(crate) fn from_image(image: &[u8]) -> Option<Self> {
            let capacity = image.len().checked_sub(topic::RING)? / 8 * 8;
            let heap = Self::zeroed(capacity).ok()?;
            let word32 = |at: usize| u32::from_ne_bytes(image[at..at + 4].try_into().expect("4"));
            let word64 = |at: usize| u64::from_ne_bytes(image[at..at + 8].try_into().expect("8"));
            // Each byte through its own piece: Miri's weak-memory emulation takes one size per byte.
            let s = &heap.stored;
            for (word, at) in [
                (&s.region_end, topic::REGION_END),
                (&s.write, topic::WRITE),
                (&s.tail, topic::TAIL),
                (&s.last, topic::LAST),
                (&s.half, topic::HALF),
                (&s.frontier, topic::FRONTIER),
            ] {
                word.store(word64(at), Ordering::Relaxed);
            }
            for (word, at) in [
                (&s.magic, topic::MAGIC),
                (&s.version, topic::VERSION),
                (&s.seq, topic::SEQ),
            ] {
                word.store(word32(at), Ordering::Relaxed);
            }
            s.ring.store(0, &image[topic::RING..topic::RING + capacity]);
            Some(heap)
        }

        /// `BadValue` for a ring of zero bytes, which `Stored::carve` refuses by panicking.
        fn zeroed(capacity: usize) -> Result<Self> {
            if capacity == 0 {
                return Err(Error::BadValue("a ring of zero bytes"));
            }
            let carve = Mapping::heap(topic::RING + capacity)?.carve();
            Ok(Self {
                stored: Stored::carve(carve, capacity)?,
            })
        }

        /// A writer's view. Use one at a time, as there is one publisher.
        #[cfg(test)]
        pub(crate) fn stored(&self) -> Stored {
            self.stored.clone()
        }

        /// A reader's view: the writer's pieces, read-only.
        pub(crate) fn loaded(&self) -> Loaded {
            let s = &self.stored;
            let ro = |word: &Word<AtomicU64>| word.as_ro().expect("a target with `RoWord64`");
            Loaded {
                write: ro(&s.write),
                magic: s.magic.as_ro(),
                version: s.version.as_ro(),
                seq: s.seq.as_ro(),
                tail: ro(&s.tail),
                last: ro(&s.last),
                half: ro(&s.half),
                frontier: ro(&s.frontier),
                ring: s.ring.as_ro(),
            }
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
