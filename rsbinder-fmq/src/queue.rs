// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::cell::Cell;
use std::marker::PhantomData;
use std::mem::size_of;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::descriptor::{
    default_layout, recheck_ashmem_sizes, validate, AttachPolicy, Descriptor, Flavor, Grantor,
};
use crate::error::{received, Error, Result};
use crate::event_flag::{EventFlag, NOT_EMPTY, NOT_FULL};
pub(crate) use crate::shm::AtomicU64;
pub use crate::shm::Element;
use crate::shm::{self, Bytes, CreateOptions, Deadline, Mapping, RoMapping, Seals, Word};

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

/// The 8-byte counter at grantor `g`, mapped read-write.
pub(crate) fn map_counter(desc: &Descriptor, g: Grantor) -> Result<Word<AtomicU64>> {
    // Before any system call; `word64` would say `Unsupported` too, after the `mmap`.
    check_target()?;
    // Only the 8-byte word: a peer-sized extent would be the peer's choice of address space.
    let mut carve = map_grantor(desc, g, 8)?.carve();
    carve.word64(0).map_err(received)
}

/// The data grantor `g` of a descriptor that `validate` passed or this crate laid out, read-write.
pub(crate) fn map_ring(desc: &Descriptor, g: Grantor) -> Result<Bytes> {
    let mut carve = map_grantor(desc, g, g.extent)?.carve();
    // ≤ i32::MAX: checked by `validate`, and by `ring_bytes` for a new queue.
    carve.bytes(0, g.extent as usize).map_err(received)
}

/// `len` bytes at grantor `g`; what `Mapping::map` refuses of it is the descriptor's failure.
pub(crate) fn map_grantor(desc: &Descriptor, g: Grantor, len: u64) -> Result<Mapping> {
    let fd = desc.fds[g.fd_index as usize].as_fd();
    Mapping::map(fd, u64::from(g.offset), len).map_err(received)
}

/// [`map_grantor`], read-only.
pub(crate) fn map_grantor_ro(desc: &Descriptor, g: Grantor, len: u64) -> Result<RoMapping> {
    let fd = desc.fds[g.fd_index as usize].as_fd();
    RoMapping::map(fd, u64::from(g.offset), len).map_err(received)
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
/// A write is visible only after [`WriteTxn::commit`] moves the write
/// counter (a `Release` store after the data copy), and the reader loads
/// the counter with `Acquire`; so the reader never observes a partially
/// copied element. The reverse holds for the space a read frees
/// ([`ReadTxn::commit`]).
///
/// # The ring is the peer's memory too
///
/// Nothing here hands out a `&[T]` into the ring. [`Regions`], which a
/// transaction derefs to, copies elements in and out between the counter
/// accesses, and a reader interprets
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
    ring: Bytes,
    state: State<T>,
}

/// Everything of a queue but its ring, so that a transaction borrows the two apart.
struct State<T: Element> {
    read: Word<AtomicU64>,
    write: Word<AtomicU64>,
    ring_bytes: u64,
    flag: Option<EventFlag>,
    desc: Descriptor,
    capacity: usize,
    /// (read, write) as the cached writer last saw them; `None` until a cached write loads them.
    writer_view: Option<(u64, u64)>,
    _not_sync: PhantomData<Cell<()>>,
    // No `T` is held, so `T` decides no auto trait.
    _element: PhantomData<fn() -> T>,
}

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
        let policy = AttachPolicy::new(capacity).require_event_flag(event_flag);
        let queue = Self::open(desc, &policy)?;
        queue.state.read.store(0, Ordering::Release);
        queue.state.write.store(0, Ordering::Release);
        if let Some(flag) = &queue.state.flag {
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
    /// is [`InvalidDescriptor`](Error::InvalidDescriptor). On 32-bit Android
    /// bionic's `mmap64` also refuses (`ENOMEM`) a mapping whose page-rounded
    /// length is more than `isize::MAX`.
    ///
    /// A descriptor in which a counter, the data or the EventFlag grantor
    /// names a hugetlbfs fd (a memfd created `MFD_HUGETLB`) is
    /// `InvalidDescriptor` as well: [`Mapping::map`] refuses a file the kernel reports as
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
    /// duplicated, [`Error::InvalidDescriptor`] for a descriptor the checks
    /// above or `policy` refuse, and [`Error::Os`] when a system call those
    /// checks or the mappings make fails.
    pub fn attach(desc: &Descriptor, policy: &AttachPolicy) -> Result<Self> {
        check_target()?;
        Self::open(desc.try_clone()?, policy)
    }

    fn open(desc: Descriptor, policy: &AttachPolicy) -> Result<Self> {
        // The target's verdict comes before the descriptor's.
        check_target()?;
        let geo = validate(&desc, size_of::<T>(), policy, Flavor::SynchronizedReadWrite)?;
        let read = map_counter(&desc, geo.read)?;
        let write = map_counter(&desc, geo.write)?;
        let ring = map_ring(&desc, geo.data)?;
        let flag = match geo.event_flag {
            // The 4-byte word only, as for the counters.
            Some(g) => {
                let mut carve = map_grantor(&desc, g, 4)?.carve();
                Some(EventFlag::from_word(carve.word32(0).map_err(received)?))
            }
            None => None,
        };
        recheck_ashmem_sizes(&desc, &geo)?;
        Ok(Self {
            state: State {
                read,
                write,
                ring_bytes: ring.len() as u64,
                flag,
                desc,
                capacity: geo.capacity,
                writer_view: None,
                _not_sync: PhantomData,
                _element: PhantomData,
            },
            ring,
        })
    }

    /// The descriptor a peer attaches with; its fds are duplicates.
    pub fn descriptor(&self) -> Result<Descriptor> {
        self.state.desc.try_clone()
    }

    /// Elements the ring holds.
    pub fn capacity(&self) -> usize {
        self.state.capacity
    }

    /// Bytes per element, `size_of::<T>()`.
    pub fn quantum(&self) -> usize {
        size_of::<T>()
    }

    /// A handle on the EventFlag word, when the queue has one. It outlives
    /// the queue.
    pub fn event_flag(&self) -> Option<EventFlag> {
        self.state.flag.clone()
    }

    /// Elements that can be read now. `Err` when the counters fail their
    /// invariant.
    pub fn available_to_read(&self) -> Result<usize> {
        let (read, write) = self.state.positions()?;
        Ok(((write - read) / size_of::<T>() as u64) as usize)
    }

    /// Elements that can be written now. `Err` when the counters fail their
    /// invariant.
    pub fn available_to_write(&self) -> Result<usize> {
        Ok(self.state.capacity - self.available_to_read()?)
    }

    /// The ring's `n` elements at counter value `position`, borrowed apart from the state.
    fn regions_at(&mut self, position: u64, n: usize) -> (&mut State<T>, Regions<'_, T>) {
        assert!(n <= self.state.capacity, "more elements than the capacity");
        let regions = Regions::over(&self.ring, position, n, false);
        (&mut self.state, regions)
    }

    /// Reserve `n` elements to write. `None` when fewer are free now;
    /// [`BadValue`](Error::BadValue) when `n` exceeds the capacity, which no
    /// wait can make room for. Copy the elements in through the
    /// transaction's [`Regions`], then [`commit`](WriteTxn::commit) it; a
    /// transaction dropped without a commit publishes nothing. The two
    /// regions are the part before the ring's end and the part after its
    /// wrap.
    ///
    /// ```
    /// # #[cfg(any(target_os = "linux", target_os = "android"))]
    /// # fn main() -> Result<(), rsbinder_fmq::Error> {
    /// let mut queue = rsbinder_fmq::MessageQueue::<u32>::create(8, false)?;
    /// if let Some(mut txn) = queue.begin_write(3)? {
    ///     txn.write_at(0, &[1, 2])?;
    ///     txn.write_at(2, &[3])?;
    ///     txn.commit()?;
    /// }
    /// assert_eq!(queue.available_to_read()?, 3);
    /// # Ok(()) }
    /// # #[cfg(not(any(target_os = "linux", target_os = "android")))]
    /// # fn main() {}
    /// ```
    pub fn begin_write(&mut self, n: usize) -> Result<Option<WriteTxn<'_, T>>> {
        if n > self.state.capacity {
            return Err(Error::BadValue("more items than the queue holds"));
        }
        let (read, write) = self.state.positions()?;
        let free = self.state.capacity - ((write - read) / size_of::<T>() as u64) as usize;
        if free < n {
            return Ok(None);
        }
        let (state, regions) = self.regions_at(write, n);
        Ok(Some(WriteTxn {
            state,
            regions,
            position: write,
            path: WritePath::Loaded,
        }))
    }

    /// [`begin_write`](Self::begin_write) for a writer that relies on being
    /// the queue's only one, keeping at most `limit` elements in flight once
    /// these `n` are in (`limit` is capped at the capacity). `None` when they
    /// do not fit now; [`BadValue`](Error::BadValue) when `n` exceeds that
    /// capped `limit`, which they never fit.
    ///
    /// The write counter is this handle's own: it is loaded on the first
    /// call, then taken from what the last cached transaction's
    /// [`commit`](WriteTxn::commit) stored. The read counter is loaded again
    /// only when the last value seen leaves no room for `n`. A write that
    /// fits by that value loads no counter at all, where a `begin_write`
    /// transaction loads both counters at its begin and again at its commit,
    /// and does not load the counter the reader writes. The commit's store
    /// still writes the reader's cache line: libfmq's layout puts both
    /// counters in the first 16 bytes.
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
    /// The commit of a [`begin_write`](Self::begin_write) transaction, and
    /// so [`write`](Self::write) and [`write_blocking`](Self::write_blocking),
    /// drops the view; the next cached call loads both counters again.
    pub fn begin_write_cached(
        &mut self,
        n: usize,
        limit: usize,
    ) -> Result<Option<WriteTxn<'_, T>>> {
        let limit = limit.min(self.state.capacity);
        if n > limit {
            return Err(Error::BadValue("more items than the limit in flight"));
        }
        let state = &mut self.state;
        let (mut read, write) = match state.writer_view {
            Some(view) => view,
            None => {
                let view = state.positions()?;
                state.writer_view = Some(view);
                view
            }
        };
        let q = size_of::<T>() as u64;
        let fits = |read: u64| ((write - read) / q) as usize + n <= limit;
        if !fits(read) {
            let fresh = state.read.load(Ordering::Acquire);
            if fresh < read {
                return Err(Error::Corrupted("read counter moved backwards"));
            }
            if fresh > write {
                return Err(Error::Corrupted("read counter ahead of the write counter"));
            }
            if write - fresh > state.ring_bytes {
                return Err(Error::Corrupted("more bytes in flight than the ring holds"));
            }
            if fresh % q != 0 {
                return Err(Error::Corrupted(
                    "counter not a multiple of the element size",
                ));
            }
            state.writer_view = Some((fresh, write));
            if !fits(fresh) {
                return Ok(None);
            }
            read = fresh;
        }
        let (state, regions) = self.regions_at(write, n);
        Ok(Some(WriteTxn {
            state,
            regions,
            position: write,
            path: WritePath::Cached { read },
        }))
    }

    /// The next `n` unread elements, or `None` when fewer are available now;
    /// [`BadValue`](Error::BadValue) when `n` exceeds the capacity, which
    /// never holds that many. Copy them out through the transaction's
    /// [`Regions`], then [`commit`](ReadTxn::commit) it to free the space; a
    /// transaction dropped without a commit frees nothing, and the next read
    /// sees the same elements.
    pub fn begin_read(&mut self, n: usize) -> Result<Option<ReadTxn<'_, T>>> {
        if n > self.state.capacity {
            return Err(Error::BadValue("more items than the queue holds"));
        }
        let (read, write) = self.state.positions()?;
        let available = ((write - read) / size_of::<T>() as u64) as usize;
        if available < n {
            return Ok(None);
        }
        let (state, regions) = self.regions_at(read, n);
        Ok(Some(ReadTxn {
            state,
            regions,
            position: read,
        }))
    }

    /// Write all of `items`, or none: [`Transfer::WouldBlock`] when they do
    /// not fit now, and the items stay the caller's;
    /// [`BadValue`](Error::BadValue) when there are more than the capacity.
    pub fn write(&mut self, items: &[T]) -> Result<Transfer> {
        let Some(mut txn) = self.begin_write(items.len())? else {
            return Ok(Transfer::WouldBlock);
        };
        txn.write_at(0, items)?;
        txn.commit()?;
        Ok(Transfer::Done)
    }

    /// Fill all of `out`, or nothing: [`Transfer::WouldBlock`] when fewer
    /// elements are available now, and `out` is unchanged;
    /// [`BadValue`](Error::BadValue) when `out` is longer than the capacity.
    pub fn read(&mut self, out: &mut [T]) -> Result<Transfer> {
        let Some(txn) = self.begin_read(out.len())? else {
            return Ok(Transfer::WouldBlock);
        };
        txn.read_at(0, out)?;
        txn.commit()?;
        Ok(Transfer::Done)
    }

    /// [`write`](Self::write), waiting on the EventFlag for
    /// [`NOT_FULL`](crate::NOT_FULL) while the items do not fit and setting
    /// [`NOT_EMPTY`](crate::NOT_EMPTY) once they are in: libfmq's
    /// `writeBlocking(data, count, timeOutNanos)`.
    /// [`write_blocking_with`](Self::write_blocking_with) with
    /// [`Notify::WRITE`].
    ///
    /// `timeout` `None` waits without end, as libfmq's `timeOutNanos == 0`
    /// does; `Some(Duration::ZERO)` tries once and never waits on the
    /// EventFlag word, and is [`TimedOut`](Error::TimedOut) when the items do
    /// not fit. A transfer that succeeds sets the wake bits, whatever the
    /// timeout.
    ///
    /// Fails with [`NoEventFlag`](Error::NoEventFlag) on a queue without the
    /// word, `BadValue` when `items` exceed the capacity (the wait could
    /// never end), and `TimedOut` when `timeout` elapses first. The transfer
    /// is tried once more after the deadline, since the reader may have run
    /// while the clock ran out.
    ///
    /// Once the items are committed the call succeeds: a failure of the
    /// wake that follows is logged, not returned, as libfmq's
    /// `writeBlocking` ignores it. An `Err` there would invite a retry that
    /// writes the items twice. The bits are set before the futex call, so a
    /// waiter that has not yet slept still sees them.
    ///
    /// Only the transfer or the deadline ends the call, as in libfmq. A bit
    /// another thread or process sets wakes it only when the bit is in the
    /// wait mask; the wait then takes the bit off the word and tries the
    /// transfer again, so the bit is not seen by anyone else. To stop a
    /// blocked writer from outside, give it a deadline, or wait on the
    /// [`EventFlag`](crate::EventFlag) yourself and transfer with
    /// [`write`](Self::write).
    pub fn write_blocking(&mut self, items: &[T], timeout: Option<Duration>) -> Result<()> {
        self.write_blocking_with(items, Notify::WRITE, timeout)
    }

    /// [`write_blocking`](Self::write_blocking) with the EventFlag bits of
    /// `notify`: wait on [`notify.wait`](field@Notify::wait) while the items
    /// do not fit, set [`notify.wake`](field@Notify::wake) (`0` for none)
    /// once they are in. libfmq's `writeBlocking(data, count,
    /// readNotification, writeNotification, timeOutNanos)` is
    /// `Notify { wait: readNotification, wake: writeNotification }`.
    pub fn write_blocking_with(
        &mut self,
        items: &[T],
        notify: Notify,
        timeout: Option<Duration>,
    ) -> Result<()> {
        self.flag_ref()?;
        if items.len() > self.state.capacity {
            return Err(Error::BadValue("more items than the queue holds"));
        }
        self.blocking(notify, timeout, |queue| queue.write(items))
    }

    /// [`read`](Self::read), waiting on the EventFlag for
    /// [`NOT_EMPTY`](crate::NOT_EMPTY) while too few elements are available
    /// and setting [`NOT_FULL`](crate::NOT_FULL) once they are out: libfmq's
    /// `readBlocking(data, count, timeOutNanos)`.
    /// [`read_blocking_with`](Self::read_blocking_with) with
    /// [`Notify::READ`]. The timeout, the failures, and what ends the wait
    /// are those of [`write_blocking`](Self::write_blocking).
    pub fn read_blocking(&mut self, out: &mut [T], timeout: Option<Duration>) -> Result<()> {
        self.read_blocking_with(out, Notify::READ, timeout)
    }

    /// [`read_blocking`](Self::read_blocking) with the EventFlag bits of
    /// `notify`: wait on [`notify.wait`](field@Notify::wait) while too few
    /// elements are available, set [`notify.wake`](field@Notify::wake) (`0`
    /// for none) once they are out.
    ///
    /// libfmq's `readBlocking(data, count, readNotification,
    /// writeNotification, timeOutNanos)` takes the bits the other way round:
    /// it is `Notify { wait: writeNotification, wake: readNotification }`.
    /// libfmq names each mask after the side that sets it, and this type
    /// after what this call does with it.
    pub fn read_blocking_with(
        &mut self,
        out: &mut [T],
        notify: Notify,
        timeout: Option<Duration>,
    ) -> Result<()> {
        self.flag_ref()?;
        if out.len() > self.state.capacity {
            return Err(Error::BadValue("more items than the queue holds"));
        }
        self.blocking(notify, timeout, |queue| queue.read(out))
    }

    /// Run `transfer` until it is done, waiting on `notify.wait` between tries; wake once done.
    fn blocking(
        &mut self,
        notify: Notify,
        timeout: Option<Duration>,
        mut transfer: impl FnMut(&mut Self) -> Result<Transfer>,
    ) -> Result<()> {
        let done = |queue: &mut Self, transfer: Transfer| {
            if transfer.is_done() {
                queue.wake_after_commit(notify.wake);
            }
            transfer.is_done()
        };
        if timeout == Some(Duration::ZERO) {
            let transfer = transfer(self)?;
            return if done(self, transfer) {
                Ok(())
            } else {
                Err(Error::TimedOut)
            };
        }
        let deadline = timeout.and_then(Deadline::after);
        loop {
            let transfer_now = transfer(self)?;
            if done(self, transfer_now) {
                return Ok(());
            }
            match self.flag_ref()?.wait_until(notify.wait.get(), deadline) {
                Ok(_) => {}
                // One more try: the peer may have run while the clock ran out.
                Err(Error::TimedOut) => {
                    let last = transfer(self)?;
                    return if done(self, last) {
                        Ok(())
                    } else {
                        Err(Error::TimedOut)
                    };
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl<T: Element> State<T> {
    /// Both counters, checked against a hostile peer: order, in-flight ≤ ring, no wrap, alignment.
    fn positions(&self) -> Result<(u64, u64)> {
        let write = self.write.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Acquire);
        if read > write {
            return Err(Error::Corrupted("read counter ahead of the write counter"));
        }
        if write - read > self.ring_bytes {
            return Err(Error::Corrupted("more bytes in flight than the ring holds"));
        }
        // A commit adds at most `ring_bytes`; a counter this high would wrap.
        if write > u64::MAX - self.ring_bytes {
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

    /// Publish `n` elements at `position`, the write counter `begin_write` loaded; drops the view.
    fn commit_write(&mut self, position: u64, n: usize) -> Result<()> {
        self.writer_view = None;
        let (read, write) = self.positions()?;
        if write != position {
            return Err(Error::Corrupted(
                "write counter moved during the transaction",
            ));
        }
        let q = size_of::<T>() as u64;
        if (write - read) / q + n as u64 > self.capacity as u64 {
            return Err(Error::Corrupted(
                "read counter moved back during the transaction",
            ));
        }
        self.write.store(write + n as u64 * q, Ordering::Release);
        Ok(())
    }

    /// Publish `n` elements at `position`, the view's write counter, and keep the new one as view.
    fn commit_write_cached(&mut self, read: u64, position: u64, n: usize) -> Result<()> {
        let next = position + n as u64 * size_of::<T>() as u64;
        // `positions` refused a start this close; reaching it takes 2^64 bytes.
        if next > u64::MAX - self.ring_bytes {
            return Err(Error::Corrupted("counter too close to wrapping"));
        }
        self.write.store(next, Ordering::Release);
        self.writer_view = Some((read, next));
        Ok(())
    }

    /// Free `n` elements at `position`, the read counter `begin_read` loaded.
    fn commit_read(&mut self, position: u64, n: usize) -> Result<()> {
        let (read, write) = self.positions()?;
        if read != position {
            return Err(Error::Corrupted(
                "read counter moved during the transaction",
            ));
        }
        let q = size_of::<T>() as u64;
        if n as u64 > (write - read) / q {
            return Err(Error::Corrupted(
                "write counter moved back during the transaction",
            ));
        }
        self.read.store(read + n as u64 * q, Ordering::Release);
        Ok(())
    }
}

/// Which counter a [`WriteTxn`]'s commit reads: both again, or the cached writer's view.
#[derive(Clone, Copy, Debug)]
enum WritePath {
    Loaded,
    Cached { read: u64 },
}

/// A write reserved by [`MessageQueue::begin_write`] or
/// [`MessageQueue::begin_write_cached`]: [`len`](Regions::len) elements of
/// ring at the write position, which no reader sees until a commit.
///
/// Copy the elements in with [`write_at`](Self::write_at), then
/// [`commit`](Self::commit) all of them or
/// [`commit_first`](Self::commit_first) a prefix. The transaction borrows
/// its queue mutably, so nothing else moves the queue's counters meanwhile.
/// It derefs to its [`Regions`] for the lengths, shared only: a `&mut
/// Regions` would let two transactions swap theirs, and each then write the
/// other queue's ring and commit its own position.
///
/// ```compile_fail,E0596
/// # fn swap(a: &mut rsbinder_fmq::MessageQueue<u8>, b: &mut rsbinder_fmq::MessageQueue<u8>)
/// # -> rsbinder_fmq::Result<()> {
/// let mut x = a.begin_write(1)?.expect("room");
/// let mut y = b.begin_write(1)?.expect("room");
/// std::mem::swap(&mut *x, &mut *y);
/// # Ok(()) }
/// ```
///
/// # Dropped without a commit
///
/// A transaction dropped without a commit publishes nothing: the write
/// counter stays where it was, the next `begin_write` returns the same
/// position, and what was copied in is overwritten by the next write.
/// libfmq has no transaction object; its `commitWrite(n)` is a call of its
/// own. AOSP's Rust binding to libfmq (`WriteCompletion`) commits the
/// whole reservation when it is dropped, whether or not the elements were
/// written. This type does not, since a commit on drop publishes elements
/// that were never written — after an early return or a `?`, a reader
/// would read whatever the ring held.
#[must_use = "dropping a write transaction publishes nothing; `commit` makes the elements visible"]
pub struct WriteTxn<'a, T: Element> {
    state: &'a mut State<T>,
    regions: Regions<'a, T>,
    position: u64,
    path: WritePath,
}

impl<T: Element> WriteTxn<'_, T> {
    /// Copy `src` into the reserved elements starting at `start`, the index
    /// running across the wrap as [`Regions`] describes. `BadValue` when
    /// `start + src.len()` exceeds [`len`](Regions::len).
    pub fn write_at(&mut self, start: usize, src: &[T]) -> Result<()> {
        self.regions.write_at(start, src)
    }

    /// Publish every element the transaction reserved.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupted`] when the counters fail their check, or when the
    /// write counter is no longer where the transaction began or the read
    /// counter moved backwards: another party broke the protocol. A
    /// transaction of [`begin_write_cached`](MessageQueue::begin_write_cached)
    /// loads no counter here and fails only when the write counter would
    /// come within one ring of wrapping. Nothing is published on an `Err`.
    pub fn commit(self) -> Result<()> {
        let n = self.regions.len();
        self.commit_first(n)
    }

    /// Publish the first `n` of the elements reserved; the rest are not
    /// published, and the next write starts right after the `n`.
    ///
    /// # Errors
    ///
    /// [`Error::BadValue`] when `n` exceeds [`len`](Regions::len), and those
    /// of [`commit`](Self::commit).
    pub fn commit_first(self, n: usize) -> Result<()> {
        if n > self.regions.len() {
            return Err(Error::BadValue("commit exceeds the elements reserved"));
        }
        match self.path {
            WritePath::Loaded => self.state.commit_write(self.position, n),
            WritePath::Cached { read } => self.state.commit_write_cached(read, self.position, n),
        }
    }
}

impl<'a, T: Element> std::ops::Deref for WriteTxn<'a, T> {
    type Target = Regions<'a, T>;

    fn deref(&self) -> &Regions<'a, T> {
        &self.regions
    }
}

impl<T: Element> std::fmt::Debug for WriteTxn<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteTxn")
            .field("regions", &self.regions)
            .field("cached", &matches!(self.path, WritePath::Cached { .. }))
            .finish()
    }
}

/// A read reserved by [`MessageQueue::begin_read`]: the next
/// [`len`](Regions::len) unread elements, which stay in the ring until a
/// commit frees their space.
///
/// Copy the elements out through the [`Regions`] the transaction derefs to,
/// then [`commit`](Self::commit) all of them or
/// [`commit_first`](Self::commit_first) a prefix. A transaction dropped
/// without a commit frees nothing: the next `begin_read` returns the same
/// elements. The transaction gives no way to write the ring.
#[must_use = "dropping a read transaction frees nothing; `commit` consumes the elements"]
pub struct ReadTxn<'a, T: Element> {
    state: &'a mut State<T>,
    regions: Regions<'a, T>,
    position: u64,
}

impl<T: Element> ReadTxn<'_, T> {
    /// Free the space of every element the transaction covers.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupted`] when the counters fail their check, or when the
    /// read counter is no longer where the transaction began or the write
    /// counter moved backwards: another party broke the protocol. Nothing
    /// is freed on an `Err`.
    pub fn commit(self) -> Result<()> {
        let n = self.regions.len();
        self.commit_first(n)
    }

    /// Free the space of the first `n` elements; the rest stay unread, and
    /// the next read starts with them.
    ///
    /// # Errors
    ///
    /// [`Error::BadValue`] when `n` exceeds [`len`](Regions::len), and those
    /// of [`commit`](Self::commit).
    pub fn commit_first(self, n: usize) -> Result<()> {
        if n > self.regions.len() {
            return Err(Error::BadValue("commit exceeds the elements reserved"));
        }
        self.state.commit_read(self.position, n)
    }
}

impl<'a, T: Element> std::ops::Deref for ReadTxn<'a, T> {
    type Target = Regions<'a, T>;

    fn deref(&self) -> &Regions<'a, T> {
        &self.regions
    }
}

impl<T: Element> std::fmt::Debug for ReadTxn<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadTxn")
            .field("regions", &self.regions)
            .finish()
    }
}

/// What [`MessageQueue::write`] or [`MessageQueue::read`] did.
///
/// The type is `#[must_use]`, so a caller that drops the answer — and with
/// it the items of a write that would block — is warned, after `?` too:
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// fn send(queue: &mut rsbinder_fmq::MessageQueue<u8>) -> rsbinder_fmq::Result<()> {
///     queue.write(&[1])?;
///     Ok(())
/// }
/// ```
///
/// ```
/// #![deny(unused_must_use)]
/// fn send(queue: &mut rsbinder_fmq::MessageQueue<u8>) -> rsbinder_fmq::Result<bool> {
///     Ok(queue.write(&[1])?.is_done())
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[must_use = "`WouldBlock` means nothing was transferred: the items are still the caller's"]
pub enum Transfer {
    /// Every element was written or read.
    Done,
    /// The elements did not fit (a write) or were not all there (a read)
    /// now; nothing was transferred and no counter moved.
    WouldBlock,
}

impl Transfer {
    /// Whether the elements were transferred.
    pub const fn is_done(self) -> bool {
        matches!(self, Transfer::Done)
    }
}

/// The EventFlag bits of a blocking call: what it waits on while the
/// transfer cannot be made, and what it sets once it has been.
///
/// Named fields rather than two positional `u32`s, because libfmq's
/// `writeBlocking` and `readBlocking` put them in opposite orders (see
/// [`MessageQueue::read_blocking_with`]). The wait mask is never empty: a
/// wait on no bit could only end at the deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Notify {
    /// Bits to wait on; the wait consumes them (see [`EventFlag`]).
    pub wait: NonZeroU32,
    /// Bits to set, and wake their waiters, after the transfer; `0` sets
    /// none.
    pub wake: u32,
}

impl Notify {
    /// The bits of [`MessageQueue::write_blocking`]: wait on
    /// [`NOT_FULL`](crate::NOT_FULL), wake [`NOT_EMPTY`](crate::NOT_EMPTY).
    pub const WRITE: Notify = Notify::new(mask(NOT_FULL), NOT_EMPTY);
    /// The bits of [`MessageQueue::read_blocking`]: wait on
    /// [`NOT_EMPTY`](crate::NOT_EMPTY), wake [`NOT_FULL`](crate::NOT_FULL).
    pub const READ: Notify = Notify::new(mask(NOT_EMPTY), NOT_FULL);

    /// Wait on `wait`, wake `wake`.
    pub const fn new(wait: NonZeroU32, wake: u32) -> Self {
        Self { wait, wake }
    }
}

/// `bits` of a constant mask, which is never zero.
const fn mask(bits: u32) -> NonZeroU32 {
    match NonZeroU32::new(bits) {
        Some(mask) => mask,
        None => panic!("an EventFlag mask constant is zero"),
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
        match &self.state.flag {
            Some(flag) => Ok(flag),
            None => Err(Error::NoEventFlag),
        }
    }

    /// The transfer is committed, so a failed wake is logged, not returned (libfmq does too).
    fn wake_after_commit(&self, bits: u32) {
        let Some(flag) = &self.state.flag else { return };
        if let Err(e) = flag.wake(bits) {
            log::error!("fmq: the wake after a committed transfer failed: {e:?}");
        }
    }
}

impl<T: Element> std::fmt::Debug for MessageQueue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageQueue")
            .field("capacity", &self.state.capacity)
            .field("quantum", &size_of::<T>())
            .field("event_flag", &self.state.flag.is_some())
            .finish()
    }
}

/// The span of ring a transaction covers — [`WriteTxn`], [`ReadTxn`] and
/// [`UnsyncWriteTxn`](crate::UnsyncWriteTxn) deref to one, shared only:
/// `first_len` elements up to the ring's end, then `second_len` from its
/// start (libfmq `MemTransaction`). An element is never split between the
/// two. Only a write transaction copies in, through its own
/// [`WriteTxn::write_at`] or
/// [`UnsyncWriteTxn::write_at`](crate::UnsyncWriteTxn::write_at): the copy
/// into the ring is not reachable through a shared `Regions`.
///
/// Indices in those `write_at`s and in [`read_at`](Self::read_at)
/// run over both parts as one sequence, so a caller lays out a record
/// without knowing where the wrap falls. Every access is a copy; there is
/// no way to hold a reference into the ring.
///
/// # Soundness
///
/// Every copy goes through the ring's [`Bytes`](shm::Bytes), which checks
/// the range against the ring's length and gives each ring byte one access
/// size for every copy, whatever its start and length (see that type); the
/// caller's slice is ordinary memory on the other side of the copy. `'a` is
/// a shared borrow of the ring, which the transaction takes apart from the
/// mutable borrow of the queue's or writer's other state, so the ring stays
/// mapped while a `Regions` lives. The regions of an
/// [`UnsyncWriter`](crate::UnsyncWriter) differ in one respect: their
/// stores are `Release`, the order that flavor's readers depend on.
///
/// A change made to the ring from outside — the peer process, another
/// handle in this process (each handle `mmap`s the memory itself: a second
/// writer, or a descriptor whose grantors overlap), C code, or `rsbinder`'s
/// `SharedMemory` on the same fd — is another thread's atomic store in
/// Rust's memory model. A copy that runs
/// at the same time is a race between atomics, not a data race: the bytes
/// copied out are some mix of old and new, every such pattern is a `T`
/// (the [`Element`] contract), and the outcome is a wrong element or, when
/// the counters were changed too, [`Corrupted`](Error::Corrupted). It is
/// never undefined behavior.
///
/// When the protocol is kept, the reader copies elements only after the
/// `Acquire` load of the write counter, which [`WriteTxn::commit`] stored
/// with `Release` after the writer's copy, so the reader sees every byte the
/// writer copied (libfmq's `write` is a `memcpy` followed by the same
/// store).
pub struct Regions<'a, T: Element> {
    ring: &'a Bytes,
    /// The ring byte the first part starts at.
    first_at: usize,
    first_len: usize,
    second_len: usize,
    /// `write_at` stores with `Release`: the order the unsynchronized writer's readers need.
    release: bool,
    _element: PhantomData<fn() -> T>,
}

impl<'a, T: Element> Regions<'a, T> {
    /// `n` elements of `ring` at counter value `position`; a span past the ring panics at the copy.
    pub(crate) fn over(ring: &'a Bytes, position: u64, n: usize, release: bool) -> Self {
        let q = size_of::<T>();
        let bytes = ring.len();
        let first_at = (position % bytes as u64) as usize;
        let contiguous = (bytes - first_at) / q;
        debug_assert_eq!(first_at % q, 0);
        let (first_len, second_len) = if n > contiguous {
            (contiguous, n - contiguous)
        } else {
            (n, 0)
        };
        Regions {
            ring,
            first_at,
            first_len,
            second_len,
            release,
            _element: PhantomData,
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

    /// Where elements `start..start + n` lie: ring bytes before and after the wrap, and how many
    /// of them come before it.
    fn split(&self, start: usize, n: usize) -> (usize, usize, usize) {
        let q = size_of::<T>();
        let head = self.first_len.saturating_sub(start).min(n);
        let first = self.first_at + start.min(self.first_len) * q;
        let second = start.saturating_sub(self.first_len) * q;
        (first, second, head)
    }

    /// Copy `src` into the elements starting at `start`. `BadValue` when
    /// `start + src.len()` exceeds [`len`](Self::len).
    pub(crate) fn write_at(&mut self, start: usize, src: &[T]) -> Result<()> {
        if start
            .checked_add(src.len())
            .is_none_or(|end| end > self.len())
        {
            return Err(Error::BadValue("write past the reserved elements"));
        }
        let (first, second, head) = self.split(start, src.len());
        let (to_first, to_second) = src.split_at(head);
        if self.release {
            self.ring.store_release(first, to_first);
            self.ring.store_release(second, to_second);
        } else {
            self.ring.store(first, to_first);
            self.ring.store(second, to_second);
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
        let (first, second, head) = self.split(start, dst.len());
        let (to_first, to_second) = dst.split_at_mut(head);
        self.ring.load(first, to_first);
        self.ring.load(second, to_second);
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

#[cfg(all(test, not(loom)))]
mod tests;
