// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use rustix::fd::{BorrowedFd, FromRawFd, OwnedFd};

pub(crate) use crate::sys::binder::flat_binder_object;
use crate::{binder::*, error::*, process_state, sys::*};

impl Default for flat_binder_object {
    /// Creates a new flat_binder_object with safe default values.
    ///
    /// This provides a safe alternative to `std::mem::zeroed()` which can be
    /// undefined behavior for some types. All fields are explicitly initialized
    /// to known safe values.
    fn default() -> Self {
        flat_binder_object {
            hdr: binder_object_header {
                type_: BINDER_TYPE_BINDER,
            },
            flags: 0,
            __bindgen_anon_1: flat_binder_object__bindgen_ty_1 { binder: 0 },
            cookie: 0,
        }
    }
}

impl flat_binder_object {
    pub(crate) fn new_with_fd(fd: i32, take_ownership: bool) -> Self {
        flat_binder_object {
            hdr: binder_object_header {
                type_: BINDER_TYPE_FD,
            },
            // AOSP `Parcel::writeFileDescriptor` (kernel arm) sets `obj.flags = 0`
            // for a `BINDER_TYPE_FD` object: it bypasses `flattenBinder`, so neither
            // schedBits nor `ACCEPTS_FDS` apply. The kernel ignores this field for FD
            // objects (it rewrites the object on delivery), but match AOSP exactly.
            flags: 0,
            // Init via the 8-byte `binder` field (not the u32 `handle`) so the upper bytes
            // are zeroed: mirrors AOSP `obj.binder = 0; obj.handle = fd;`, avoids leaking
            // uninit stack to the remote.
            __bindgen_anon_1: flat_binder_object__bindgen_ty_1 {
                binder: (fd as u32) as u64,
            },
            cookie: if take_ownership { 1 } else { 0 },
        }
    }

    /// Creates a new flat_binder_object for a binder with the specified flags.
    /// This is a safe alternative to using Default::default() and manually setting flags.
    pub(crate) fn new_binder_with_flags(flags: u32) -> Self {
        flat_binder_object {
            hdr: binder_object_header {
                type_: BINDER_TYPE_BINDER,
            },
            flags,
            __bindgen_anon_1: flat_binder_object__bindgen_ty_1 { binder: 0 },
            cookie: 0,
        }
    }

    /// Creates a new flat_binder_object for a remote handle
    /// (`BINDER_TYPE_HANDLE`).
    pub(crate) fn new_handle(handle: u32, flags: u32) -> Self {
        flat_binder_object {
            hdr: binder_object_header {
                type_: BINDER_TYPE_HANDLE,
            },
            flags,
            // Init via the 8-byte `binder` field (not the u32 `handle`) so the
            // upper bytes are zeroed: mirrors AOSP `obj.binder = 0; obj.handle =
            // handle;` and avoids leaking uninit stack to the remote (the whole
            // 24-byte struct is copied onto the wire by `write_object`).
            __bindgen_anon_1: flat_binder_object__bindgen_ty_1 {
                binder: handle as u64,
            },
            cookie: 0,
        }
    }

    pub(crate) fn header_type(&self) -> u32 {
        self.hdr.type_
    }

    pub(crate) fn handle(&self) -> u32 {
        // SAFETY: `__bindgen_anon_1` is an integer union (`binder: u64` |
        // `handle: u32`); every bit pattern is a valid value for both
        // variants, so the read itself is never UB. Reading `.handle` is
        // meaningful only for handle/FD-typed objects — that selection is
        // the caller's contract per `hdr.type`.
        unsafe { self.__bindgen_anon_1.handle }
    }

    pub(crate) fn borrowed_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: caller invariant — only called on a BINDER_TYPE_FD object
        // whose fd is kept alive by the owning parcel for the returned
        // borrow's lifetime (tied to `&self`).
        unsafe { BorrowedFd::borrow_raw(self.handle() as _) }
    }

    pub(crate) fn owned_fd(&self) -> OwnedFd {
        // SAFETY: caller invariant — only called on a BINDER_TYPE_FD object
        // that owns its fd, and at most once, so the resulting OwnedFd has
        // exclusive ownership and will not double-close.
        unsafe { OwnedFd::from_raw_fd(self.handle() as _) }
    }

    pub(crate) fn set_handle(&mut self, handle: u32) {
        self.__bindgen_anon_1.handle = handle
    }

    pub(crate) fn pointer(&self) -> binder_uintptr_t {
        // SAFETY: integer union read (see `handle`); never UB. Meaningful
        // only for BINDER_TYPE_(WEAK_)BINDER objects — caller's contract.
        unsafe { self.__bindgen_anon_1.binder }
    }

    pub(crate) fn set_cookie(&mut self, cookie: binder_uintptr_t) {
        self.cookie = cookie;
    }

    pub(crate) fn acquire(&self) -> Result<()> {
        match self.hdr.type_ {
            BINDER_TYPE_BINDER => {
                // Native binder: bump publish_count for this buffer
                // instance. Symmetric with `release()` below — every
                // `Parcel::write_object` / `Parcel::append_from` call
                // pairs an `acquire` here with exactly one `release`
                // from `Parcel::release_objects` (driven by
                // `Parcel::Drop` for caller-owned outgoing parcels).
                // Driver-mmapped incoming parcels skip both ends
                // symmetrically (their `Drop` calls `BC_FREE_BUFFER`
                // instead of `release_objects`, and the deserializer
                // does not call `acquire`), so the pairing invariant
                // is preserved without any per-object bookkeeping.
                if self.pointer() != 0 {
                    let id = self.pointer();
                    if !process_state::ProcessState::as_self().incref_publish(id) {
                        log::error!("flat_binder_object::acquire: unknown native id {id}");
                        debug_assert!(false, "acquire on unknown native id {id}");
                    }
                }

                Ok(())
            }
            BINDER_TYPE_HANDLE => process_state::ProcessState::as_self()
                .strong_proxy_for_handle(self.handle())?
                .increase(),
            BINDER_TYPE_FD => {
                // Notion to do.
                Ok(())
            }
            _ => {
                log::error!("Invalid object type {:08x}", self.hdr.type_);
                Err(StatusCode::InvalidOperation)
            }
        }
    }

    pub(crate) fn release(&self) -> Result<()> {
        match self.hdr.type_ {
            BINDER_TYPE_BINDER => {
                // Native binder: decrement publish_count. If both
                // publish_count and kernel_refs hit zero,
                // decref_publish removes the entry, which drives
                // RefCounter.strong / RefCounter.weak 1→0 and drops
                // the canonical Arc<dyn IBinder> (Inner<T>::drop runs
                // cleanly — kernel guaranteed no further BR_* will
                // reference this id since kernel_refs was 0 at
                // removal).
                if self.pointer() != 0 {
                    let id = self.pointer();
                    if !process_state::ProcessState::as_self().decref_publish(id) {
                        log::error!("flat_binder_object::release: unknown native id {id}");
                        debug_assert!(false, "release on unknown native id {id}");
                    }
                }
                Ok(())
            }
            BINDER_TYPE_HANDLE => process_state::ProcessState::as_self()
                .strong_proxy_for_handle(self.handle())?
                .decrease(),
            BINDER_TYPE_FD => {
                if self.cookie != 0 {
                    // Get owned fd and close it.
                    self.owned_fd();
                }

                Ok(())
            }
            _ => {
                log::error!("Invalid object type {:08x}", self.hdr.type_);
                Err(StatusCode::InvalidOperation)
            }
        }
    }
}

const SCHED_NORMAL: u32 = 0;
const FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT: u32 = 9;
/// 2-bit field for the scheduling policy embedded in `flat_binder_object.flags`.
/// The AOSP-canonical post-shift mask is `FLAT_BINDER_FLAG_SCHED_POLICY_MASK = 0x600`
/// (= this value `<< FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT`); the constant here is
/// the pre-shift value mask used to clamp callers (`policy` must be 0..=3).
/// Naming `_VALUE_MASK` (rather than `_MASK`) avoids colliding with the post-shift
/// `_MASK` in AOSP `binder.h`.
const FLAT_BINDER_FLAG_SCHED_POLICY_VALUE_MASK: u32 = 0x3;

fn sched_policy_mask(policy: u32, priority: u32) -> u32 {
    (priority & FLAT_BINDER_FLAG_PRIORITY_MASK)
        | ((policy & FLAT_BINDER_FLAG_SCHED_POLICY_VALUE_MASK)
            << FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT)
}

impl From<&SIBinder> for flat_binder_object {
    fn from(binder: &SIBinder) -> Self {
        let sched_bits = if !process_state::ProcessState::as_self().background_scheduling_disabled()
        {
            sched_policy_mask(SCHED_NORMAL, 19)
        } else {
            0
        };

        if let Some(proxy) = binder.as_proxy() {
            // AOSP-correct: `flattenBinder`'s HANDLE arm sets `obj.flags = 0`,
            // then applies `obj.flags |= schedBits` after both arms, so the
            // final value is `0 | schedBits` == `sched_bits`. Do not "fix" to 0.
            flat_binder_object::new_handle(proxy.handle(), sched_bits)
        } else {
            // Native binder. Acquire (or dedup-resolve) an id via the
            // sidecar table on `ProcessState`; the table holds an
            // `Arc<dyn IBinder>` strong reference for the duration
            // either an outgoing parcel (`publish_count > 0`) or any
            // kernel-held ref (`kernel_refs > 0`) references this
            // binder. Replaces the previous fat-pointer encoding
            // (data ptr in `binder`, vtable ptr in `cookie`) which
            // could dangle once `Inner<T>` was dropped while a
            // `BR_DECREFS` was still in flight — Android closes the
            // same window with a two-allocation
            // (`weakref_type*` / `BBinder*`) design; we reach the
            // same invariant via id-indirection.
            //
            // The entry is created with `publish_count = 0`; the
            // immediately-following `Parcel::write_object` →
            // `flat_binder_object::acquire` brings it to 1. The
            // single-statement window between this `From` returning
            // and the first `acquire` is the only leak path under a
            // `Parcel::write_aligned` panic (typically OOM), which is
            // process-fatal anyway.
            let id =
                process_state::ProcessState::as_self().publish_native(Arc::clone(binder.as_arc()));

            // AOSP `Parcel.cpp::flattenBinder`: the scheduler priority /
            // policy bits come from a SINGLE source. An explicit min
            // priority/policy on the binder (any non-zero bit in those
            // ranges — matching AOSP's `policy != 0 || priority != 0`
            // test) OVERRIDES the default node priority instead of being
            // OR-combined with it ("override value, since it is set
            // explicitly"). Blindly OR-ing `sched_bits` over
            // `local_binder_flags()` would corrupt the requested priority
            // (e.g. requested 5 | default 19 = 23).
            let local = binder.local_binder_flags();
            let sched_mask = FLAT_BINDER_FLAG_PRIORITY_MASK
                | (FLAT_BINDER_FLAG_SCHED_POLICY_VALUE_MASK << FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT);
            let effective_sched = if local & sched_mask != 0 {
                local & sched_mask
            } else {
                sched_bits
            };

            flat_binder_object {
                hdr: binder_object_header {
                    type_: BINDER_TYPE_BINDER,
                },
                flags: (local & !sched_mask) | effective_sched,
                __bindgen_anon_1: flat_binder_object__bindgen_ty_1 { binder: id },
                cookie: 0,
            }
        }
    }
}

/// Reads a flat_binder_object from a potentially unaligned buffer position.
///
/// Parcel buffers use 4-byte alignment, but flat_binder_object requires 8-byte alignment
/// due to its u64 fields. Using read_unaligned avoids alignment UB and returns a stack copy,
/// which also eliminates lifetime soundness issues from the previous transmute approach.
pub(crate) fn read_flat_binder(data: &[u8], offset: usize) -> Result<flat_binder_object> {
    let size = std::mem::size_of::<flat_binder_object>();
    let bytes = data
        .get(offset..offset + size)
        .ok_or(StatusCode::NotEnoughData)?;
    // SAFETY: `get(offset..offset + size)` guarantees `bytes` is exactly
    // `size_of::<flat_binder_object>()` readable bytes. `flat_binder_object`
    // is a bindgen `#[repr(C)]` POD (no invalid bit patterns), so any byte
    // pattern is a valid value; `read_unaligned` covers the unknown
    // alignment of the parcel offset and returns an owned stack copy.
    Ok(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const flat_binder_object) })
}

/// Writes a flat_binder_object to a potentially unaligned buffer position.
pub(crate) fn write_flat_binder(
    data: &mut [u8],
    offset: usize,
    obj: &flat_binder_object,
) -> Result<()> {
    let size = std::mem::size_of::<flat_binder_object>();
    let bytes = data
        .get_mut(offset..offset + size)
        .ok_or(StatusCode::NotEnoughData)?;
    // SAFETY: `get_mut(offset..offset + size)` guarantees `bytes` is exactly
    // `size_of::<flat_binder_object>()` writable bytes. `*obj` is a valid
    // `flat_binder_object`; `write_unaligned` covers the unknown alignment
    // of the parcel offset.
    unsafe { std::ptr::write_unaligned(bytes.as_mut_ptr() as *mut flat_binder_object, *obj) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard for `new_with_fd`.
    ///
    /// `flags` must be `0`: AOSP `Parcel::writeFileDescriptor` (kernel arm)
    /// writes `obj.flags = 0` for a `BINDER_TYPE_FD` object — it bypasses
    /// `flattenBinder`, so no schedBits / `ACCEPTS_FDS`. An earlier change set
    /// `0x7F | ACCEPTS_FDS` (= `0x17F`) and this test asserted it as
    /// "byte-identical to AOSP" — it was not. The kernel ignores the field for
    /// FD objects so nothing broke functionally, but it diverged on the wire.
    /// Like the ParcelableHolder stability case, an rsbinder<->rsbinder round
    /// trip cannot catch a wrong-but-symmetric flags value, so this asserts the
    /// AOSP golden byte (`0`) directly.
    ///
    /// Also guards the full-width union init: the 8-byte `binder` field must be
    /// written (not the u32 `handle` variant) so the upper 4 bytes are zero and
    /// no uninitialized stack leaks to the remote.
    #[test]
    fn new_with_fd_flags_zero_and_full_width_init() {
        let fd: i32 = 7;
        let obj = flat_binder_object::new_with_fd(fd, false);

        assert_eq!(obj.header_type(), BINDER_TYPE_FD, "must be a FD object");

        // AOSP writeFileDescriptor sets flags = 0 for FD objects.
        assert_eq!(
            obj.flags, 0,
            "FD object flags must be 0 (AOSP Parcel::writeFileDescriptor)"
        );

        // The full 8-byte union must equal exactly `fd` with a zeroed upper
        // half — no uninitialized stack bytes leaked.
        assert_eq!(
            obj.pointer(),
            fd as u32 as u64,
            "upper 32 bits of the union must be zero (uninit-leak UB regression)"
        );
        assert_eq!(
            obj.handle(),
            fd as u32,
            "handle variant must round-trip the fd"
        );
    }

    /// Regression guard for the `From<&SIBinder>` proxy arm, which builds its
    /// HANDLE object via [`flat_binder_object::new_handle`].
    ///
    /// The 8-byte `binder` union field must be written (not the u32 `handle`
    /// variant) so the upper 4 bytes are zero — AOSP `flattenBinder` does
    /// `obj.binder = 0; obj.handle = handle`. Writing only the `handle` variant
    /// leaves the upper half uninitialized, and `write_object` copies the whole
    /// struct onto the wire (uninitialized-read UB + a 4-byte stack info leak to
    /// the peer). An rsbinder<->rsbinder round trip cannot catch this because
    /// both sides ignore the upper bytes, so assert the union directly.
    #[test]
    fn new_handle_full_width_init() {
        let handle: u32 = 0xDEAD_BEEF;
        let obj = flat_binder_object::new_handle(handle, 0);

        assert_eq!(
            obj.header_type(),
            BINDER_TYPE_HANDLE,
            "must be a HANDLE object"
        );
        assert_eq!(
            obj.pointer(),
            handle as u64,
            "upper 32 bits of the union must be zero (uninit-leak UB regression)"
        );
        assert_eq!(
            obj.handle(),
            handle,
            "handle variant must round-trip the handle"
        );
    }

    #[test]
    fn new_with_fd_cookie_tracks_take_ownership() {
        assert_eq!(flat_binder_object::new_with_fd(3, true).cookie, 1);
        assert_eq!(flat_binder_object::new_with_fd(3, false).cookie, 0);
    }
}
