// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::cell::Cell;
use std::marker::PhantomData;
use std::mem::size_of;
use std::os::fd::AsFd;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;

use crate::descriptor::{
    default_layout, recheck_ashmem_sizes, validate, AttachPolicy, Descriptor, Flavor, Grantor,
};
use crate::error::{Error, Result};
use crate::queue::{
    allocate_shared, check_target, ring_bytes, AtomicU64, Counter, Element, Regions, Ring,
};
use crate::ring;
use crate::shm::{self, Access, Mapping, RoRun, RoWord64, Seals};

mod rules;

use rules::{Shape, View};

/// How [`UnsyncWriter::create`] makes the queue's memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnsyncCreate {
    /// Seal the memfd `F_SEAL_FUTURE_WRITE` once the writer has mapped it,
    /// so that no holder of the descriptor can map the queue writable: a
    /// reader cannot change the counters or the ring under the other
    /// readers. [`UnsyncReader`] maps read-only whatever this is. A libfmq
    /// reader falls back to a read-only mapping when the writable one is
    /// refused (`MessageQueueBase.h`, `mapGrantorDescr`), and must then be
    /// constructed with `resetPointers = false` — see
    /// [`UnsyncWriter::descriptor`].
    ///
    /// Without it the memfd is sealed against resizing only, as a
    /// synchronized queue's is, and every holder of the descriptor can write
    /// the shared memory.
    pub read_only_readers: bool,
}

impl UnsyncCreate {
    /// A queue any holder of the descriptor can map writable.
    pub const fn new() -> Self {
        Self {
            read_only_readers: false,
        }
    }

    /// Set [`read_only_readers`](field@Self::read_only_readers).
    pub const fn read_only_readers(mut self, read_only_readers: bool) -> Self {
        self.read_only_readers = read_only_readers;
        self
    }
}

/// Where an [`UnsyncReader`] starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnsyncStart {
    /// At counter value zero, as a libfmq reader does. On a queue whose
    /// writer has already written more than the ring holds, the first read
    /// reports [`UnsyncRead::Lost`] and continues half a ring behind the
    /// writer.
    Oldest,
    /// At the write counter as it is when the reader attaches: the reader
    /// gets what is committed from then on. Elements the writer had reserved
    /// and not yet committed at that moment are among them.
    Now,
}

/// What [`UnsyncReader::read`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "`Empty` and `Lost` leave the output buffer without the elements asked for"]
pub enum UnsyncRead {
    /// The buffer holds the next elements, in the order they were written.
    Done,
    /// Fewer elements than the buffer holds are committed. Nothing was
    /// consumed; the buffer is unchanged.
    Empty,
    /// The writer overwrote elements this reader had not read. The reader
    /// skipped `elements` of them — always at least one — and the next read
    /// starts at the oldest element it kept. Nothing was read by this call;
    /// the buffer's contents are unspecified.
    ///
    /// libfmq reports the same event as a `read` that returns `false`.
    Lost {
        /// How many elements the reader skipped.
        elements: u64,
    },
}

/// A counter the writer stores: 8 bytes it mapped read-write, or an `AtomicU64` of a test.
struct Stored {
    ptr: NonNull<AtomicU64>,
}

impl Stored {
    fn get(&self) -> &AtomicU64 {
        // SAFETY: `WriterCore::new`'s contract: an `AtomicU64` for as long as the core lives.
        unsafe { self.ptr.as_ref() }
    }
}

/// The writer's protocol over two counters and a ring, mapped or not (a test has them on the heap).
struct WriterCore {
    end: Stored,
    write: Stored,
    ring: NonNull<u8>,
    shape: Shape,
    /// The write counter. The shared word is only stored, so what a peer does to it is not read.
    written: u64,
    /// The region end as last stored. It is never lowered.
    reserved: u64,
}

impl WriterCore {
    /// # Safety: for `self`'s life, `AtomicU64`s and `shape.bytes` of writable ring per `Run::new`.
    unsafe fn new(
        end: NonNull<AtomicU64>,
        write: NonNull<AtomicU64>,
        ring: NonNull<u8>,
        shape: Shape,
    ) -> Self {
        Self {
            end: Stored { ptr: end },
            write: Stored { ptr: write },
            ring,
            shape,
            written: 0,
            reserved: 0,
        }
    }

    /// Announce a write of `bytes` at the write counter, whose value is returned.
    fn reserve(&mut self, bytes: u64) -> Result<u64> {
        // `View::new` refuses a counter above this, so the writer never stores one.
        let upto = self
            .written
            .checked_add(bytes)
            .filter(|upto| *upto <= u64::MAX - self.shape.bytes)
            .ok_or(Error::Corrupted("counter too close to wrapping"))?;
        self.reserved = self.reserved.max(upto);
        // Before any byte of the write: a reader that copies one of them then loads this value.
        self.end.get().store(self.reserved, Ordering::Release);
        Ok(self.written)
    }

    fn commit(&mut self, bytes: u64) -> Result<()> {
        if bytes > self.reserved - self.written {
            return Err(Error::BadValue("commit exceeds the elements reserved"));
        }
        self.written += bytes;
        self.write.get().store(self.written, Ordering::Release);
        Ok(())
    }

    /// Reserve `n` elements, fewer than the ring holds, and hand out their ring bytes.
    fn begin<T: Element>(&mut self, n: usize) -> Result<Regions<'_, T>> {
        let bytes = self.shape.bytes as usize;
        if n >= bytes / size_of::<T>() {
            return Err(Error::BadValue("more elements than the capacity less one"));
        }
        let position = self.reserve((n * size_of::<T>()) as u64)?;
        // SAFETY: `new`'s contract for the ring, borrowed with `self`; `n` elements fit in it.
        Ok(unsafe { Regions::over(self.ring, bytes, position, n, true) })
    }

    /// Commit the first `n` of the elements `begin` reserved.
    fn finish<T: Element>(&mut self, n: usize) -> Result<()> {
        if n >= self.shape.bytes as usize / size_of::<T>() {
            return Err(Error::BadValue("commit exceeds the elements reserved"));
        }
        self.commit((n * size_of::<T>()) as u64)
    }
}

/// The writing end of an unsynchronized Fast Message Queue: one writer, any
/// number of readers, and a write that never waits — when the ring is full
/// it overwrites the oldest elements, and a reader that had not read them
/// finds out on its next read.
///
/// This is the writer of libfmq's `MessageQueue<T, kUnsynchronizedWrite>`,
/// with the memory layout and counter discipline of `android-17.0.0_r1`
/// (`system/libfmq`, `MessageQueueBase.h`): grantor 0 is the *write region
/// end*, grantor 1 the write counter, grantor 2 the ring. There is no shared
/// read counter — each reader keeps its own — and no EventFlag word: the
/// flavor has no blocking operations in libfmq either.
///
/// ```
/// # #[cfg(any(target_os = "linux", target_os = "android"))]
/// # fn main() -> Result<(), rsbinder_fmq::Error> {
/// use rsbinder_fmq::{
///     AttachPolicy, UnsyncCreate, UnsyncRead, UnsyncReader, UnsyncStart, UnsyncWriter,
/// };
///
/// // The writer allocates; `read_only_readers` seals the memory against the readers.
/// let options = UnsyncCreate::new().read_only_readers(true);
/// let mut writer = UnsyncWriter::<u32>::create(4, &options)?;
/// let descriptor = writer.descriptor()?; // send it to each reader
///
/// let policy = AttachPolicy {
///     max_capacity: 1 << 16,
///     require_seal: true,
///     require_event_flag: false,
/// };
/// let mut reader = UnsyncReader::<u32>::attach(&descriptor, &policy, UnsyncStart::Now)?;
///
/// writer.write(&[1, 2])?;
/// let mut pair = [0u32; 2];
/// assert_eq!(reader.read(&mut pair)?, UnsyncRead::Done);
/// assert_eq!(pair, [1, 2]);
///
/// // Six more through a ring of four: the reader was lapped, and is told so.
/// for value in 3..=8 {
///     writer.write(&[value])?;
/// }
/// assert_eq!(reader.read(&mut pair)?, UnsyncRead::Lost { elements: 4 });
/// assert_eq!(reader.read(&mut pair)?, UnsyncRead::Done);
/// assert_eq!(pair, [7, 8]);
/// # Ok(()) }
/// # #[cfg(not(any(target_os = "linux", target_os = "android")))]
/// # fn main() {}
/// ```
///
/// # What a write does
///
/// [`begin_write`](Self::begin_write) stores the counter value the write
/// will reach in the write region end, *before* any element is copied;
/// [`commit_write`](Self::commit_write) then moves the write counter. A
/// reader that copied elements while the writer was overwriting them loads
/// the region end after its copy, sees that the reserved region reaches the
/// position it read from, and discards what it copied
/// ([`UnsyncRead::Lost`]). The stores into the ring are `Release` and the
/// reader's loads end with an acquire fence
/// ([`Run::store_release`](shm::Run::store_release)), so that check is
/// ordered in Rust's memory model and not only on the processors it was
/// tested on.
///
/// One write is at most `capacity - 1` elements. A lapped reader resumes
/// half a ring behind the writer, which is possible only while less than the
/// whole ring is being rewritten; libfmq accepts a write of the whole
/// capacity and logs that it defeats this.
///
/// # The region end is never lowered
///
/// libfmq's `commitWrite` lowers the region end to the new write counter
/// when fewer elements are committed than were reserved. A writer that
/// filled the whole reservation and committed part of it has then
/// overwritten elements that no counter covers any more, and a reader
/// returns them as valid. This type keeps the region end where
/// `begin_write` put it until the write counter passes it. A reader may
/// then report a loss that a lowered region end would have hidden; it never
/// returns an overwritten element. The value stays within what a libfmq
/// reader accepts.
///
/// # Peers
///
/// * [`UnsyncReader`] follows the protocol above.
/// * A libfmq reader from `android-16.0.0_r4` follows it too.
/// * A libfmq reader before that release does not load the region end: a
///   read that overlaps a write in progress can return overwritten
///   elements. That is the defect `android-16.0.0_r4` fixed, and with such
///   a reader it is still there on the reader's side.
///
/// Every libfmq reader must attach with `resetPointers = false`; see
/// [`descriptor`](Self::descriptor).
///
/// # One handle, one writer
///
/// An `UnsyncWriter` is `Send` and not `Sync`, and every operation takes
/// `&mut self`. Nothing detects a second writer on the same memory (another
/// process that mapped the descriptor writable, C code): the readers then
/// see counters that fail their checks, or wrong elements. The writer keeps
/// its counters to itself and only stores the shared ones, so nothing a
/// peer writes there changes where the writer writes; it is never undefined
/// behavior (see "Soundness" on [`Regions`]).
///
/// ```compile_fail,E0277
/// fn is_sync<T: Sync>() {}
/// is_sync::<rsbinder_fmq::UnsyncWriter<u8>>();
/// ```
pub struct UnsyncWriter<T: Element> {
    core: WriterCore,
    desc: Descriptor,
    capacity: usize,
    _end: Counter,
    _write: Counter,
    _ring: Ring,
    _not_sync: PhantomData<Cell<()>>,
    _element: PhantomData<T>,
}

// SAFETY: pointers into owned `Mapping`s (`Send`), reached only via atomics (`Stored`, `Run`).
unsafe impl<T: Element> Send for UnsyncWriter<T> {}

impl<T: Element> UnsyncWriter<T> {
    /// Allocate a queue of `capacity` elements in a new memfd.
    ///
    /// The layout is libfmq's single-fd layout without the EventFlag word:
    /// the write region end at 0, the write counter at 8, the ring at 16.
    /// Every page is allocated up front (`fallocate`), as for
    /// [`MessageQueue::create`](crate::MessageQueue::create). The writer
    /// maps the memory and only then seals the fd: `F_SEAL_GROW |
    /// F_SEAL_SHRINK | F_SEAL_SEAL`, and `F_SEAL_FUTURE_WRITE` as well with
    /// [`read_only_readers`](field@UnsyncCreate::read_only_readers) — that
    /// seal refuses every writable mapping made after it, the writer's own
    /// included, and leaves the one already made.
    ///
    /// `capacity` must be at least 2, since one write is at most
    /// `capacity - 1` elements, and `capacity * size_of::<T>()` at most
    /// `i32::MAX` less the page size, as for a synchronized queue.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] comes first, whatever the arguments are, on a
    /// target that cannot run a queue: one that is not Linux or Android, or
    /// has no 64-bit atomics. On the other targets: [`Error::BadValue`] for
    /// a zero-sized `T` or a `capacity` outside the bounds above;
    /// [`Error::Unsupported`] when `read_only_readers` is set and the kernel
    /// has no `F_SEAL_FUTURE_WRITE` (it answers `EINVAL`; mainline Linux has
    /// the seal from 5.1, the Android common kernels from 4.14);
    /// [`Error::Os`] when creating, allocating, mapping or sealing the memfd
    /// fails otherwise.
    pub fn create(capacity: usize, options: &UnsyncCreate) -> Result<Self> {
        check_target()?;
        let data_bytes = ring_bytes::<T>(capacity)?;
        if capacity < 2 {
            return Err(Error::BadValue("capacity below two"));
        }
        let (grantors, total) = default_layout(data_bytes, false);
        let desc = Descriptor {
            fds: vec![allocate_shared(total)?],
            ints: Vec::new(),
            grantors,
            quantum: size_of::<T>() as u32,
            flavor: Flavor::UnsynchronizedWrite,
        };
        let end = Counter::map(&desc, desc.grantors[Descriptor::WRITE_REGION_END])?;
        let write = Counter::map(&desc, desc.grantors[Descriptor::WRITE_COUNTER])?;
        let ring = Ring::map(&desc, desc.grantors[Descriptor::DATA], Access::ReadWrite)?;
        // After the mappings: `FUTURE_WRITE` refuses every writable mapping made from here on.
        let mut seals = Seals::GROW | Seals::SHRINK | Seals::SEAL;
        if options.read_only_readers {
            seals |= Seals::FUTURE_WRITE;
        }
        match shm::add_seals(desc.fds[0].as_fd(), seals) {
            // The kernel applies none of a mask that holds a seal it does not know.
            Err(Error::Os(rustix::io::Errno::INVAL)) if options.read_only_readers => {
                return Err(Error::Unsupported)
            }
            other => other?,
        }
        let shape = Shape {
            bytes: data_bytes,
            quantum: size_of::<T>() as u64,
        };
        // SAFETY: words and ring of the three mappings `self` owns; fresh memfd pages are zero.
        let core = unsafe { WriterCore::new(end.address(), write.address(), ring.base, shape) };
        Ok(Self {
            core,
            desc,
            capacity,
            _end: end,
            _write: write,
            _ring: ring,
            _not_sync: PhantomData,
            _element: PhantomData,
        })
    }

    /// The descriptor a reader attaches with; its fds are duplicates and its
    /// flavor is [`Flavor::UnsynchronizedWrite`].
    ///
    /// # A libfmq reader attaches with `resetPointers = false`
    ///
    /// libfmq's constructor from a descriptor takes `resetPointers`, `true`
    /// by default, and with it stores zero in the shared write counter and
    /// write region end (`MessageQueueBase.h`). On a queue that is in use
    /// that is a write by a second party to the writer's counters:
    ///
    /// * With [`read_only_readers`](field@UnsyncCreate::read_only_readers)
    ///   the reader's mapping is read-only, the store faults, and that
    ///   reader's process ends with `SIGSEGV`. The writer and the other
    ///   readers are not affected.
    /// * Without it both shared counters become zero. The writer keeps its
    ///   own and stores them again with its next
    ///   [`begin_write`](Self::begin_write) and
    ///   [`commit_write`](Self::commit_write); until then an
    ///   [`UnsyncReader`] that looks sees the write counter behind what it
    ///   saw before and fails with [`Error::Corrupted`], and a read that
    ///   overlaps the write in progress may miss it.
    ///
    /// [`UnsyncReader::attach`] stores nothing.
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

    /// Elements committed since the queue was created.
    pub fn written(&self) -> u64 {
        self.core.written / size_of::<T>() as u64
    }

    /// Reserve `n` elements at the write position and announce the write to
    /// the readers. Fill the regions, then
    /// [`commit_write`](Self::commit_write). The two regions of the result
    /// are the part before the ring's end and the part after its wrap.
    ///
    /// A write never waits for space: the regions may cover elements no
    /// reader has read yet, and from this call on a reader treats the
    /// reserved elements' positions as overwritten. Reserving again before a
    /// commit starts over from the same position; the announcement covers
    /// the longer of the two.
    ///
    /// # Errors
    ///
    /// [`Error::BadValue`] when `n` is more than `capacity - 1`.
    pub fn begin_write(&mut self, n: usize) -> Result<Regions<'_, T>> {
        self.core.begin(n)
    }

    /// Publish the first `n` of the elements reserved by
    /// [`begin_write`](Self::begin_write).
    ///
    /// `n` may be less than what was reserved. The rest of the reservation
    /// stays announced as being written (see "The region end is never
    /// lowered" on the type), and a later `commit_write` may publish it
    /// without another `begin_write`.
    ///
    /// # Errors
    ///
    /// [`Error::BadValue`] when `n` is more than the elements reserved and
    /// not yet committed.
    pub fn commit_write(&mut self, n: usize) -> Result<()> {
        self.core.finish::<T>(n)
    }

    /// Write all of `items`, overwriting the oldest elements when the ring
    /// is full.
    ///
    /// # Errors
    ///
    /// [`Error::BadValue`] when `items` is more than `capacity - 1`
    /// elements.
    pub fn write(&mut self, items: &[T]) -> Result<()> {
        self.begin_write(items.len())?.write_at(0, items)?;
        self.commit_write(items.len())
    }
}

impl<T: Element> std::fmt::Debug for UnsyncWriter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnsyncWriter")
            .field("capacity", &self.capacity)
            .field("quantum", &size_of::<T>())
            .field("written", &self.written())
            .finish()
    }
}

/// The reader's protocol over two counters and a ring it only loads, mapped or not.
struct ReaderCore {
    end: NonNull<u8>,
    write: NonNull<u8>,
    ring: NonNull<u8>,
    shape: Shape,
    /// This reader's read counter. It lives here, not in shared memory.
    read: u64,
    /// The highest write counter loaded so far; `read` never passes it.
    seen: Cell<u64>,
    /// Set by the first corrupted view; every later call fails with it.
    failed: Cell<Option<&'static str>>,
}

impl ReaderCore {
    /// # Safety: as `WriterCore::new`, but words per `RoWord64::from_ptr` and a readable ring.
    unsafe fn new(end: NonNull<u8>, write: NonNull<u8>, ring: NonNull<u8>, shape: Shape) -> Self {
        Self {
            end,
            write,
            ring,
            shape,
            read: 0,
            seen: Cell::new(0),
            failed: Cell::new(None),
        }
    }

    /// Both counters as they are now, checked.
    fn view(&self) -> Result<View> {
        if let Some(what) = self.failed.get() {
            return Err(Error::Corrupted(what));
        }
        // SAFETY: `new`'s contract: both words meet `RoWord64::from_ptr` while `self` lives.
        let (end, write) =
            unsafe { (RoWord64::from_ptr(self.end), RoWord64::from_ptr(self.write)) };
        // `end` first: the `write` loaded after it is at least the one `end` was reserved from.
        let end = end.load_acquire();
        let write = write.load_acquire();
        let view = View::new(self.seen.get(), write, end, self.shape).inspect_err(|e| {
            if let Error::Corrupted(what) = e {
                self.failed.set(Some(what));
            }
        })?;
        self.seen.set(view.write);
        Ok(view)
    }

    /// Copy the ring bytes at counter value `position` into `dst`, at most one ring of them.
    fn copy_out(&self, position: u64, dst: &mut [u8]) {
        let bytes = self.shape.bytes as usize;
        assert!(dst.len() <= bytes, "a copy longer than the ring");
        let offset = (position % self.shape.bytes) as usize;
        let (head, tail) = dst.split_at_mut(dst.len().min(bytes - offset));
        // SAFETY: `new`'s contract; `offset < bytes` and the assert put both runs inside the ring.
        unsafe {
            RoRun::new(self.ring.add(offset), head.len()).load_acquire(head);
            RoRun::new(self.ring, tail.len()).load_acquire(tail);
        }
    }

    /// Move to `resume`, where `View::lapped` put this reader.
    fn skip_to(&mut self, resume: u64) -> UnsyncRead {
        let elements = (resume - self.read) / self.shape.quantum;
        self.read = resume;
        UnsyncRead::Lost { elements }
    }

    fn start_now(&mut self) -> Result<()> {
        self.read = self.view()?.write;
        Ok(())
    }

    /// Bytes committed and not read by this reader.
    fn pending(&self) -> Result<u64> {
        Ok(self.view()?.write - self.read)
    }

    /// Fill `dst`, whole elements, from the read position.
    fn take(&mut self, dst: &mut [u8]) -> Result<UnsyncRead> {
        let want = dst.len() as u64;
        if want > self.shape.bytes {
            return Err(Error::BadValue("more elements than the ring holds"));
        }
        let view = self.view()?;
        if let Some(resume) = view.lapped(self.read, self.shape) {
            return Ok(self.skip_to(resume));
        }
        if view.write - self.read < want {
            return Ok(UnsyncRead::Empty);
        }
        self.copy_out(self.read, dst);
        // The writer may have overwritten what was copied; the counters loaded after it say so.
        if let Some(resume) = self.view()?.lapped(self.read, self.shape) {
            return Ok(self.skip_to(resume));
        }
        self.read += want;
        Ok(UnsyncRead::Done)
    }

    /// Copy the newest elements into the front of `dst` and consume all; the bytes copied.
    fn take_latest(&mut self, dst: &mut [u8]) -> Result<usize> {
        let view = self.view()?;
        let (start, length) = view.latest(self.read, dst.len() as u64, self.shape);
        if length == 0 {
            self.read = view.write;
            return Ok(0);
        }
        let length = length as usize;
        self.copy_out(start, &mut dst[..length]);
        let after = self.view()?;
        if after.lapped(start, self.shape).is_some() {
            // `read <= start`, so this reader is lapped too.
            if let Some(resume) = after.lapped(self.read, self.shape) {
                self.read = resume;
            }
            return Ok(0);
        }
        self.read = view.write;
        Ok(length)
    }
}

/// A read-only 8-byte word of a queue's fd.
struct Word {
    _mapping: Mapping,
    at: NonNull<u8>,
}

impl Word {
    fn map(desc: &Descriptor, g: Grantor) -> Result<Self> {
        // Only the 8-byte word: a peer-sized extent would be the peer's choice of address space.
        let mapping = Mapping::map(
            desc.fds[g.fd_index as usize].as_fd(),
            u64::from(g.offset),
            8,
            Access::ReadOnly,
        )?;
        Ok(Self {
            at: mapping.as_ptr(),
            _mapping: mapping,
        })
    }
}

/// A reading end of an unsynchronized Fast Message Queue: it reads what the
/// writer wrote, in order, at its own pace, and when the writer has
/// overwritten elements it had not read yet it is told so and continues
/// from newer ones.
///
/// This is a reader of libfmq's `MessageQueue<T, kUnsynchronizedWrite>` as
/// of `android-17.0.0_r1` (`system/libfmq`, `MessageQueueBase.h`). A queue
/// has any number of readers. Each keeps its read counter in its own
/// process, so readers do not affect one another and the writer does not
/// know they exist.
///
/// # The memory is mapped read-only
///
/// The reader maps both counters and the ring `PROT_READ`, whether or not
/// the fd would allow more, and reaches them through
/// `shm::RoWord64` and [`RoRun`], which have no method that
/// stores. A reader cannot disturb the queue.
/// [`attach`](Self::attach) stores nothing either, where libfmq's
/// constructor resets the shared counters by default.
///
/// A 64-bit counter in a read-only mapping needs `RoWord64`, which has a
/// value on the 64-bit architectures, on 32-bit x86 with SSE2 and on 32-bit ARM
/// with 64-bit atomics (its documentation gives the grounds).
/// [`attach`](Self::attach) returns [`Error::Unsupported`] on any other
/// target.
///
/// # What a read may return
///
/// An element returned with [`UnsyncRead::Done`] is one the writer
/// committed, whole, in order: after a copy the reader loads both shared
/// counters again and discards the copy when the write counter or the write
/// region end is more than one ring past the position it copied from
/// (libfmq `processOverflow`). The copy's loads end with an acquire fence
/// and the writer's stores are `Release`
/// ([`Run::store_release`](shm::Run::store_release)), so a reader that
/// copied even one overwritten byte then loads a region end that covers it.
///
/// That holds against an [`UnsyncWriter`]. Against a libfmq writer the
/// check is libfmq's own, and as strong as libfmq's:
///
/// * libfmq copies with `memcpy`, and its source notes that the C++
///   standard does not order a `memcpy` against the atomic stores around it
///   (`MessageQueueBase.h`, `beginRead`); it was stress-tested on x86-64
///   and Arm64.
/// * A libfmq writer before `android-16.0.0_r4` does not store the region
///   end. The reader then judges by the write counter alone, which moves
///   only at the commit, and a copy that overlaps a write in progress can
///   be returned.
/// * A libfmq writer that commits fewer elements than it reserved lowers
///   the region end; elements it overwrote beyond the commit can be
///   returned (see [`UnsyncWriter`]).
///
/// After a loss the reader resumes half a ring behind the write counter, as
/// libfmq does from `android-15.0.0_r6`, or at the write counter when the
/// write in progress reaches into that half.
///
/// # A writer that breaks the protocol
///
/// The counters are the writer's memory and are checked on every call. A
/// write counter that is not a multiple of the element size, that moved
/// backwards or is about to wrap, or a region end more than one ring past
/// the write counter is [`Error::Corrupted`], and from then on every call
/// on this reader fails the same way: a counter that went backwards cannot
/// be told from one that will be put right. A second process that resets
/// the shared counters — a libfmq reader attached with the default
/// `resetPointers = true` to a queue without
/// [`read_only_readers`](field@UnsyncCreate::read_only_readers) — is such a
/// case.
///
/// It is never undefined behavior: every access is an atomic load, every
/// copy is bounded by the mapped ring, and whatever bytes arrive are a `T`
/// (the [`Element`] contract).
///
/// # One handle, one reader
///
/// An `UnsyncReader` is `Send` and not `Sync`. Any number of them may be
/// attached to one queue, in one process or several.
///
/// ```compile_fail,E0277
/// fn is_sync<T: Sync>() {}
/// is_sync::<rsbinder_fmq::UnsyncReader<u8>>();
/// ```
pub struct UnsyncReader<T: Element> {
    core: ReaderCore,
    capacity: usize,
    _end: Word,
    _write: Word,
    _ring: Ring,
    _not_sync: PhantomData<Cell<()>>,
    _element: PhantomData<T>,
}

// SAFETY: pointers into owned `Mapping`s (`Send`), reached only via atomic loads.
unsafe impl<T: Element> Send for UnsyncReader<T> {}

impl<T: Element> UnsyncReader<T> {
    /// Map a queue a peer described, read-only, after checking the
    /// descriptor against `policy` and libfmq's own rules as
    /// [`MessageQueue::attach`](crate::MessageQueue::attach) does (see
    /// [`AttachPolicy`]). `desc` is borrowed and stays usable; the mappings
    /// outlive its fds.
    ///
    /// The descriptor's flavor must be [`Flavor::UnsynchronizedWrite`]. A
    /// queue [`UnsyncWriter`] made has three grantors, and one libfmq made
    /// may have the EventFlag word as a fourth; it is validated when present
    /// and never mapped. `policy.require_event_flag` therefore refuses every
    /// queue of this crate's writer.
    ///
    /// Nothing is stored: the shared counters are left as they are.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] comes first, whatever `desc` and `policy`
    /// hold: on a target that is not Linux or Android, has no 64-bit
    /// atomics, or has `shm::RoWord64::PRESENT` false. On the other targets:
    /// [`Error::BadValue`] for a descriptor the checks refuse,
    /// [`Error::Os`] when a system call those checks or the mappings make
    /// fails, and, with [`UnsyncStart::Now`], [`Error::Corrupted`] when the
    /// counters fail their check.
    pub fn attach(desc: &Descriptor, policy: &AttachPolicy, start: UnsyncStart) -> Result<Self> {
        check_target()?;
        if !RoWord64::PRESENT {
            return Err(Error::Unsupported);
        }
        let geo = validate(desc, size_of::<T>(), policy, Flavor::UnsynchronizedWrite)?;
        let end = Word::map(desc, geo.read)?;
        let write = Word::map(desc, geo.write)?;
        let ring = Ring::map(desc, geo.data, Access::ReadOnly)?;
        recheck_ashmem_sizes(desc, &geo)?;
        let shape = Shape {
            bytes: ring.bytes as u64,
            quantum: size_of::<T>() as u64,
        };
        // SAFETY: words and ring of the three read-only mappings `self` owns, each at one size.
        let mut core = unsafe { ReaderCore::new(end.at, write.at, ring.base, shape) };
        if start == UnsyncStart::Now {
            core.start_now()?;
        }
        Ok(Self {
            core,
            capacity: geo.capacity,
            _end: end,
            _write: write,
            _ring: ring,
            _not_sync: PhantomData,
            _element: PhantomData,
        })
    }

    /// Elements the ring holds.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes per element, `size_of::<T>()`.
    pub fn quantum(&self) -> usize {
        size_of::<T>()
    }

    /// Elements committed and not read by this reader, as libfmq's
    /// `availableToRead` counts them: the distance from the read counter to
    /// the write counter. It is more than [`capacity`](Self::capacity) when
    /// the writer has committed past this reader. A reader may be lapped
    /// with a smaller count, by a write that is reserved and not yet
    /// committed; the next [`read`](Self::read) reports the loss in both
    /// cases. The count is `usize::MAX` when it does not fit.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupted`] when the counters fail their check.
    pub fn available_to_read(&self) -> Result<usize> {
        let elements = self.core.pending()? / size_of::<T>() as u64;
        Ok(usize::try_from(elements).unwrap_or(usize::MAX))
    }

    /// Fill all of `out` with the next elements, or read nothing; see
    /// [`UnsyncRead`] for the three outcomes.
    ///
    /// # Errors
    ///
    /// [`Error::BadValue`] when `out` is longer than
    /// [`capacity`](Self::capacity), which no read could fill, and
    /// [`Error::Corrupted`] when the counters fail their check.
    pub fn read(&mut self, out: &mut [T]) -> Result<UnsyncRead> {
        self.core.take(ring::bytes_of_mut(out))
    }

    /// Copy the newest elements into the front of `out` — as many as it
    /// holds, fewer when fewer are unread — and consume everything
    /// committed so far. Returns how many elements were copied; they are in
    /// the order they were written, the newest last. libfmq `readLatest`.
    ///
    /// A reader that wants the current value and not the backlog calls this
    /// with a buffer of one element. Elements skipped this way are not
    /// reported as lost.
    ///
    /// `0` means one of three things: nothing was committed since the last
    /// read; a write in progress reaches every unread element (a libfmq
    /// writer that reserved the whole ring), and they are skipped without a
    /// copy; or the writer overwrote the elements while they were copied.
    /// In the last case the front of `out` holds unspecified bytes, the
    /// reader has moved on as after [`UnsyncRead::Lost`], and the next call
    /// copies newer ones. An empty `out` consumes everything committed so
    /// far and returns `0` as well. Where libfmq's `readLatest` returns `0`
    /// because a write is in progress on a full ring, this returns the
    /// elements that write does not reach, which is none of them only when
    /// the write reserves the whole ring.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupted`] when the counters fail their check.
    pub fn read_latest(&mut self, out: &mut [T]) -> Result<usize> {
        let copied = self.core.take_latest(ring::bytes_of_mut(out))?;
        Ok(copied / size_of::<T>())
    }
}

impl<T: Element> std::fmt::Debug for UnsyncReader<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnsyncReader")
            .field("capacity", &self.capacity)
            .field("quantum", &size_of::<T>())
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests;

#[cfg(all(test, loom))]
mod loom_tests;
