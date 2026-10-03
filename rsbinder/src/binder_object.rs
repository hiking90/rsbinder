// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `flat_binder_object` construction, field access and reference counting.
//!
//! # Native binder ids
//!
//! A native binder goes on the wire as an id from the sidecar table on
//! `ProcessState` (`publish_native` acquires or dedup-resolves it), not as a
//! pointer. The table holds an `Arc<dyn IBinder>` strong reference for as long
//! as an outgoing parcel (`publish_count > 0`) or a kernel-held reference
//! (`kernel_refs > 0`) names the binder. A fat-pointer encoding (data pointer
//! in `binder`, vtable pointer in `cookie`) could dangle once `Inner<T>` was
//! dropped while a `BR_DECREFS` was still in flight; AOSP closes the same
//! window with its two-allocation `weakref_type*` / `BBinder*` design, and
//! rsbinder reaches the same invariant through the id indirection.
//!
//! - `publish_native` creates the entry with `publish_count = 0`; the
//!   `Parcel::write_object` → `acquire` that immediately follows brings it to 1.
//!   Between the two, `Parcel::write_aligned_data` can leave the entry unacquired
//!   (`publish_count = 0`, `kernel_refs = 0`, still holding `Inner<T>`): by a
//!   panic (typically OOM), or by `Err(BadValue)` when the write would end
//!   past `i32::MAX` or `Err(PermissionDenied)` when it would overlap a
//!   recorded object (both reachable through `set_data_position`). That entry
//!   is not reclaimed.
//! - Every `Parcel::write_object` / `Parcel::append_from` call pairs one
//!   `acquire` with exactly one `release` from `Parcel::release_objects`, driven
//!   by `Parcel::Drop` for caller-owned outgoing parcels. Driver-mmapped
//!   incoming parcels skip both ends: their `Drop` sends `BC_FREE_BUFFER`
//!   instead of calling `release_objects`, and the deserializer does not call
//!   `acquire`. The pairing therefore needs no per-object bookkeeping.
//! - `release` decrements `publish_count`. When `publish_count`,
//!   `kernel_refs` and `pending_reservations` are all zero, `decref_publish`
//!   removes the entry, which drives
//!   `RefCounter.strong` / `RefCounter.weak` 1→0 and drops the canonical
//!   `Arc<dyn IBinder>`. `Inner<T>::drop` then runs cleanly: since
//!   `kernel_refs` was 0 at removal, the kernel sends no further `BR_*` naming
//!   the id.
//!
//! # Scheduler bits
//!
//! As in AOSP `Parcel.cpp::flattenBinder`, the priority / policy bits come
//! from a single source. For a native binder, an explicit min priority or
//! policy (any non-zero bit in those ranges, AOSP's
//! `policy != 0 || priority != 0` test) overrides the default node priority
//! instead of being OR-combined with it; OR-ing `sched_bits` over
//! `local_binder_flags()` would corrupt the requested priority (requested
//! 5 | default 19 = 23). For a proxy, `flattenBinder`'s HANDLE arm sets
//! `obj.flags = 0` and ORs in `schedBits` after both arms, so the final value
//! is `sched_bits`, not 0.
//!
//! The policy is a 2-bit field in `flat_binder_object.flags`. AOSP
//! `binder.h` defines the post-shift mask `FLAT_BINDER_FLAG_SCHED_POLICY_MASK
//! = 0x600` (the value mask `<< FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT`); the
//! crate clamps callers with the pre-shift value mask `0x3`, named
//! `FLAT_BINDER_FLAG_SCHED_POLICY_VALUE_MASK` so it does not collide with the
//! AOSP post-shift name.
//!
//! # Layout and the union
//!
//! `FlatBinderObject` is the UAPI `struct flat_binder_object` held as plain
//! fields, host-native (the driver parses it): the `__u32` header type, the
//! `__u32` flags, the 8-byte `binder`/`handle` union, and the 8-byte cookie,
//! at offsets 0, 4, 8 and 16 of 24 bytes. `to_bytes`/`from_bytes` copy those
//! fields, and compile-time asserts tie the offsets to the bindgen struct.
//!
//! The union is kept as its 8 bytes. `binder` is all of them; `handle` is the
//! first four, so `set_handle` zeroes the eight and then writes the `u32` over
//! the first four, as AOSP does (`obj.binder = 0; obj.handle = ...;`). That
//! puts the handle where the driver reads it on either byte order, and the
//! upper four bytes go out as zeros rather than stale data.
//!
//! `new_with_fd` also writes `flags = 0` as AOSP `Parcel::writeFileDescriptor`
//! (kernel arm) does; the kernel ignores the field for FD objects (it rewrites
//! the object on delivery), but the bytes match AOSP exactly. A value such as
//! `0x7F | ACCEPTS_FDS` (`0x17F`) breaks nothing functionally but diverges from
//! AOSP, whose `writeFileDescriptor` bypasses `flattenBinder` and so writes no
//! sched bits or `ACCEPTS_FDS`. An rsbinder↔rsbinder round trip cannot catch
//! that or a misplaced handle, since both sides agree with themselves, so
//! `new_with_fd_flags_zero_and_full_width_init` and `new_handle_full_width_init`
//! (which covers the `From<&SIBinder>` proxy arm) assert the AOSP bytes
//! directly.

use std::sync::Arc;

use crate::sys::binder::{flat_binder_object, flat_binder_object__bindgen_ty_1};
use crate::{binder::*, error::*, process_state, sys::*};

const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<flat_binder_object>() == FlatBinderObject::SIZE);
    assert!(offset_of!(flat_binder_object, flags) == 4);
    assert!(offset_of!(flat_binder_object, __bindgen_anon_1) == 8);
    assert!(size_of::<flat_binder_object__bindgen_ty_1>() == 8);
    assert!(offset_of!(flat_binder_object, cookie) == 16);
};

/// UAPI `struct flat_binder_object`, host-native; see module doc "Layout and the union".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FlatBinderObject {
    type_: u32,
    pub(crate) flags: u32,
    /// The `binder`/`handle` union as its bytes: `handle` is the first four.
    object: [u8; 8],
    pub(crate) cookie: binder_uintptr_t,
}

impl Default for FlatBinderObject {
    fn default() -> Self {
        Self::new_binder_with_flags(0)
    }
}

impl FlatBinderObject {
    /// `sizeof(struct flat_binder_object)`.
    pub(crate) const SIZE: usize = 24;

    pub(crate) fn new_with_fd(fd: i32, take_ownership: bool) -> Self {
        let mut obj = FlatBinderObject {
            type_: BINDER_TYPE_FD,
            // AOSP `writeFileDescriptor` bypasses `flattenBinder`: no schedBits, no ACCEPTS_FDS.
            flags: 0,
            object: [0; 8],
            cookie: if take_ownership { 1 } else { 0 },
        };
        obj.set_handle(fd as u32);
        obj
    }

    /// Creates a new flat_binder_object for a binder with the specified flags.
    pub(crate) fn new_binder_with_flags(flags: u32) -> Self {
        FlatBinderObject {
            type_: BINDER_TYPE_BINDER,
            flags,
            object: [0; 8],
            cookie: 0,
        }
    }

    /// Creates a new flat_binder_object for a remote handle (`BINDER_TYPE_HANDLE`).
    pub(crate) fn new_handle(handle: u32, flags: u32) -> Self {
        let mut obj = FlatBinderObject {
            type_: BINDER_TYPE_HANDLE,
            flags,
            object: [0; 8],
            cookie: 0,
        };
        obj.set_handle(handle);
        obj
    }

    /// The 24 bytes the driver parses, host-native.
    pub(crate) fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0; Self::SIZE];
        bytes[0..4].copy_from_slice(&self.type_.to_ne_bytes());
        bytes[4..8].copy_from_slice(&self.flags.to_ne_bytes());
        bytes[8..16].copy_from_slice(&self.object);
        bytes[16..24].copy_from_slice(&self.cookie.to_ne_bytes());
        bytes
    }

    /// The object in 24 host-native bytes; any bytes are a value.
    pub(crate) fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        // Fixed ranges of a 24-byte array, so every `try_into` succeeds.
        FlatBinderObject {
            type_: u32::from_ne_bytes(bytes[0..4].try_into().unwrap()),
            flags: u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            object: bytes[8..16].try_into().unwrap(),
            cookie: binder_uintptr_t::from_ne_bytes(bytes[16..24].try_into().unwrap()),
        }
    }

    /// The bindgen struct, for an ioctl that takes one (`BINDER_SET_CONTEXT_MGR_EXT`).
    pub(crate) fn to_uapi(self) -> flat_binder_object {
        flat_binder_object {
            hdr: binder_object_header { type_: self.type_ },
            flags: self.flags,
            // `binder` is all eight bytes, so this writes the union exactly as held.
            __bindgen_anon_1: flat_binder_object__bindgen_ty_1 {
                binder: self.pointer(),
            },
            cookie: self.cookie,
        }
    }

    pub(crate) fn header_type(&self) -> u32 {
        self.type_
    }

    pub(crate) fn handle(&self) -> u32 {
        let [h0, h1, h2, h3, ..] = self.object;
        u32::from_ne_bytes([h0, h1, h2, h3])
    }

    pub(crate) fn set_handle(&mut self, handle: u32) {
        self.object = [0; 8];
        self.object[..4].copy_from_slice(&handle.to_ne_bytes());
    }

    pub(crate) fn pointer(&self) -> binder_uintptr_t {
        binder_uintptr_t::from_ne_bytes(self.object)
    }

    fn set_pointer(&mut self, pointer: binder_uintptr_t) {
        self.object = pointer.to_ne_bytes();
    }

    pub(crate) fn set_cookie(&mut self, cookie: binder_uintptr_t) {
        self.cookie = cookie;
    }

    pub(crate) fn acquire(&self) -> Result<()> {
        match self.type_ {
            BINDER_TYPE_BINDER => {
                // publish_count += 1, paired 1:1 with `release`; module doc "Native binder ids".
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
                log::error!("Invalid object type {:08x}", self.type_);
                Err(StatusCode::InvalidOperation)
            }
        }
    }

    pub(crate) fn release(&self) -> Result<()> {
        match self.type_ {
            BINDER_TYPE_BINDER => {
                // publish_count -= 1; teardown rule: see module doc "Native binder ids".
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
            // The parcel's `kernel_fds` owns and closes the fd, never these bytes.
            BINDER_TYPE_FD => Ok(()),
            _ => {
                log::error!("Invalid object type {:08x}", self.type_);
                Err(StatusCode::InvalidOperation)
            }
        }
    }
}

const SCHED_NORMAL: u32 = 0;
const FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT: u32 = 9;
/// Pre-shift 2-bit policy mask clamping `policy` to 0..=3; see module doc "Scheduler bits".
const FLAT_BINDER_FLAG_SCHED_POLICY_VALUE_MASK: u32 = 0x3;

fn sched_policy_mask(policy: u32, priority: u32) -> u32 {
    (priority & FLAT_BINDER_FLAG_PRIORITY_MASK)
        | ((policy & FLAT_BINDER_FLAG_SCHED_POLICY_VALUE_MASK)
            << FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT)
}

impl From<&SIBinder> for FlatBinderObject {
    fn from(binder: &SIBinder) -> Self {
        let sched_bits = if !process_state::ProcessState::as_self().background_scheduling_disabled()
        {
            sched_policy_mask(SCHED_NORMAL, 19)
        } else {
            0
        };

        if let Some(proxy) = binder.as_proxy() {
            // AOSP-correct `sched_bits`, not 0: see module doc "Scheduler bits". Do not "fix".
            FlatBinderObject::new_handle(proxy.handle(), sched_bits)
        } else {
            // Native binder: wire id from the sidecar table; see module doc "Native binder ids".
            let id =
                process_state::ProcessState::as_self().publish_native(Arc::clone(binder.as_arc()));

            // Explicit sched bits override the default, never OR (module doc "Scheduler bits").
            let local = binder.local_binder_flags();
            let sched_mask = FLAT_BINDER_FLAG_PRIORITY_MASK
                | (FLAT_BINDER_FLAG_SCHED_POLICY_VALUE_MASK << FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT);
            let effective_sched = if local & sched_mask != 0 {
                local & sched_mask
            } else {
                sched_bits
            };

            let mut obj =
                FlatBinderObject::new_binder_with_flags((local & !sched_mask) | effective_sched);
            obj.set_pointer(id);
            obj
        }
    }
}

/// Copy a flat_binder_object out of `data` at `offset`, which needs no alignment.
pub(crate) fn read_flat_binder(data: &[u8], offset: usize) -> Result<FlatBinderObject> {
    let bytes = data
        .get(offset..offset + FlatBinderObject::SIZE)
        .ok_or(StatusCode::NotEnoughData)?;
    Ok(FlatBinderObject::from_bytes(bytes.try_into().unwrap()))
}

/// Writes a flat_binder_object into `data` at `offset`, which needs no alignment.
pub(crate) fn write_flat_binder(
    data: &mut [u8],
    offset: usize,
    obj: &FlatBinderObject,
) -> Result<()> {
    let bytes = data
        .get_mut(offset..offset + FlatBinderObject::SIZE)
        .ok_or(StatusCode::NotEnoughData)?;
    bytes.copy_from_slice(&obj.to_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 8 union bytes AOSP `obj.binder = 0; obj.handle = h;` leaves: `h`, then four zeros.
    fn handle_bytes(h: u32) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&h.to_ne_bytes());
        b
    }

    /// `new_with_fd` writes AOSP's `flags = 0` and a zeroed union upper half; see module doc.
    #[test]
    fn new_with_fd_flags_zero_and_full_width_init() {
        let fd: i32 = 7;
        let obj = FlatBinderObject::new_with_fd(fd, false);

        assert_eq!(obj.header_type(), BINDER_TYPE_FD, "must be a FD object");

        // AOSP writeFileDescriptor sets flags = 0 for FD objects.
        assert_eq!(
            obj.flags, 0,
            "FD object flags must be 0 (AOSP Parcel::writeFileDescriptor)"
        );

        // The fd in `handle`'s bytes and the rest zero (no uninit leak), on either byte order.
        assert_eq!(
            obj.pointer().to_ne_bytes(),
            handle_bytes(fd as u32),
            "the union must hold the fd as `handle` with zeroed upper bytes"
        );
        assert_eq!(
            obj.handle(),
            fd as u32,
            "handle variant must round-trip the fd"
        );
    }

    /// `new_handle` (the `From<&SIBinder>` proxy arm) zeroes the union upper half; see module doc.
    #[test]
    fn new_handle_full_width_init() {
        let handle: u32 = 0xDEAD_BEEF;
        let obj = FlatBinderObject::new_handle(handle, 0);

        assert_eq!(
            obj.header_type(),
            BINDER_TYPE_HANDLE,
            "must be a HANDLE object"
        );
        assert_eq!(
            obj.pointer().to_ne_bytes(),
            handle_bytes(handle),
            "the union must hold `handle` with zeroed upper bytes"
        );
        assert_eq!(
            obj.handle(),
            handle,
            "handle variant must round-trip the handle"
        );
    }

    #[test]
    fn new_with_fd_cookie_tracks_take_ownership() {
        assert_eq!(FlatBinderObject::new_with_fd(3, true).cookie, 1);
        assert_eq!(FlatBinderObject::new_with_fd(3, false).cookie, 0);
    }

    /// The bytes are the UAPI layout on either byte order, and they read back as the object.
    #[test]
    fn to_bytes_is_the_uapi_layout_and_round_trips() {
        let mut obj = FlatBinderObject::new_handle(0xDEAD_BEEF, 0x17F);
        obj.set_cookie(0x0102_0304_0506_0708);
        let bytes = obj.to_bytes();
        assert_eq!(bytes[0..4], BINDER_TYPE_HANDLE.to_ne_bytes());
        assert_eq!(bytes[4..8], 0x17Fu32.to_ne_bytes());
        assert_eq!(bytes[8..16], handle_bytes(0xDEAD_BEEF));
        assert_eq!(bytes[16..24], 0x0102_0304_0506_0708u64.to_ne_bytes());
        assert_eq!(FlatBinderObject::from_bytes(&bytes), obj);
        assert_eq!(read_flat_binder(&bytes, 0), Ok(obj));
    }
}
