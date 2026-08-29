// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Shared-memory IPC: AOSP `IMemory` / `IMemoryHeap` for rsbinder
//! ([plan/4-7a](https://github.com/hiking90/rsbinder/blob/master/plans/4-7a-shared-memory-impl.md)).
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
//!   See [`heap`](crate::shared_memory::heap) for the backing-store table and the aliasing
//!   contract.
//! * [`MemoryDealer`](crate::shared_memory::MemoryDealer) — AOSP's chunk
//!   allocator: one heap, many `IMemory` windows, with
//!   [`HeapCache`](crate::shared_memory::HeapCache) on the receiving side
//!   so the heap is mapped once. See [`dealer`](crate::shared_memory::dealer).
//!
//! A shared region travels over any transport that can carry an fd:
//! the kernel binder (`BINDER_TYPE_FD`) and Unix-socket RPC with
//! `rpc::FileDescriptorTransportMode::Unix` (the `rpc` feature)
//! negotiated on both ends. TCP / vsock / TLS sessions cannot carry
//! fds at all; writing a heap fd into such a parcel fails with
//! `StatusCode::BadType` exactly as a plain
//! [`ParcelFileDescriptor`](crate::ParcelFileDescriptor) does.
//!
//! Targets without a backing store (anything other than Linux,
//! Android, macOS) compile the whole trait surface but every
//! constructor returns `Err(StatusCode::InvalidOperation)`; check
//! [`is_supported`](crate::shared_memory::is_supported) to branch at runtime.

pub mod dealer;
pub mod heap;
pub mod shared;
pub mod wire;

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
/// mapping from a peer that ignores the flag; the owner's own mapping,
/// created before the seal, stays writable.
pub const FLAG_READ_ONLY: u32 = 0x0000_0001;

pub(crate) const WORD: usize = std::mem::size_of::<usize>();

/// A mapped shared region, split into its `usize`-aligned words and the
/// `< WORD` trailing bytes. Every access is word-sized on the word part
/// (partial words go through a CAS) and byte-sized on the tail: Rust's
/// memory model makes overlapping atomic accesses of *different* sizes a
/// data race when one is a write, so a byte-granular fast path here would
/// race the word path of another thread writing the same window.
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

    /// Copy `src` in at `off` (relaxed stores; a partial word is merged
    /// with a CAS so the neighbouring bytes of another writer survive).
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
/// the heap fd; this trait deliberately exposes the fd as a borrowed
/// raw fd (`i32`) rather than an owned [`std::os::fd::OwnedFd`] so the
/// transaction marshalling can dup the fd into a
/// [`crate::ParcelFileDescriptor`] without taking ownership away from
/// the heap object.
///
/// All methods return `&` borrows (heap geometry is immutable for the
/// lifetime of the heap); the size and offset are captured at heap
/// construction time and never mutate. Mutation surface is intentionally
/// absent — heap resize is not in AOSP `IMemoryHeap` either
/// ([IMemory.h:41-45](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/IMemory.h;l=41)).
pub trait IMemoryHeap: Send + Sync {
    /// AOSP `getHeapID()`. Returns the fd-as-i32 for parcel marshalling
    /// (wrapped in `ParcelFileDescriptor` on the wire).
    fn heap_id(&self) -> i32;
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
    /// AOSP `getMemory(offset*, size*)`. Returns the backing heap plus
    /// the in-heap offset and size for this slice. `&self` borrow keeps
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

    /// `FLAG_READ_ONLY` matches AOSP `IMemoryHeap::READ_ONLY = 0x01`;
    /// the `MemoryHeapBase.h` flags likewise.
    #[test]
    fn flags_match_aosp_constants() {
        assert_eq!(FLAG_READ_ONLY, 0x0000_0001);
        assert_eq!(FLAG_DONT_MAP_LOCALLY, 0x0000_0100);
        assert_eq!(FLAG_NO_CACHING, 0x0000_0200);
        assert_eq!(FLAG_FORCE_MEMFD, 0x0000_0400);
        assert_eq!(FLAG_MEMFD_ALLOW_SEALING, 0x0000_0800);
    }

    /// Unsupported targets signal "not implemented" rather than
    /// panicking, so caller code can opt out gracefully.
    #[test]
    fn constructor_matches_is_supported() {
        let r = MemoryHeapBase::new(4096, FLAG_READ_ONLY);
        if is_supported() {
            assert!(r.is_ok());
        } else {
            assert_eq!(r.unwrap_err(), crate::StatusCode::InvalidOperation);
        }
    }

    /// The trait surface itself is object-safe — we can hold an
    /// `&dyn IMemoryHeap`.
    #[test]
    fn imemoryheap_is_object_safe() {
        struct Stub;
        impl IMemoryHeap for Stub {
            fn heap_id(&self) -> i32 {
                42
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
        let h: &dyn IMemoryHeap = &Stub;
        assert_eq!(h.heap_id(), 42);
    }
}
