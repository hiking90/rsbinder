// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Shared-memory IPC: AOSP `IMemory` / `IMemoryHeap` for rsbinder (plan 4-7a).
//!
//! AOSP `IMemory.h`
//! ([source](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/IMemory.h))
//! describes **handwritten** C++ binders, not AIDL. This module mirrors
//! that surface:
//!
//! * [`IMemoryHeap`](crate::shared_memory::IMemoryHeap) / [`IMemory`](crate::shared_memory::IMemory) — the trait surface.
//! * [`MemoryHeapBase`](crate::shared_memory::MemoryHeapBase) — owner-side heap (`memfd_create` on
//!   Linux/Android, `shm_open` on macOS). [`MappedHeap`](crate::shared_memory::MappedHeap) — the
//!   receiver-side mapping of an fd that arrived through a parcel.
//!   See [Heaps](#heaps) for the backing-store table and the aliasing
//!   contract.
//! * [`MemoryDealer`](crate::shared_memory::MemoryDealer) — AOSP's chunk
//!   allocator: one heap, many `IMemory` windows, with
//!   [`HeapCache`](crate::shared_memory::HeapCache) on the receiving side
//!   so the heap is mapped once. See [Dealer](#dealer).
//! * [`SharedMemory`] — `android.os.SharedMemory`; see [SharedMemory](#sharedmemory).
//! * [`BnMemoryHeap`] / [`BpMemoryHeap`] / [`BnMemory`] / [`BpMemory`] — the
//!   `IMemoryHeap` / `IMemory` wire; see [Wire](#wire).
//!
//! A shared region travels over any transport that can carry an fd:
//! the kernel binder (`BINDER_TYPE_FD`) and Unix-socket RPC with
//! `rpc::FileDescriptorTransportMode::Unix` (the `rpc` feature)
//! negotiated on both ends. TCP / vsock / TLS sessions cannot carry
//! fds at all; writing a heap fd into such a parcel fails with
//! `StatusCode::FdsNotAllowed` exactly as a plain
//! [`ParcelFileDescriptor`] does.
//!
//! Targets without a backing store (anything other than Linux,
//! Android, macOS) compile the whole trait surface, but every
//! `MemoryHeapBase` constructor returns
//! `Err(StatusCode::InvalidOperation)`; the receiver side
//! (`MappedHeap::from_fd`) still maps a received fd. Check
//! [`is_supported`](crate::shared_memory::is_supported) to branch at runtime.
//!
//! # Heaps
//!
//! Concrete shared-memory heaps (plan 4-7a Phase A).
//!
//! * [`MemoryHeapBase`] — the *owner* side. Allocates an anonymous
//!   shared region and (by default) maps it into this process.
//! * [`MappedHeap`] — the *receiver* side. Wraps an fd that arrived
//!   through a parcel plus the wire geometry (`size`, `offset`,
//!   `flags`) and maps it.
//!
//! Backing store by target:
//!
//! | target | create | size guard |
//! |---|---|---|
//! | linux / android | `memfd_create(MFD_CLOEXEC \| MFD_ALLOW_SEALING)` | `F_SEAL_GROW \| F_SEAL_SHRINK` (+`F_SEAL_FUTURE_WRITE` for read-only, +`F_SEAL_SEAL` unless [`FLAG_MEMFD_ALLOW_SEALING`]) — the AOSP `MemoryHeapBase::FORCE_MEMFD` path |
//! | macos | `shm_open` + immediate `shm_unlink` (two fds: `O_RDWR` for the owner, `O_RDONLY` to export) | kernel-inherent: a POSIX shm object accepts exactly **one** `ftruncate` (≡ `F_SEAL_GROW \| F_SEAL_SHRINK`), and an `O_RDONLY` fd refuses `PROT_WRITE` mappings and `mprotect` upgrades (≡ `F_SEAL_FUTURE_WRITE`). See plan 4-7b |
//! | other | no backing store — every [`MemoryHeapBase`] constructor returns `InvalidOperation`, and [`is_supported`] is `false`. The receiver side is not all gone: see [`MappedHeap::from_fd`] | — |
//!
//! The owner mapping is always created *before* seals are applied so
//! that `F_SEAL_FUTURE_WRITE` leaves the owner's own mapping writable,
//! exactly like AOSP `MemoryHeapBase.cpp`. Receivers map `PROT_READ`
//! only when [`FLAG_READ_ONLY`] is set — and on every backend the
//! kernel refuses a writable mapping to a peer that ignores the flag
//! (Linux 5.1+; an older kernel lacks `F_SEAL_FUTURE_WRITE`, which
//! `seals()` reports).
//!
//! `seals()` reports the **effective protections** of the fd a heap
//! exports as `SEAL_*` bits: `F_GET_SEALS` on Linux/Android, synthesized
//! from the kernel semantics above on macOS (`SEAL_SHRINK | SEAL_GROW`,
//! plus `SEAL_FUTURE_WRITE` when the exported fd is read-only; never
//! `SEAL_SEAL`). On macOS only a POSIX shm object qualifies: it has no
//! vnode, so `st_mode` carries no `S_IFMT` bits. A regular file (resizable
//! by any holder) yields `None`, matching `F_GET_SEALS` failing on
//! non-memfd fds on Linux.
//!
//! **Aliasing.** A shared mapping is, by definition, writable by other
//! processes at any time, so no safe accessor ever exposes it as `&[u8]`:
//! [`read_at`](crate::shared_memory::MemoryHeapBase::read_at) /
//! [`write_at`](crate::shared_memory::MemoryHeapBase::write_at)
//! copy through atomics, and [`IMemoryHeap::base`] returns the read-only
//! [`SharedBytes`] view built on the same copies. Only the raw
//! [`as_ptr`](crate::shared_memory::MemoryHeapBase::as_ptr) carries AOSP's `unsecurePointer()`
//! caveat, and it is the caller's `unsafe` to dereference.
//!
//! The heaps' shared state is `Send + Sync` on the same basis. The mapping
//! is a `rsbinder_fmq::shm::Mapping`, an address range with no thread
//! affinity that is `Send + Sync` itself, and every safe access goes through
//! `Mapping::region` — atomic loads and stores, uniformly word-sized on the
//! word part — so concurrent `read_at`/`write_at`/`base` from several
//! threads (or another process) are data races only in the hardware sense,
//! never in Rust's model. That is the contract `shm::Mapping` sets for the
//! code that turns its address into references: atomic access only, no byte
//! at two sizes through one mapping, and `Relaxed` loads no wider than a
//! `usize` through a read-only one. Stores are gated on the heap being
//! writable, so a `PROT_READ` mapping is never written. The atomic view
//! relies on the mapping starting on a page boundary (the heap's offset is
//! page-aligned, so the word part is `usize`-aligned), `AtomicUsize` having
//! `usize`'s size and alignment, and `AtomicU8` being `repr(transparent)`
//! over `u8`.
//!
//! `shm::Mapping` adds two refusals to the `mmap` it makes, both
//! `BadValue`: a range of more than `isize::MAX` bytes, which no Rust slice
//! may span (a 32-bit target only), and on Linux and Android an fd the
//! kernel reports as a hugetlbfs file, whose mapping could not be released
//! on drop.
//!
//! # Dealer
//!
//! `MemoryDealer` — AOSP's chunk allocator over one shared heap
//! (`frameworks/native/libs/binder/MemoryDealer.cpp`, plan 4-7a Phase D).
//!
//! One heap is allocated and published **once**; every
//! [`allocate`](crate::shared_memory::MemoryDealer::allocate) carves an `(offset, size)`
//! window out of it and hands it back as an [`Allocation`] — an
//! `android.utils.IMemory` whose `GET_MEMORY` reply names the shared
//! heap binder. A peer that keeps a [`HeapCache`]
//! therefore maps the heap a single time and then resolves each
//! allocation to a bare offset: no fd passing and no `mmap` per buffer,
//! which is the whole point for streaming (camera / audio) traffic.
//!
//! Allocator = AOSP `SimpleBestFitAllocator`: 32-byte granules
//! (`kMemoryAlign`), best-fit over a sorted free list, and coalescing
//! with both neighbours on free. `PAGE_ALIGNED` allocations are
//! supported through
//! [`allocate_page_aligned`](crate::shared_memory::MemoryDealer::allocate_page_aligned).
//!
//! # SharedMemory
//!
//! `android.os.SharedMemory` / NDK `ASharedMemory` equivalent (plan
//! 4-7a Phase C).
//!
//! On the wire a `SharedMemory` is **one file descriptor** and nothing
//! else (`SharedMemory.java` `writeToParcel` → `writeFileDescriptor`);
//! the receiver recovers the size from the fd itself. So an AIDL
//! `ParcelFileDescriptor` argument/return carrying a memfd or ashmem fd
//! is byte-identical to a Java/NDK `SharedMemory`, and this type is the
//! convenience wrapper on both ends.
//!
//! Size recovery follows libcutils `ashmem_get_size_region`: `fstat`
//! for a memfd / POSIX shm object, and — on Android — the
//! `ASHMEM_GET_SIZE` ioctl for a legacy `/dev/ashmem` fd, which reports
//! `st_size == 0`.
//!
//! # Wire
//!
//! Wire-faithful `android.utils.IMemoryHeap` / `android.utils.IMemory`
//! stubs (plan 4-7a Phase B).
//!
//! AOSP `IMemory.cpp` is handwritten, not AIDL; the transaction layout
//! reproduced here is (android17-release):
//!
//! | interface | code | request | reply |
//! |---|---|---|---|
//! | `android.utils.IMemoryHeap` | [`HEAP_ID`] = `FIRST_CALL_TRANSACTION` | token only | **raw** `fd` object · `u64 size` · `i64 offset` · `u32 flags` (`IMemory.cpp:404-409`) |
//! | `android.utils.IMemory` | [`GET_MEMORY`] = `FIRST_CALL_TRANSACTION` | token only | `strong binder (heap)` · `i64 offset` · `u64 size` (`IMemory.cpp:239-245`) |
//!
//! Neither reply carries a `Status` header — the transaction status is
//! the only error channel, as in AOSP. The heap fd is AOSP
//! `writeFileDescriptor` / `readFileDescriptor`: the bare fd object with
//! **no** `ParcelFileDescriptor` not-null / comm markers (STAGE3 against
//! real libbinder is what pins this — rsbinder↔rsbinder is symmetric
//! and would not notice an 8-byte skew). The interface token on the
//! request is checked by the native dispatcher before `on_transact`
//! runs, so the `Bn*` types here do not re-check it.
//!
//! Proxies go through `SIBinder::as_remote` (kernel `ProxyHandle` or
//! RPC `RpcProxy`) and fall back to an in-process
//! `Transactable` call for a local binder, so one `Bp*` serves all
//! three cases. A `Bp*` resolves its remote geometry **once**, on
//! first use, like AOSP `BpMemoryHeap::assertReallyMapped()` /
//! `BpMemory::getMemory()`; it never re-transacts.
//!
//! [`SharedMemory`]: crate::shared_memory::SharedMemory
//! [`BnMemoryHeap`]: crate::shared_memory::BnMemoryHeap
//! [`BpMemoryHeap`]: crate::shared_memory::BpMemoryHeap
//! [`BnMemory`]: crate::shared_memory::BnMemory
//! [`BpMemory`]: crate::shared_memory::BpMemory
//! [`MemoryHeapBase`]: crate::shared_memory::MemoryHeapBase
//! [`MappedHeap`]: crate::shared_memory::MappedHeap
//! [`MappedHeap::from_fd`]: crate::shared_memory::MappedHeap::from_fd
//! [`FLAG_MEMFD_ALLOW_SEALING`]: crate::shared_memory::FLAG_MEMFD_ALLOW_SEALING
//! [`FLAG_READ_ONLY`]: crate::shared_memory::FLAG_READ_ONLY
//! [`is_supported`]: crate::shared_memory::is_supported
//! [`IMemoryHeap::base`]: crate::shared_memory::IMemoryHeap::base
//! [`SharedBytes`]: crate::shared_memory::SharedBytes
//! [`Allocation`]: crate::shared_memory::Allocation
//! [`HeapCache`]: crate::shared_memory::HeapCache
//! [`HEAP_ID`]: crate::shared_memory::HEAP_ID
//! [`GET_MEMORY`]: crate::shared_memory::GET_MEMORY
//!
//! # Mixed-size atomics
//!
//! The crate-private `Region` view splits a mapped shared region into its `usize`-aligned
//! words and the `< WORD` trailing bytes. Every access is word-sized on the word part (partial
//! words go through a CAS) and byte-sized on the tail: Rust's memory model makes overlapping
//! atomic accesses of *different* sizes a data race when one is a write, so a byte-granular
//! fast path would race the word path of another thread writing the same window.

mod dealer;
pub(crate) mod heap;
pub(crate) mod shared;
mod wire;

use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use crate::error::{Result, StatusCode};

pub use dealer::{Allocation, MemoryDealer, ALLOCATION_ALIGNMENT};
pub use heap::{
    is_supported, MappedHeap, MemoryHeapBase, FLAG_DONT_MAP_LOCALLY, FLAG_FORCE_MEMFD,
    FLAG_MEMFD_ALLOW_SEALING, FLAG_NO_CACHING, SEAL_FUTURE_WRITE, SEAL_GROW, SEAL_SEAL,
    SEAL_SHRINK, SEAL_WRITE,
};
pub use shared::{region_size, SharedMemory};
pub use wire::{
    export_heap, BnMemory, BnMemoryHeap, BpMemory, BpMemoryHeap, HeapCache, MemoryBase, GET_MEMORY,
    HEAP_ID, IMEMORY_DESCRIPTOR, IMEMORY_HEAP_DESCRIPTOR,
};

/// AOSP `IMemoryHeap::READ_ONLY` flag
/// ([IMemory.h:37-39](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/IMemory.h;l=37)).
/// Receivers map `PROT_READ` only. On Linux/Android the owner also
/// applies `F_SEAL_FUTURE_WRITE` so the kernel refuses a writable
/// mapping from a peer that ignores the flag (Linux 5.1+; an older
/// kernel lacks that seal, which `seals()` reports); the owner's own
/// mapping, created before the seal, stays writable.
pub const FLAG_READ_ONLY: u32 = 0x0000_0001;

pub(crate) const WORD: usize = std::mem::size_of::<usize>();

/// Mapped region as `usize` words + a `< WORD` tail; see module doc "Mixed-size atomics".
#[derive(Clone, Copy)]
pub(crate) struct Region<'a> {
    words: &'a [AtomicUsize],
    tail: &'a [AtomicU8],
}

impl<'a> Region<'a> {
    pub(crate) fn new(words: &'a [AtomicUsize], tail: &'a [AtomicU8]) -> Self {
        Self { words, tail }
    }

    pub(crate) fn len(&self) -> usize {
        self.words.len() * WORD + self.tail.len()
    }

    fn check(&self, off: usize, len: usize) -> Result<()> {
        let end = off.checked_add(len).ok_or(StatusCode::BadValue)?;
        if end > self.len() {
            return Err(StatusCode::BadValue);
        }
        Ok(())
    }

    /// Copy `dst.len()` bytes out, starting at `off` (relaxed loads).
    pub(crate) fn load(&self, off: usize, dst: &mut [u8]) -> Result<()> {
        self.check(off, dst.len())?;
        let word_bytes = self.words.len() * WORD;
        let mut done = 0;
        while done < dst.len() && off + done < word_bytes {
            let at = off + done;
            let shift = at % WORD;
            let n = (WORD - shift).min(dst.len() - done);
            let w = self.words[at / WORD].load(Ordering::Relaxed).to_ne_bytes();
            dst[done..done + n].copy_from_slice(&w[shift..shift + n]);
            done += n;
        }
        let tail_at = (off + done).saturating_sub(word_bytes);
        for (d, s) in dst[done..].iter_mut().zip(&self.tail[tail_at..]) {
            *d = s.load(Ordering::Relaxed);
        }
        Ok(())
    }

    /// Copy `src` in at `off` (relaxed stores; a partial word is CAS-merged to keep neighbours).
    pub(crate) fn store(&self, off: usize, src: &[u8]) -> Result<()> {
        self.check(off, src.len())?;
        let word_bytes = self.words.len() * WORD;
        let mut done = 0;
        while done < src.len() && off + done < word_bytes {
            let at = off + done;
            let shift = at % WORD;
            let n = (WORD - shift).min(src.len() - done);
            let w = &self.words[at / WORD];
            let chunk = &src[done..done + n];
            if n == WORD {
                let full = usize::from_ne_bytes(chunk.try_into().expect("one word"));
                w.store(full, Ordering::Relaxed);
            } else {
                let mut cur = w.load(Ordering::Relaxed);
                loop {
                    let mut bytes = cur.to_ne_bytes();
                    bytes[shift..shift + n].copy_from_slice(chunk);
                    let next = usize::from_ne_bytes(bytes);
                    match w.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
                        Ok(_) => break,
                        Err(seen) => cur = seen,
                    }
                }
            }
            done += n;
        }
        let tail_at = (off + done).saturating_sub(word_bytes);
        for (d, s) in self.tail[tail_at..].iter().zip(&src[done..]) {
            d.store(*s, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// Read-only view of a mapped shared-memory window — what
/// [`IMemoryHeap::base`] returns.
///
/// The window aliases memory another process (or, through `write_at`,
/// another thread) writes at any moment, so the view hands out neither a
/// `&[u8]` — which would promise an immutability shared memory cannot keep
/// — nor a way to store: reads copy through relaxed atomics, and writes go
/// through the concrete heap's `write_at`, which also honours the mapping's
/// protection (a [`FLAG_READ_ONLY`] mapping is `PROT_READ`; a store into it
/// would fault). Any cross-process ordering is the surrounding protocol's
/// business, as with AOSP `unsecurePointer()`
/// ([IMemory.h:78-91](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/IMemory.h;l=78)).
#[derive(Clone, Copy)]
pub struct SharedBytes<'a> {
    region: Region<'a>,
    off: usize,
    len: usize,
}

impl<'a> SharedBytes<'a> {
    pub(crate) fn whole(region: Region<'a>) -> Self {
        Self {
            region,
            off: 0,
            len: region.len(),
        }
    }

    /// Length of the window in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The byte at `i`, or `None` past the end.
    pub fn load(&self, i: usize) -> Option<u8> {
        let mut b = [0u8; 1];
        self.copy_to(i, &mut b).ok().map(|()| b[0])
    }

    /// Copy `dst.len()` bytes starting at `off` (relative to the window).
    /// `BadValue` if the range is out of bounds.
    pub fn copy_to(&self, off: usize, dst: &mut [u8]) -> Result<()> {
        let end = off.checked_add(dst.len()).ok_or(StatusCode::BadValue)?;
        if end > self.len {
            return Err(StatusCode::BadValue);
        }
        self.region.load(self.off + off, dst)
    }

    /// The sub-window `[off, off + len)`, or `None` if out of bounds.
    pub fn slice(&self, off: usize, len: usize) -> Option<SharedBytes<'a>> {
        let end = off.checked_add(len)?;
        if end > self.len {
            return None;
        }
        Some(Self {
            region: self.region,
            off: self.off + off,
            len,
        })
    }

    /// Snapshot of the whole window.
    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = vec![0u8; self.len];
        self.region
            .load(self.off, &mut v)
            .expect("window is within the region");
        v
    }
}

impl std::fmt::Debug for SharedBytes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBytes")
            .field("len", &self.len)
            .finish()
    }
}

/// Server-side representation of a heap. AOSP `IMemoryHeap` is keyed by
/// the heap fd; this trait exposes it as a [`BorrowedFd`] rather than an
/// owned [`std::os::fd::OwnedFd`] so the transaction marshalling can write
/// it into the reply as a bare fd object (AOSP `writeFileDescriptor`, which
/// dups) without taking ownership away from the heap object.
///
/// The fd is a [`BorrowedFd`], not a raw number, because a peer's
/// `HEAP_ID` request makes [`BnMemoryHeap`] dup it into the reply: the
/// borrow is what guarantees the fd is open and belongs to the heap for as
/// long as `&self` lives. A raw number from a safe trait method could name
/// any fd of the process (stdin, or one already closed), and a remote
/// request would then send it to the peer.
///
/// Heap geometry is immutable for the lifetime of the heap: the size and
/// offset are captured at heap construction time and never mutate.
/// Mutation surface is intentionally absent — heap resize is not in AOSP `IMemoryHeap` either
/// ([IMemory.h:41-45](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/IMemory.h;l=41)).
pub trait IMemoryHeap: Send + Sync {
    /// The heap fd, as AOSP `getHeapID()` names it; `None` while the heap
    /// has none (an unmapped [`BpMemoryHeap`]).
    ///
    /// [`BnMemoryHeap`] answers `HEAP_ID` with a dup of this fd, written as
    /// a bare fd object (AOSP `writeFileDescriptor`), and with `BadValue`
    /// when it is `None`.
    fn heap_fd(&self) -> Option<BorrowedFd<'_>>;
    /// AOSP `getHeapID()`: the number of [`heap_fd`](Self::heap_fd), or
    /// `-1` without one. Informational only: the `HEAP_ID` reply is built
    /// from [`heap_fd`](Self::heap_fd), never from this number.
    fn heap_id(&self) -> i32 {
        self.heap_fd().map_or(-1, |fd| fd.as_raw_fd())
    }
    /// AOSP `getSize()`. Total byte length of the heap.
    fn size(&self) -> usize;
    /// AOSP `getFlags()`. Bitmask of `FLAG_READ_ONLY` etc.
    fn flags(&self) -> u32;
    /// AOSP `getOffset()`. Offset within the underlying fd at which
    /// this heap begins; `0` for a freshly-allocated heap.
    fn offset(&self) -> usize;
    /// AOSP `getBase()`. Returns a read-only view of the local mapping if
    /// the heap is currently mapped into this process, else `None`. See
    /// [`SharedBytes`] for why it is neither a `&[u8]` nor writable;
    /// writes go through the concrete heap's `write_at`.
    fn base(&self) -> Option<SharedBytes<'_>>;
}

/// Sub-region of an [`IMemoryHeap`]. AOSP
/// [`IMemory`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/IMemory.h;l=69)
/// equivalent. An `IMemory` references a heap plus an `(offset, size)`
/// pair so that one large heap can host many small allocations (the
/// AOSP `MemoryDealer` pattern).
pub trait IMemory: Send + Sync {
    /// AOSP `getMemory(offset*, size*)`: the backing heap; the window is
    /// [`offset`](Self::offset)/[`size`](Self::size). `&self` borrow keeps
    /// the heap alive for the duration of the returned reference.
    fn memory(&self) -> &dyn IMemoryHeap;
    /// AOSP `offset()`. Offset within the backing heap.
    fn offset(&self) -> usize;
    /// AOSP `size()`. Byte length of this slice. May be smaller than
    /// the backing heap.
    fn size(&self) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `FLAG_READ_ONLY` matches AOSP `IMemoryHeap::READ_ONLY = 0x01`, the rest `MemoryHeapBase.h`.
    #[test]
    fn flags_match_aosp_constants() {
        assert_eq!(FLAG_READ_ONLY, 0x0000_0001);
        assert_eq!(FLAG_DONT_MAP_LOCALLY, 0x0000_0100);
        assert_eq!(FLAG_NO_CACHING, 0x0000_0200);
        assert_eq!(FLAG_FORCE_MEMFD, 0x0000_0400);
        assert_eq!(FLAG_MEMFD_ALLOW_SEALING, 0x0000_0800);
    }

    /// Unsupported targets return `InvalidOperation` instead of panicking, so callers can opt out.
    #[test]
    fn constructor_matches_is_supported() {
        let r = MemoryHeapBase::new(4096, FLAG_READ_ONLY);
        if is_supported() {
            assert!(r.is_ok());
        } else {
            assert_eq!(r.unwrap_err(), crate::StatusCode::InvalidOperation);
        }
    }

    /// `IMemoryHeap` is object-safe (`&dyn` compiles); `heap_id` derives from `heap_fd`.
    #[test]
    fn imemoryheap_is_object_safe() {
        struct Stub(Option<std::fs::File>);
        impl IMemoryHeap for Stub {
            fn heap_fd(&self) -> Option<BorrowedFd<'_>> {
                self.0.as_ref().map(std::os::fd::AsFd::as_fd)
            }
            fn size(&self) -> usize {
                0
            }
            fn flags(&self) -> u32 {
                0
            }
            fn offset(&self) -> usize {
                0
            }
            fn base(&self) -> Option<SharedBytes<'_>> {
                None
            }
        }
        let file = std::fs::File::open("/dev/null").unwrap();
        let raw = file.as_raw_fd();
        let h: &dyn IMemoryHeap = &Stub(Some(file));
        assert_eq!(h.heap_id(), raw);
        assert_eq!(Stub(None).heap_id(), -1);
    }
}
