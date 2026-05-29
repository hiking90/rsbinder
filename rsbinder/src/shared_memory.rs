// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Shared-memory IPC trait skeleton.
//!
//! AOSP `IMemory` / `IMemoryHeap`
//! ([`IMemory.h`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/IMemory.h))
//! are **handwritten** C++ binders, not AIDL. The Rust traits in this
//! module mirror that surface for in-process use. Not yet implemented,
//! left as future work:
//!
//! * A `memfd_create(2)` + `F_ADD_SEALS` backed `MemoryHeapBase`
//!   (Linux/Android only). Receiver-side `F_GET_SEALS` verification is
//!   mandatory for the read-only PROT negotiation.
//! * A `MemoryDealer` chunk allocator on top of a heap.
//! * Hermetic verification that two processes can mmap the same page
//!   through `ParcelFileDescriptor` transfer.
//!
//! Interop with real `IMemory`/`IMemoryHeap` peers is out of scope — it
//! would require manual `Bn`/`Bp` matching AOSP's handwritten transaction
//! codes (`HEAP_ID_TRANSACTION` etc.).
//!
//! The macOS host build compiles every trait declaration in this module
//! but the concrete `MemoryHeapBase` stub returns
//! `Err(StatusCode::InvalidOperation)` from every operation — there is no
//! `memfd_create` equivalent on darwin. The Linux/Android impl, once
//! added, is gated on `cfg(any(target_os = "linux", target_os = "android"))`.

use crate::error::{Result, StatusCode};

/// AOSP `IMemoryHeap::READ_ONLY` flag
/// ([IMemory.h:37-39](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/IMemory.h;l=37)).
/// Receivers MUST honor this by mapping `PROT_READ` only and rejecting
/// any subsequent `mprotect(PROT_WRITE)`. The PROT negotiation also
/// requires the sender to enforce `F_SEAL_WRITE` on the underlying
/// `memfd` so the kernel rejects a mismatched mmap from a peer that
/// ignores this flag.
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
    /// AOSP `getBase()`. Returns the local mmap base pointer if the
    /// heap is currently mapped into this process, else `None`.
    /// A real `MemoryHeapBase` would populate this; the macOS stub
    /// always returns `None`.
    ///
    /// **Safety contract.** The returned slice is valid only for the
    /// lifetime of the heap (`&self`) and only points to memory mapped
    /// by *this* process — see AOSP `unsecurePointer()` security note
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

/// Concrete heap allocator. Currently only the public surface is
/// present; the actual `memfd_create` + `F_ADD_SEALS` implementation is
/// future work, gated on Linux/Android.
///
/// On macOS this is a *compile-only* stub: every constructor returns
/// `Err(StatusCode::InvalidOperation)` so callers can write
/// `cfg`-portable code that depends on the trait surface but doesn't
/// actually rely on shared memory backing existing.
#[derive(Debug)]
pub struct MemoryHeapBase {
    // `_` prefix suppresses dead-code until the real impl fills these.
    _size: usize,
    _flags: u32,
    _offset: usize,
}

impl MemoryHeapBase {
    /// macOS / Linux / Android constructor stub. Returns
    /// `Err(StatusCode::InvalidOperation)` until the
    /// `memfd_create`-backed impl lands (Linux/Android cfg) — on macOS
    /// it remains permanent (`Err(InvalidOperation)`).
    pub fn new(size: usize, flags: u32) -> Result<Self> {
        // Suppress unused: keep them in the signature so callers see
        // the AOSP-faithful surface from day 1.
        let _ = (size, flags);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // For now even Linux/Android returns `InvalidOperation`; the
            // real impl will call `memfd_create(2)` + map here.
            Err(StatusCode::InvalidOperation)
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            Err(StatusCode::InvalidOperation)
        }
    }
}

impl IMemoryHeap for MemoryHeapBase {
    fn heap_id(&self) -> i32 {
        -1
    }
    fn size(&self) -> usize {
        self._size
    }
    fn flags(&self) -> u32 {
        self._flags
    }
    fn offset(&self) -> usize {
        self._offset
    }
    fn base(&self) -> Option<&[u8]> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The public trait + struct surface compiles on the
    /// host tree, including the macOS host where `memfd_create` is
    /// absent. The stub constructor signals "not implemented" rather
    /// than panicking, so caller code can opt out gracefully when
    /// shared memory is unavailable.
    #[test]
    fn macos_stub_constructor_signals_not_implemented() {
        let err = MemoryHeapBase::new(4096, FLAG_READ_ONLY).unwrap_err();
        assert_eq!(err, StatusCode::InvalidOperation);
    }

    /// `FLAG_READ_ONLY` matches AOSP `IMemoryHeap::READ_ONLY = 0x01`.
    #[test]
    fn flag_read_only_matches_aosp_constant() {
        assert_eq!(FLAG_READ_ONLY, 0x0000_0001);
    }

    /// The trait surface itself is object-safe — we can hold an
    /// `&dyn IMemoryHeap`. The actual impl is exercised in hermetic
    /// tests once the memfd backend exists.
    #[test]
    fn imemoryheap_is_object_safe() {
        // Trivial impl: zero-sized heap with no base mapping.
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
