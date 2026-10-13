// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::cell::Cell;
use std::marker::PhantomData;
use std::mem::size_of;
use std::os::fd::{AsFd, OwnedFd};
use std::ptr::NonNull;
#[cfg(target_has_atomic = "64")]
pub(crate) use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::descriptor::{
    default_layout, recheck_ashmem_sizes, validate, AttachPolicy, Descriptor, Flavor, Grantor,
};
use crate::error::{Error, Result};
use crate::event_flag::EventFlag;
use crate::ring;
use crate::shm::{self, Access, CreateOptions, Deadline, Mapping, Run, Seals};

/// A type that can be an element of a [`MessageQueue`].
///
/// The ring is memory another process writes, so an element is read from it
/// as raw bytes and must be meaningful whatever those bytes are.
///
/// # Safety
///
/// Every bit pattern of `size_of::<Self>()` bytes must be a valid `Self`;
/// `Self` must have no padding bytes (they would be copied out of and back
/// into shared memory as data) and an alignment of at most 8. The
/// primitive integers and floats satisfy this; `bool`, `char` and enums do
/// not. A `#[repr(C)]` struct of such fields with no padding does, which is
/// how AIDL `@FixedSize` parcelables become elements.
pub unsafe trait Element: Copy + 'static {}

macro_rules! elements {
    ($($t:ty),* $(,)?) => { $(
        // SAFETY: primitive numbers: no padding, every bit pattern valid, align <= 8.
        unsafe impl Element for $t {}
    )* };
}
elements!(u8, i8, u16, i16, u32, i32, u64, i64, f32, f64);

/// Stands in for std's `AtomicU64` on a target without one. Uninhabited: no `Counter` is made.
#[cfg(not(target_has_atomic = "64"))]
pub(crate) enum AtomicU64 {}

#[cfg(not(target_has_atomic = "64"))]
impl AtomicU64 {
    fn load(&self, _: Ordering) -> u64 {
        match *self {}
    }

    pub(crate) fn store(&self, _: u64, _: Ordering) {
        match *self {}
    }
}

/// `Unsupported` where no queue can exist: off Linux and Android, or without 64-bit atomics.
pub(crate) fn check_target() -> Result<()> {
    let runs_a_queue = cfg!(all(
        any(target_os = "linux", target_os = "android"),
        target_has_atomic = "64"
    ));
    if runs_a_queue {
        Ok(())
    } else {
        Err(Error::Unsupported)
    }
}

pub(crate) struct Counter {
    _mapping: Mapping,
    ptr: NonNull<AtomicU64>,
}

impl Counter {
    pub(crate) fn map(desc: &Descriptor, g: Grantor) -> Result<Self> {
        // No `Counter` exists where `AtomicU64` is the uninhabited stand-in; `get` relies on it.
        check_target()?;
        // Only the 8-byte word: a peer-sized extent would be the peer's choice of address space.
        let mapping = Mapping::map(
            desc.fds[g.fd_index as usize].as_fd(),
            u64::from(g.offset),
            8,
            Access::ReadWrite,
        )?;
        let ptr = mapping.as_ptr().cast::<AtomicU64>();
        debug_assert_eq!(ptr.as_ptr() as usize % std::mem::align_of::<AtomicU64>(), 0);
        Ok(Self {
            _mapping: mapping,
            ptr,
        })
    }

    fn get(&self) -> &AtomicU64 {
        // SAFETY: in `_mapping`, 8-aligned (grantor offsets are 8-aligned), atomic-only.
        unsafe { self.ptr.as_ref() }
    }

    /// The word's address, valid while this `Counter` lives.
    pub(crate) fn address(&self) -> NonNull<AtomicU64> {
        self.ptr
    }
}

pub(crate) struct Ring {
    _mapping: Mapping,
    pub(crate) base: NonNull<u8>,
    pub(crate) bytes: usize,
}

impl Ring {
    /// Map the data grantor `g` of a descriptor that `validate` passed or this crate laid out.
    pub(crate) fn map(desc: &Descriptor, g: Grantor, access: Access) -> Result<Self> {
        let mapping = Mapping::map(
            desc.fds[g.fd_index as usize].as_fd(),
            u64::from(g.offset),
            g.extent,
            access,
        )?;
        Ok(Self {
            base: mapping.as_ptr(),
            // ≤ i32::MAX: checked by `validate`, and by `ring_bytes` for a new queue.
            bytes: g.extent as usize,
            _mapping: mapping,
        })
    }
}

/// `capacity * size_of::<T>()`, within what libfmq maps and a 32-bit `usize` holds.
pub(crate) fn ring_bytes<T: Element>(capacity: usize) -> Result<u64> {
    let quantum = size_of::<T>();
    if quantum == 0 {
        return Err(Error::BadValue("zero-sized element"));
    }
    if capacity == 0 {
        return Err(Error::BadValue("capacity of zero"));
    }
    (capacity as u64)
        .checked_mul(quantum as u64)
        .filter(|b| *b <= i32::MAX as u64 - rustix::param::page_size() as u64)
        .ok_or(Error::BadValue("queue too large"))
}

/// A synchronized-flavor Fast Message Queue: one writer, one reader, a ring
/// of `capacity` elements in shared memory, and two 64-bit counters that
/// never wrap (a position is `counter % ring bytes`).
///
/// This is libfmq's `MessageQueue<T, kSynchronizedReadWrite>` — the same
/// memory layout, counter discipline and futex protocol, so one end can be
/// C++ using `libfmq` and the other this type. See the [crate
/// docs](crate) for the compatibility scope.
///
/// # One handle, one side
///
/// A `MessageQueue` is `Send` but not `Sync`: every operation that moves a
/// counter or touches the ring takes `&mut self`. The flavor allows one
/// writer and one reader, and keeping to that is the callers' protocol, as
/// it is with libfmq: two handles on the same queue in one process (one
/// from [`create`](Self::create), one from [`attach`](Self::attach)) are
/// fine as long as one only writes and the other only reads.
///
/// Nothing detects a second writer or a second reader, whether it is
/// another handle in this process or the peer. It does to this queue what
/// it does to libfmq's: the counters fail their check
/// ([`Corrupted`](Error::Corrupted)) or a reader copies out wrong elements.
/// It is not undefined behavior, since every access to the shared memory is
/// atomic (see "Soundness" on [`Regions`]).
///
/// A descriptor of the unsynchronized flavor is refused here: that flavor
/// has any number of readers, each with a read counter of its own, and a
/// writer that overwrites what they have not read. It is
/// [`UnsyncWriter`](crate::UnsyncWriter) and
/// [`UnsyncReader`](crate::UnsyncReader).
///
/// ```compile_fail,E0277
/// fn is_sync<T: Sync>() {}
/// is_sync::<rsbinder_fmq::MessageQueue<u8>>();
/// ```
///
/// # What the reader sees
///
/// A write is visible only after [`commit_write`](Self::commit_write) moves
/// the write counter (a `Release` store after the data copy), and the
/// reader loads the counter with `Acquire`; so the reader never observes a
/// partially copied element. The reverse holds for the space a read frees.
///
/// # The ring is the peer's memory too
///
/// Nothing here hands out a `&[T]` into the ring. [`Regions`] copies
/// elements in and out between the counter accesses, and a reader interprets
/// what it copied out, never the ring itself — a peer may rewrite the
/// bytes at any moment, and a hostile one will.
///
/// # Attach before the first write
///
/// libfmq's constructor from a descriptor resets both counters to zero by
/// default (`resetPointers = true`). A queue this type creates has zero
/// counters until its first commit, so a libfmq peer that attaches *before*
/// any write sees no difference; one that attaches after would discard
/// what was written. [`attach`](Self::attach) never writes the counters.
pub struct MessageQueue<T: Element> {
    read: Counter,
    write: Counter,
    ring: Ring,
    flag: Option<EventFlag>,
    desc: Descriptor,
    capacity: usize,
    /// (read, write) as the cached writer last saw them; `None` until a cached write loads them.
    writer_view: Option<(u64, u64)>,
    _not_sync: PhantomData<Cell<()>>,
    _element: PhantomData<T>,
}

// SAFETY: pointers into owned `Mapping`s (`Send`), reached only via atomics (`Counter`, `Run`).
unsafe impl<T: Element> Send for MessageQueue<T> {}

impl<T: Element> MessageQueue<T> {
    /// Allocate a queue of `capacity` elements in a new memfd, with an
    /// EventFlag word when `event_flag` is set.
    ///
    /// Every page is allocated up front (`fallocate`), so the memory is
    /// charged to this process rather than to whichever peer first touches
    /// a page; the fd is sealed `F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL`,
    /// so no holder can resize it or add a seal that blocks this process's
    /// own writes. The layout is libfmq's single-fd layout
    /// (`AidlMQDescriptorShimBase.h`): read counter at 0, write counter at
    /// 8, data at 16, EventFlag word at the next multiple of 8.
    ///
    /// `capacity` must be at least 1, and `capacity * size_of::<T>()` at
    /// most `i32::MAX` less the page size: libfmq's `mapGrantorDescr`
    /// refuses a larger extent, and the EventFlag word's offset, an AIDL
    /// `int`, lands past the ring. [`attach`](Self::attach) caps `extent` (a `long`
    /// in AIDL) at `i32::MAX` too, so that a ring fits a 32-bit `usize`.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] comes first: on a target that cannot run a
    /// queue — one that is not Linux or Android, or has no 64-bit atomics —
    /// it is returned whatever the arguments are, before any system call.
    /// On the other targets: [`Error::BadValue`] for a zero-sized `T` or a
    /// `capacity` outside the bounds above, then [`Error::Os`] when
    /// creating, allocating, sealing or mapping the memfd fails.
    pub fn create(capacity: usize, event_flag: bool) -> Result<Self> {
        check_target()?;
        let data_bytes = ring_bytes::<T>(capacity)?;
        let (grantors, total) = default_layout(data_bytes, event_flag);
        let fd = allocate_shared(total)?;
        shm::add_seals(fd.as_fd(), Seals::GROW | Seals::SHRINK | Seals::SEAL)?;
        let desc = Descriptor {
            fds: vec![fd],
            ints: Vec::new(),
            grantors,
            quantum: size_of::<T>() as u32,
            flavor: Flavor::SynchronizedReadWrite,
        };
        let policy = AttachPolicy {
            max_capacity: capacity,
            require_seal: true,
            require_event_flag: event_flag,
        };
        let queue = Self::open(desc, &policy)?;
        queue.read.get().store(0, Ordering::Release);
        queue.write.get().store(0, Ordering::Release);
        if let Some(flag) = &queue.flag {
            flag.reset();
        }
        Ok(queue)
    }

    /// Map a queue a peer described, after checking the descriptor against
    /// `policy` and libfmq's own rules (see [`AttachPolicy`]). The fds are
    /// duplicated; `desc` stays usable. A synchronized single-fd queue that
    /// libfmq made is within the `i32::MAX` caps (its shim bounds offsets
    /// there too); a two-fd one may exceed the `extent` cap, and an
    /// unsynchronized one is refused
    /// ([`UnsyncReader::attach`](crate::UnsyncReader::attach) takes it).
    ///
    /// On a 32-bit target the ring has one more bound. The bytes mapped for
    /// it start at the page boundary below the data grantor's `offset`, and
    /// [`Mapping::map`] refuses more than `isize::MAX` of them: `extent`
    /// plus `offset` modulo the page size is at most that, or the descriptor
    /// is `BadValue`. On 32-bit Android bionic's `mmap64` also refuses
    /// (`ENOMEM`) a mapping whose page-rounded length is more than
    /// `isize::MAX`.
    ///
    /// A descriptor in which a counter, the data or the EventFlag grantor
    /// names a hugetlbfs fd (a memfd created `MFD_HUGETLB`) is `BadValue` as
    /// well: [`Mapping::map`] refuses a file the kernel reports as
    /// hugetlbfs, whose mapping it could not release. Where the kernel does
    /// not answer that query (see [`Mapping`]) the fd is mapped like any
    /// other, and a mapping of a hugetlbfs file stays until the process
    /// exits.
    ///
    /// The counters are left as they are.
    ///
    /// Grantors whose bytes overlap are refused: two at the same fd index,
    /// or on two fds that are not ashmem and name the same file (equal
    /// `st_dev` and `st_ino`, as a `dup` of one fd does). This is a
    /// consistency check on the descriptor, not a safety boundary. Ashmem
    /// fds are compared by index only (every ashmem fd has the device
    /// node's inode), and memory reached by any other path is not compared
    /// at all; an overlap that passes makes the counters fail their check
    /// or the elements wrong, not undefined behavior (see "One handle, one
    /// side").
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] comes first: on a target that cannot run a
    /// queue — one that is not Linux or Android, or has no 64-bit atomics —
    /// it is returned whatever `desc` and `policy` hold, before any system
    /// call. On the other targets: [`Error::Os`] when an fd cannot be
    /// duplicated, [`Error::BadValue`] for a descriptor the checks above
    /// refuse, and [`Error::Os`] when a system call those checks or the
    /// mappings make fails.
    pub fn attach(desc: &Descriptor, policy: &AttachPolicy) -> Result<Self> {
        check_target()?;
        Self::open(desc.try_clone()?, policy)
    }

    fn open(desc: Descriptor, policy: &AttachPolicy) -> Result<Self> {
        // The target's verdict comes before the descriptor's.
        check_target()?;
        let geo = validate(&desc, size_of::<T>(), policy, Flavor::SynchronizedReadWrite)?;
        let read = Counter::map(&desc, geo.read)?;
        let write = Counter::map(&desc, geo.write)?;
        let ring = Ring::map(&desc, geo.data, Access::ReadWrite)?;
        let flag = match geo.event_flag {
            // The 4-byte word only, as for the counters.
            Some(g) => Some(EventFlag::from_mapping(Mapping::map(
                desc.fds[g.fd_index as usize].as_fd(),
                u64::from(g.offset),
                4,
                Access::ReadWrite,
            )?)),
            None => None,
        };
        recheck_ashmem_sizes(&desc, &geo)?;
        Ok(Self {
            read,
            write,
            ring,
            flag,
            desc,
            capacity: geo.capacity,
            writer_view: None,
            _not_sync: PhantomData,
            _element: PhantomData,
        })
    }

    /// The descriptor a peer attaches with; its fds are duplicates.
    pub fn descriptor(&self) -> Result<Descriptor> {
        self.desc.try_clone()
    }

    /// Elements the ring holds.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes per element, `size_of::<T>()`.
    pub fn quantum(&self) -> usize {
        size_of::<T>()
    }

    /// A handle on the EventFlag word, when the queue has one. It outlives
    /// the queue.
    pub fn event_flag(&self) -> Option<EventFlag> {
        self.flag.clone()
    }

    /// Both counters, checked against a hostile peer: order, in-flight ≤ ring, no wrap, alignment.
    fn positions(&self) -> Result<(u64, u64)> {
        let write = self.write.get().load(Ordering::Acquire);
        let read = self.read.get().load(Ordering::Acquire);
        if read > write {
            return Err(Error::Corrupted("read counter ahead of the write counter"));
        }
        if write - read > self.ring.bytes as u64 {
            return Err(Error::Corrupted("more bytes in flight than the ring holds"));
        }
        // A commit adds at most `bytes`; a counter this high would wrap.
        if write > u64::MAX - self.ring.bytes as u64 {
            return Err(Error::Corrupted("counter too close to wrapping"));
        }
        let q = size_of::<T>() as u64;
        if read % q != 0 || write % q != 0 {
            return Err(Error::Corrupted(
                "counter not a multiple of the element size",
            ));
        }
        Ok((read, write))
    }

    /// Elements that can be read now. `Err` when the counters fail their
    /// invariant.
    pub fn available_to_read(&self) -> Result<usize> {
        let (read, write) = self.positions()?;
        Ok(((write - read) / size_of::<T>() as u64) as usize)
    }

    /// Elements that can be written now. `Err` when the counters fail their
    /// invariant.
    pub fn available_to_write(&self) -> Result<usize> {
        Ok(self.capacity - self.available_to_read()?)
    }

    fn regions_at(&self, position: u64, n: usize) -> Regions<'_, T> {
        debug_assert!(n <= self.capacity);
        // SAFETY: the ring is mapped read-write while `self` is borrowed; callers check `n`.
        unsafe { Regions::over(self.ring.base, self.ring.bytes, position, n, false) }
    }

    /// Reserve `n` elements to write. `None` when fewer are free (or `n`
    /// exceeds the capacity); the reservation is made real by
    /// [`commit_write`](Self::commit_write). The two regions of the result
    /// are the part before the ring's end and the part after its wrap.
    pub fn begin_write(&mut self, n: usize) -> Result<Option<Regions<'_, T>>> {
        if n > self.capacity {
            return Ok(None);
        }
        let (read, write) = self.positions()?;
        let free = self.capacity - ((write - read) / size_of::<T>() as u64) as usize;
        if free < n {
            return Ok(None);
        }
        Ok(Some(self.regions_at(write, n)))
    }

    /// Publish `n` elements written into the regions
    /// [`begin_write`](Self::begin_write) returned. `n` must not exceed the
    /// space free at this moment.
    ///
    /// Drops the view [`begin_write_cached`](Self::begin_write_cached)
    /// keeps, so a cached write after this one loads both counters again.
    pub fn commit_write(&mut self, n: usize) -> Result<()> {
        self.writer_view = None;
        let (read, write) = self.positions()?;
        let q = size_of::<T>() as u64;
        let free = self.capacity as u64 - (write - read) / q;
        if n as u64 > free {
            return Err(Error::BadValue("commit exceeds the space that was free"));
        }
        self.write
            .get()
            .store(write + n as u64 * q, Ordering::Release);
        Ok(())
    }

    /// [`begin_write`](Self::begin_write) for a writer that relies on being
    /// the queue's only one, keeping at most `limit` elements in flight once
    /// these `n` are in (`limit` is capped at the capacity). `None` when they
    /// do not fit.
    ///
    /// The write counter is this handle's own: it is loaded on the first
    /// call, then taken from what
    /// [`commit_write_cached`](Self::commit_write_cached) stored. The read
    /// counter is loaded again only when the last value seen leaves no room
    /// for `n`. A write that fits by that value loads no counter at all,
    /// where `begin_write` and `commit_write` load both counters each, and
    /// does not load the counter the reader writes. The commit's store still
    /// writes the reader's cache line: libfmq's layout puts both counters in
    /// the first 16 bytes.
    ///
    /// A reloaded read counter is checked against the view: it may not move
    /// backwards, pass the write counter, put more than the ring in flight
    /// or leave a partial element, each of which is
    /// [`Corrupted`](Error::Corrupted). What this gives up is detecting a
    /// peer that rewrites the *write* counter: the writer never reads it
    /// back. The writer's own accesses do not depend on that counter (it
    /// writes at its own position, inside the ring), so the breach shows on
    /// the reader's side, as the counters failing its check or wrong
    /// elements (see "One handle, one side").
    ///
    /// [`commit_write`](Self::commit_write), and so [`write`](Self::write)
    /// and [`write_blocking`](Self::write_blocking), drops the view; the
    /// next cached call loads both counters again.
    pub fn begin_write_cached(&mut self, n: usize, limit: usize) -> Result<Option<Regions<'_, T>>> {
        let limit = limit.min(self.capacity);
        if n > limit {
            return Ok(None);
        }
        let (read, write) = match self.writer_view {
            Some(view) => view,
            None => {
                let view = self.positions()?;
                self.writer_view = Some(view);
                view
            }
        };
        let q = size_of::<T>() as u64;
        let fits = |read: u64| ((write - read) / q) as usize + n <= limit;
        if !fits(read) {
            let fresh = self.read.get().load(Ordering::Acquire);
            if fresh < read {
                return Err(Error::Corrupted("read counter moved backwards"));
            }
            if fresh > write {
                return Err(Error::Corrupted("read counter ahead of the write counter"));
            }
            if write - fresh > self.ring.bytes as u64 {
                return Err(Error::Corrupted("more bytes in flight than the ring holds"));
            }
            if fresh % q != 0 {
                return Err(Error::Corrupted(
                    "counter not a multiple of the element size",
                ));
            }
            self.writer_view = Some((fresh, write));
            if !fits(fresh) {
                return Ok(None);
            }
        }
        Ok(Some(self.regions_at(write, n)))
    }

    /// Publish `n` elements written into the regions
    /// [`begin_write_cached`](Self::begin_write_cached) returned, and keep
    /// the new write position as the view. `BadValue` when no cached call
    /// made a view, or when `n` exceeds the space free by it.
    pub fn commit_write_cached(&mut self, n: usize) -> Result<()> {
        let (read, write) = self
            .writer_view
            .ok_or(Error::BadValue("commit with no cached view"))?;
        let q = size_of::<T>() as u64;
        let free = self.capacity as u64 - (write - read) / q;
        if n as u64 > free {
            return Err(Error::BadValue("commit exceeds the space that was free"));
        }
        let next = write + n as u64 * q;
        // `positions` refused a start this close; reaching it takes 2^64 bytes.
        if next > u64::MAX - self.ring.bytes as u64 {
            return Err(Error::Corrupted("counter too close to wrapping"));
        }
        self.write.get().store(next, Ordering::Release);
        self.writer_view = Some((read, next));
        Ok(())
    }

    /// The next `n` unread elements, or `None` when fewer are available (or
    /// `n` exceeds the capacity). Copy them out, then
    /// [`commit_read`](Self::commit_read) to free the space.
    pub fn begin_read(&mut self, n: usize) -> Result<Option<Regions<'_, T>>> {
        if n > self.capacity {
            return Ok(None);
        }
        let (read, write) = self.positions()?;
        let available = ((write - read) / size_of::<T>() as u64) as usize;
        if available < n {
            return Ok(None);
        }
        Ok(Some(self.regions_at(read, n)))
    }

    /// Free `n` elements read from the regions [`begin_read`](Self::begin_read)
    /// returned. `n` must not exceed the elements available at this moment.
    pub fn commit_read(&mut self, n: usize) -> Result<()> {
        let (read, write) = self.positions()?;
        let q = size_of::<T>() as u64;
        if n as u64 > (write - read) / q {
            return Err(Error::BadValue("commit exceeds the elements available"));
        }
        self.read
            .get()
            .store(read + n as u64 * q, Ordering::Release);
        Ok(())
    }

    /// Write all of `items`, or none: `Ok(false)` when they do not fit now.
    pub fn write(&mut self, items: &[T]) -> Result<bool> {
        {
            let Some(mut regions) = self.begin_write(items.len())? else {
                return Ok(false);
            };
            regions.write_at(0, items)?;
        }
        self.commit_write(items.len())?;
        Ok(true)
    }

    /// Fill all of `out`, or nothing: `Ok(false)` when fewer elements are
    /// available.
    pub fn read(&mut self, out: &mut [T]) -> Result<bool> {
        {
            let Some(regions) = self.begin_read(out.len())? else {
                return Ok(false);
            };
            regions.read_at(0, out)?;
        }
        self.commit_read(out.len())?;
        Ok(true)
    }

    /// [`write`](Self::write), waiting on the EventFlag for `wait_bits`
    /// (normally [`NOT_FULL`](crate::NOT_FULL)) while the items do not fit,
    /// and setting `wake_bits` (normally [`NOT_EMPTY`](crate::NOT_EMPTY);
    /// `0` for none) once they are in. libfmq `writeBlocking`.
    ///
    /// Fails with [`NoEventFlag`](Error::NoEventFlag) on a queue without the
    /// word, `BadValue` when `items` exceed the capacity (the wait could
    /// never end) or `wait_bits == 0`, and
    /// [`TimedOut`](Error::TimedOut) when `timeout` elapses first.
    ///
    /// Once the items are committed the call succeeds: a failure of the
    /// `wake_bits` wake that follows is logged, not returned, as libfmq's
    /// `writeBlocking` ignores it. An `Err` there would invite a retry that
    /// writes the items twice. The bits are set before the futex call, so a
    /// waiter that has not yet slept still sees them.
    pub fn write_blocking(
        &mut self,
        items: &[T],
        wait_bits: u32,
        wake_bits: u32,
        timeout: Option<Duration>,
    ) -> Result<()> {
        self.flag_ref()?;
        if wait_bits == 0 {
            return Err(Error::BadValue("empty wait mask"));
        }
        if items.len() > self.capacity {
            return Err(Error::BadValue("more items than the queue holds"));
        }
        let deadline = timeout.and_then(Deadline::after);
        loop {
            if self.write(items)? {
                self.wake_after_commit(wake_bits);
                return Ok(());
            }
            match self.flag_ref()?.wait_until(wait_bits, deadline) {
                Ok(_) => {}
                // One more try: the reader may have run while the clock ran out.
                Err(Error::TimedOut) => {
                    if self.write(items)? {
                        self.wake_after_commit(wake_bits);
                        return Ok(());
                    }
                    return Err(Error::TimedOut);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// [`read`](Self::read), waiting on the EventFlag for `wait_bits`
    /// (normally [`NOT_EMPTY`](crate::NOT_EMPTY)) while too few elements are
    /// available, and setting `wake_bits` (normally
    /// [`NOT_FULL`](crate::NOT_FULL); `0` for none) once they are out.
    /// libfmq `readBlocking`; the failures are those of
    /// [`write_blocking`](Self::write_blocking).
    pub fn read_blocking(
        &mut self,
        out: &mut [T],
        wait_bits: u32,
        wake_bits: u32,
        timeout: Option<Duration>,
    ) -> Result<()> {
        self.flag_ref()?;
        if wait_bits == 0 {
            return Err(Error::BadValue("empty wait mask"));
        }
        if out.len() > self.capacity {
            return Err(Error::BadValue("more items than the queue holds"));
        }
        let deadline = timeout.and_then(Deadline::after);
        loop {
            if self.read(out)? {
                self.wake_after_commit(wake_bits);
                return Ok(());
            }
            match self.flag_ref()?.wait_until(wait_bits, deadline) {
                Ok(_) => {}
                Err(Error::TimedOut) => {
                    if self.read(out)? {
                        self.wake_after_commit(wake_bits);
                        return Ok(());
                    }
                    return Err(Error::TimedOut);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// A fully allocated memfd of `total` bytes, page-rounded as libfmq rounds ashmem; no seal yet.
pub(crate) fn allocate_shared(total: u64) -> Result<OwnedFd> {
    let page = rustix::param::page_size() as u64;
    let total = total
        .checked_add(page - 1)
        .map(|t| t & !(page - 1))
        .ok_or(Error::BadValue("queue too large"))?;
    shm::create(total, &CreateOptions::new("rsbinder-fmq").allocate(true))
}

impl<T: Element> MessageQueue<T> {
    fn flag_ref(&self) -> Result<&EventFlag> {
        match &self.flag {
            Some(flag) => Ok(flag),
            None => Err(Error::NoEventFlag),
        }
    }

    /// The transfer is committed, so a failed wake is logged rather than returned (libfmq does too).
    fn wake_after_commit(&self, bits: u32) {
        let Some(flag) = &self.flag else { return };
        if let Err(e) = flag.wake(bits) {
            log::error!("fmq: the wake after a committed transfer failed: {e:?}");
        }
    }
}

impl<T: Element> std::fmt::Debug for MessageQueue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageQueue")
            .field("capacity", &self.capacity)
            .field("quantum", &size_of::<T>())
            .field("event_flag", &self.flag.is_some())
            .finish()
    }
}

/// The span of ring a [`begin_write`](MessageQueue::begin_write) or
/// [`begin_read`](MessageQueue::begin_read) covers: `first_len` elements
/// up to the ring's end, then `second_len` from its start (libfmq
/// `MemTransaction`). An element is never split between the two.
/// [`UnsyncWriter::begin_write`](crate::UnsyncWriter::begin_write) returns
/// one too.
///
/// Indices in [`write_at`](Self::write_at) and [`read_at`](Self::read_at)
/// run over both parts as one sequence, so a caller lays out a record
/// without knowing where the wrap falls. Every access is a copy; there is
/// no way to hold a reference into the ring.
///
/// # Soundness
///
/// Both runs of a copy lie inside the ring: the index range is checked
/// against [`len`](Self::len). The ring stays mapped while a `Regions`
/// borrows its queue. It is one read-write [`Mapping`]: a single `mmap`
/// result of at most `isize::MAX` bytes, every byte initialized as the pages
/// of a mapped file are. Every access to ring bytes is a relaxed
/// atomic load or store — `AtomicU8` up to the first `usize` boundary,
/// `AtomicUsize` words after it, `AtomicU8` for the bytes left over — and
/// the caller's slice is ordinary memory on the other side of the copy.
/// The regions of an [`UnsyncWriter`](crate::UnsyncWriter) differ in one
/// respect: their stores are `Release`, the order that flavor's readers
/// depend on.
/// Nothing hands out a reference into the ring. The split depends only on
/// which `usize`-aligned words a run covers whole — its start and its
/// length, the start being any byte for `u8` elements — so nothing relies
/// on the ring base's alignment.
///
/// A change made to the ring from outside — the peer process, another
/// handle in this process (each handle `mmap`s the memory itself: a second
/// writer, or a descriptor whose grantors overlap), C code, or `rsbinder`'s
/// `SharedMemory` on the same fd — is another thread's relaxed store in
/// Rust's memory model. A copy that runs
/// at the same time is a race between atomics, not a data race: the bytes
/// copied out are some mix of old and new, every such pattern is a `T`
/// (the [`Element`] contract), and the outcome is a wrong element or, when
/// the counters were changed too, [`Corrupted`](Error::Corrupted). It is
/// never undefined behavior. `rsbinder::shared_memory` treats a shared
/// mapping the same way.
///
/// Rust's model makes unordered atomic accesses of different sizes to the
/// same bytes undefined behavior unless both are loads, and two copies that
/// differ in start or length split the same bytes into different sizes.
/// Copies through one mapping
/// never run at the same time: a `Regions` is neither `Send` nor `Sync` and
/// borrows its queue or writer mutably, and neither of those is `Sync`.
///
/// ```compile_fail,E0277
/// fn is_send<T: Send>() {}
/// is_send::<rsbinder_fmq::Regions<'static, u8>>();
/// ```
///
/// ```compile_fail,E0277
/// fn is_sync<T: Sync>() {}
/// is_sync::<rsbinder_fmq::Regions<'static, u8>>();
/// ```
///
/// When the protocol is kept, the reader copies elements only after the
/// `Acquire` load of the write counter, which
/// [`commit_write`](MessageQueue::commit_write) stored with `Release` after
/// the writer's copy, so the reader sees every byte the writer copied
/// (libfmq's `write` is a `memcpy` followed by the same store).
pub struct Regions<'a, T: Element> {
    first: NonNull<u8>,
    first_len: usize,
    second: NonNull<u8>,
    second_len: usize,
    /// `write_at` stores with `Release`: the unsynchronized writer's order (`Run::store_release`).
    release: bool,
    _borrow: PhantomData<&'a mut [T]>,
}

impl<'a, T: Element> Regions<'a, T> {
    /// # Safety: the ring is writable and meets `Run::new` for `'a`; `n` elements fit in `bytes`.
    pub(crate) unsafe fn over(
        base: NonNull<u8>,
        bytes: usize,
        position: u64,
        n: usize,
        release: bool,
    ) -> Self {
        let q = size_of::<T>();
        let offset = (position % bytes as u64) as usize;
        let contiguous = (bytes - offset) / q;
        debug_assert_eq!(offset % q, 0);
        // SAFETY: `offset < bytes`, so the pointer stays in the ring.
        let first = unsafe { base.add(offset) };
        let (first_len, second_len) = if n > contiguous {
            (contiguous, n - contiguous)
        } else {
            (n, 0)
        };
        Regions {
            first,
            first_len,
            second: base,
            second_len,
            release,
            _borrow: PhantomData,
        }
    }
}

impl<T: Element> Regions<'_, T> {
    /// Elements covered, both parts together.
    pub fn len(&self) -> usize {
        self.first_len + self.second_len
    }

    /// Whether no element is covered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Elements in the part before the ring's end.
    pub fn first_len(&self) -> usize {
        self.first_len
    }

    /// Elements in the part after the wrap; `0` when there is no wrap.
    pub fn second_len(&self) -> usize {
        self.second_len
    }

    /// The ring runs elements `start..start + n` cover, before and after the wrap, as atomics.
    fn runs(&self, start: usize, n: usize) -> [Run<'_>; 2] {
        let q = size_of::<T>();
        let head = self.first_len.saturating_sub(start).min(n);
        // SAFETY: `start + n <= len()` (callers) keeps both runs in the ring; see "Soundness".
        unsafe {
            let first = self.first.as_ptr().add(start.min(self.first_len) * q);
            let second = self
                .second
                .as_ptr()
                .add(start.saturating_sub(self.first_len) * q);
            [
                Run::new(NonNull::new_unchecked(first), head * q),
                Run::new(NonNull::new_unchecked(second), (n - head) * q),
            ]
        }
    }

    /// Copy `src` into the elements starting at `start`. `BadValue` when
    /// `start + src.len()` exceeds [`len`](Self::len).
    pub fn write_at(&mut self, start: usize, src: &[T]) -> Result<()> {
        if start
            .checked_add(src.len())
            .is_none_or(|end| end > self.len())
        {
            return Err(Error::BadValue("write past the reserved elements"));
        }
        let [head, tail] = self.runs(start, src.len());
        let (to_head, to_tail) = ring::bytes_of(src).split_at(head.len());
        if self.release {
            head.store_release(to_head);
            tail.store_release(to_tail);
        } else {
            head.store(to_head);
            tail.store(to_tail);
        }
        Ok(())
    }

    /// Copy the elements starting at `start` into `dst`. `BadValue` when
    /// `start + dst.len()` exceeds [`len`](Self::len).
    pub fn read_at(&self, start: usize, dst: &mut [T]) -> Result<()> {
        if start
            .checked_add(dst.len())
            .is_none_or(|end| end > self.len())
        {
            return Err(Error::BadValue("read past the available elements"));
        }
        let [head, tail] = self.runs(start, dst.len());
        let (to_head, to_tail) = ring::bytes_of_mut(dst).split_at_mut(head.len());
        head.load(to_head);
        tail.load(to_tail);
        Ok(())
    }
}

impl<T: Element> std::fmt::Debug for Regions<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Regions")
            .field("first_len", &self.first_len)
            .field("second_len", &self.second_len)
            .finish()
    }
}
