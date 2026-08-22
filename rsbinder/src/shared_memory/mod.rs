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
//!
//! A shared region travels over any transport that can carry an fd:
//! the kernel binder (`BINDER_TYPE_FD`) and Unix-socket RPC with
//! [`FileDescriptorTransportMode::Unix`](crate::rpc::FileDescriptorTransportMode)
//! negotiated on both ends. TCP / vsock / TLS sessions cannot carry
//! fds at all; writing a heap fd into such a parcel fails with
//! `StatusCode::BadType` exactly as a plain
//! [`ParcelFileDescriptor`](crate::ParcelFileDescriptor) does.
//!
//! Targets without a backing store (anything other than Linux,
//! Android, macOS) compile the whole trait surface but every
//! constructor returns `Err(StatusCode::InvalidOperation)`; check
//! [`is_supported`](crate::shared_memory::is_supported) to branch at runtime.

pub mod heap;
pub mod shared;
pub mod wire;

pub use heap::{
    is_supported, MappedHeap, MemoryHeapBase, FLAG_DONT_MAP_LOCALLY, FLAG_FORCE_MEMFD,
    FLAG_MEMFD_ALLOW_SEALING, FLAG_NO_CACHING, SEAL_FUTURE_WRITE, SEAL_GROW, SEAL_SEAL,
    SEAL_SHRINK, SEAL_WRITE,
};
pub use shared::{region_size, SharedMemory};
pub use wire::{
    export_heap, BnMemory, BnMemoryHeap, BpMemory, BpMemoryHeap, MemoryBase, GET_MEMORY, HEAP_ID,
    IMEMORY_DESCRIPTOR, IMEMORY_HEAP_DESCRIPTOR,
};

/// AOSP `IMemoryHeap::READ_ONLY` flag
/// ([IMemory.h:37-39](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/IMemory.h;l=37)).
/// Receivers map `PROT_READ` only. On Linux/Android the owner also
/// applies `F_SEAL_FUTURE_WRITE` so the kernel refuses a writable
/// mapping from a peer that ignores the flag; the owner's own mapping,
/// created before the seal, stays writable.
pub const FLAG_READ_ONLY: u32 = 0x0000_0001;

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
    /// AOSP `getBase()`. Returns the local mapping if the heap is
    /// currently mapped into this process, else `None`.
    ///
    /// **Aliasing contract.** The slice aliases memory that other
    /// processes may write concurrently; holding it is sound only while
    /// no other party writes to the region (single-writer discipline
    /// established by the surrounding protocol). Prefer the copying
    /// accessors `read_at` / `write_at` on the concrete heap types — see
    /// AOSP `unsecurePointer()`
    /// ([IMemory.h:78-91](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/IMemory.h;l=78)).
    fn base(&self) -> Option<&[u8]>;
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
            fn base(&self) -> Option<&[u8]> {
                None
            }
        }
        let h: &dyn IMemoryHeap = &Stub;
        assert_eq!(h.heap_id(), 42);
    }
}
