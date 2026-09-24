// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::cell::Cell;
use std::marker::PhantomData;
use std::mem::size_of;
use std::os::fd::AsFd;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::descriptor::{default_layout, validate, AttachPolicy, Descriptor, Flavor, Grantor};
use crate::error::{Error, Result};
use crate::event_flag::EventFlag;
use crate::sys::{self, Mapping};

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
        // SAFETY: a primitive integer or float has no padding, every bit
        // pattern is a value, and its alignment is at most 8.
        unsafe impl Element for $t {}
    )* };
}
elements!(u8, i8, u16, i16, u32, i32, u64, i64, f32, f64);

struct Counter {
    _mapping: Mapping,
    ptr: NonNull<AtomicU64>,
}

impl Counter {
    fn map(desc: &Descriptor, g: Grantor) -> Result<Self> {
        let mapping = Mapping::map(
            desc.fds[g.fd_index as usize].as_fd(),
            u64::from(g.offset),
            g.extent,
        )?;
        let ptr = mapping.ptr().cast::<AtomicU64>();
        debug_assert_eq!(ptr.as_ptr() as usize % std::mem::align_of::<AtomicU64>(), 0);
        Ok(Self {
            _mapping: mapping,
            ptr,
        })
    }

    fn get(&self) -> &AtomicU64 {
        // SAFETY: `ptr` lies inside `_mapping`, is 8-aligned (grantor
        // offsets are multiples of 8, mappings page-aligned), and every peer
        // touches it only atomically.
        unsafe { self.ptr.as_ref() }
    }
}

struct Ring {
    _mapping: Mapping,
    base: NonNull<u8>,
    bytes: usize,
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
/// A `MessageQueue` is `Send` but not `Sync`: every operation takes
/// `&mut self`, and the flavor's rule that there is one reader and one
/// writer is the caller's to keep. Two handles on the same queue in one
/// process (one from [`create`](Self::create), one from
/// [`attach`](Self::attach)) are fine as long as one only writes and the
/// other only reads.
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
/// elements in and out with volatile accesses, and a reader interprets
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
    _not_sync: PhantomData<Cell<()>>,
    _element: PhantomData<T>,
}

// SAFETY: the raw pointers point into mappings the struct owns (`Mapping`
// is `Send`), and no other handle aliases them mutably through this one.
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
    /// `capacity` must be at least 1 and `capacity * size_of::<T>()` at
    /// most `i32::MAX` (the AIDL `extent` field's range).
    pub fn create(capacity: usize, event_flag: bool) -> Result<Self> {
        let quantum = size_of::<T>();
        if quantum == 0 {
            return Err(Error::BadValue("zero-sized element"));
        }
        if capacity == 0 {
            return Err(Error::BadValue("capacity of zero"));
        }
        let data_bytes = (capacity as u64)
            .checked_mul(quantum as u64)
            .filter(|b| *b <= i32::MAX as u64)
            .ok_or(Error::BadValue("queue too large"))?;
        let (grantors, total) = default_layout(data_bytes, event_flag);
        let fd = sys::create_shared(total)?;
        let desc = Descriptor {
            fds: vec![fd],
            ints: Vec::new(),
            grantors,
            quantum: quantum as u32,
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
    /// duplicated; `desc` stays usable.
    ///
    /// The counters are left as they are. A queue is attached once per
    /// side; attaching a second handle to read while the first writes is
    /// the caller's responsibility to keep to one reader and one writer.
    pub fn attach(desc: &Descriptor, policy: &AttachPolicy) -> Result<Self> {
        Self::open(desc.try_clone()?, policy)
    }

    fn open(desc: Descriptor, policy: &AttachPolicy) -> Result<Self> {
        let geo = validate(&desc, size_of::<T>(), policy)?;
        let read = Counter::map(&desc, geo.read)?;
        let write = Counter::map(&desc, geo.write)?;
        let ring = {
            let g = geo.data;
            let mapping = Mapping::map(
                desc.fds[g.fd_index as usize].as_fd(),
                u64::from(g.offset),
                g.extent,
            )?;
            Ring {
                base: mapping.ptr(),
                // ≤ i32::MAX, checked by `validate`.
                bytes: g.extent as usize,
                _mapping: mapping,
            }
        };
        let flag = match geo.event_flag {
            Some(g) => Some(EventFlag::from_mapping(Mapping::map(
                desc.fds[g.fd_index as usize].as_fd(),
                u64::from(g.offset),
                g.extent,
            )?)),
            None => None,
        };
        Ok(Self {
            read,
            write,
            ring,
            flag,
            desc,
            capacity: geo.capacity,
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

    /// Both counters, checked: `read ≤ write`, `write − read ≤ ring bytes`,
    /// both multiples of the quantum. libfmq's reader skips the second
    /// check; without it a peer that inflates the write counter makes the
    /// wrapped region longer than the ring.
    fn positions(&self) -> Result<(u64, u64)> {
        let write = self.write.get().load(Ordering::Acquire);
        let read = self.read.get().load(Ordering::Acquire);
        if read > write {
            return Err(Error::Corrupted("read counter ahead of the write counter"));
        }
        if write - read > self.ring.bytes as u64 {
            return Err(Error::Corrupted("more bytes in flight than the ring holds"));
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
        let q = size_of::<T>();
        let offset = (position % self.ring.bytes as u64) as usize;
        let contiguous = (self.ring.bytes - offset) / q;
        // SAFETY: `offset < bytes`, so the pointer stays inside the ring.
        let first = unsafe { self.ring.base.as_ptr().add(offset) }.cast::<T>();
        let (first_len, second_len) = if n > contiguous {
            (contiguous, n - contiguous)
        } else {
            (n, 0)
        };
        Regions {
            first: NonNull::new(first).expect("ring pointer is non-null"),
            first_len,
            second: self.ring.base.cast::<T>(),
            second_len,
            _borrow: PhantomData,
        }
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
    pub fn commit_write(&mut self, n: usize) -> Result<()> {
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
    pub fn write_blocking(
        &mut self,
        items: &[T],
        wait_bits: u32,
        wake_bits: u32,
        timeout: Option<Duration>,
    ) -> Result<()> {
        let flag = self.flag.clone().ok_or(Error::NoEventFlag)?;
        if wait_bits == 0 {
            return Err(Error::BadValue("empty wait mask"));
        }
        if items.len() > self.capacity {
            return Err(Error::BadValue("more items than the queue holds"));
        }
        let deadline = sys::deadline_after(timeout)?;
        loop {
            if self.write(items)? {
                return flag.wake(wake_bits);
            }
            match flag.wait_until(wait_bits, deadline) {
                Ok(_) => {}
                // One more try: the reader may have run while the clock ran out.
                Err(Error::TimedOut) => {
                    if self.write(items)? {
                        return flag.wake(wake_bits);
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
        let flag = self.flag.clone().ok_or(Error::NoEventFlag)?;
        if wait_bits == 0 {
            return Err(Error::BadValue("empty wait mask"));
        }
        if out.len() > self.capacity {
            return Err(Error::BadValue("more items than the queue holds"));
        }
        let deadline = sys::deadline_after(timeout)?;
        loop {
            if self.read(out)? {
                return flag.wake(wake_bits);
            }
            match flag.wait_until(wait_bits, deadline) {
                Ok(_) => {}
                Err(Error::TimedOut) => {
                    if self.read(out)? {
                        return flag.wake(wake_bits);
                    }
                    return Err(Error::TimedOut);
                }
                Err(e) => return Err(e),
            }
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
///
/// Indices in [`write_at`](Self::write_at) and [`read_at`](Self::read_at)
/// run over both parts as one sequence, so a caller lays out a record
/// without knowing where the wrap falls. Every access is a volatile copy;
/// there is no way to hold a reference into the ring.
pub struct Regions<'a, T: Element> {
    first: NonNull<T>,
    first_len: usize,
    second: NonNull<T>,
    second_len: usize,
    _borrow: PhantomData<&'a mut [T]>,
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

    fn slot(&self, index: usize) -> *mut T {
        // SAFETY: callers check `index < len()`, and each part's pointer
        // plus its length stays inside the ring.
        unsafe {
            if index < self.first_len {
                self.first.as_ptr().add(index)
            } else {
                self.second.as_ptr().add(index - self.first_len)
            }
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
        for (i, item) in src.iter().enumerate() {
            // SAFETY: `start + i < len()`, checked above; the slot is
            // aligned for `T` (ring base is 8-aligned, `T`'s alignment ≤ 8,
            // and its size is a multiple of that). Volatile: the peer may
            // read the ring concurrently, and the counter store that
            // publishes the copy is a separate `Release`.
            unsafe { std::ptr::write_volatile(self.slot(start + i), *item) };
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
        for (i, item) in dst.iter_mut().enumerate() {
            // SAFETY: as in `write_at`. Any bit pattern is a `T` (the
            // `Element` contract), so a concurrent rewrite by the peer
            // yields a wrong value, not an invalid one.
            *item = unsafe { std::ptr::read_volatile(self.slot(start + i)) };
        }
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
