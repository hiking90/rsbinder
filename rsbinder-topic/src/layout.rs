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
//! | `0x38` | 8 | zero |
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
//! | `0x08` | 4 | `state`: bit 0 waiting, bit 1 closed, bits 2–31 a count of waits |
//! | `0x0c` | 4 | `pins`: how many pin words follow the header |
//! | `0x40` | 8 | `read`: the subscriber's read position, for statistics |
//! | `0x48` | 8 | `lost`: records it reported as lost, for statistics |
//! | `0x80` | 4 × pins | pin words (plan 15, loans) |
//!
//! The rest of the first `0x80` bytes is zero. `state` has a cache line of its own because the
//! publisher loads it at every commit, while the subscriber stores `read` at every record.
//!
//! # Making and mapping
//!
//! The four entry points — [`create_topic`], [`map_topic`], [`ControlPage::create`] and
//! [`ControlView::map`] — return `Unsupported` before they look at an argument where topics cannot
//! work: off Linux and Android, and where `shm::RoWord64::PRESENT` is `false` (no 8-byte load of a
//! read-only mapping is known not to write there). The two that make a memfd return it as well on a
//! kernel without `F_SEAL_FUTURE_WRITE`; there is no way to make either object without that seal.
//!
//! The two that map check the fd first (`shm::verify_received`: a sealed memfd with every page
//! present), then its size, then the words that say what it is. For topic memory the size gives
//! the ring's capacity, and the caller's `max_ring` bounds it, which bounds the address space a
//! peer can make this process map.
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
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rsbinder::fmq::shm::{
    self, Access, CreateOptions, Mapping, RoRun, RoWord32, RoWord64, Run, Seals,
};
use rsbinder::fmq::{Error, Result};

use crate::ring::{Loads, Stores};

/// The layout version this crate reads and writes: 0.1.
pub(crate) const VERSION: u32 = 1;

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

/// `state` bit 0: the subscriber is about to sleep, or sleeps, on `seq`.
pub(crate) const WAITING: u32 = 1;

/// `state` bit 1: the subscriber has dropped the subscription.
pub(crate) const CLOSED: u32 = 2;

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
    static NO_FUTURE_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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

    fn store_write(&self, position: u64) {
        self.word64(topic::WRITE).store(position, Ordering::Release);
    }

    fn store_last(&self, position: u64) {
        self.word64(topic::LAST).store(position, Ordering::Release);
    }

    fn store_half(&self, position: u64) {
        self.word64(topic::HALF).store(position, Ordering::Release);
    }

    fn bump_seq(&self) {
        self.word32(topic::SEQ).fetch_add(1, Ordering::SeqCst);
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

    /// The word a subscriber sleeps on.
    pub(crate) fn seq(&self) -> &RoWord32 {
        self.word32(topic::SEQ)
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

    fn fence(&self) {
        // What `RoRun::load_acquire` adds to `load`.
        std::sync::atomic::fence(Ordering::Acquire);
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
pub(crate) fn map_topic(fd: BorrowedFd<'_>, max_ring: usize) -> Result<Loaded<Mapping>> {
    check_target()?;
    let size = shm::verify_received(fd)?;
    let capacity = usize::try_from(size)
        .ok()
        .and_then(|size| size.checked_sub(topic::RING))
        .filter(|ring| ring % 8 == 0 && (MIN_RING..=MAX_RING.min(max_ring)).contains(ring))
        .ok_or(Error::BadValue("topic memory of a size outside the bounds"))?;
    let mapping = Mapping::map(fd, 0, size, Access::ReadOnly)?;
    // SAFETY: a page-aligned mapping of `size` bytes `verify_received` found present and sealed.
    let loaded = unsafe { Loaded::new(mapping.as_ptr(), capacity, mapping) };
    loaded.marked()?;
    Ok(loaded)
}

/// Bytes of a control page with `pins` pin words.
fn control_size(pins: u32) -> u64 {
    (control::PIN + 4 * u64::from(pins)).div_ceil(8) * 8
}

/// A subscriber's control page: the memfd it hands to the publisher, mapped read-write.
pub(crate) struct ControlPage {
    fd: OwnedFd,
    mapping: Mapping,
}

impl ControlPage {
    /// Make a control page with `pins` pin words. The errors are [`create_topic`]'s.
    pub(crate) fn create(pins: u32) -> Result<Self> {
        check_target()?;
        let size = control_size(pins);
        let options = CreateOptions::new("rsbinder-topic-control").populate(true);
        let fd = shm::create(size, &options)?;
        let mapping = Mapping::map(fd.as_fd(), 0, size, Access::ReadWrite)?;
        let page = Self { fd, mapping };
        page.word32(control::MAGIC)
            .store(CONTROL_MAGIC, Ordering::Relaxed);
        page.word32(control::VERSION)
            .store(VERSION, Ordering::Relaxed);
        page.word32(control::PINS).store(pins, Ordering::Relaxed);
        seal(page.fd.as_fd())?;
        Ok(page)
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

    /// The fd to send to the publisher.
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The [`WAITING`] and [`CLOSED`] bits and the count of waits. Changed a bit at a time.
    pub(crate) fn state(&self) -> &AtomicU32 {
        self.word32(control::STATE)
    }

    /// The subscriber's read position, which the publisher reads for statistics only.
    pub(crate) fn read(&self) -> &AtomicU64 {
        self.word64(control::READ)
    }

    /// Records the subscriber reported as lost, for statistics only.
    pub(crate) fn lost(&self) -> &AtomicU64 {
        self.word64(control::LOST)
    }
}

/// A subscriber's control page as the publisher maps it: read-only.
pub(crate) struct ControlView {
    mapping: Mapping,
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

    /// The subscriber's [`WAITING`] and [`CLOSED`] bits and its count of waits.
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
