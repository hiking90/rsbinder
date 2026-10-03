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
//!   Between the two, `Parcel::write_aligned` can leave the entry unacquired
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
//! # Union initialisation
//!
//! `new_handle` and `new_with_fd` zero the 8-byte `binder` field, then write
//! the u32 `handle` over its first four bytes, as AOSP does
//! (`obj.binder = 0; obj.handle = ...;`). The zeroing keeps uninitialised stack
//! off the wire, since `write_object` copies the whole 24-byte struct. Writing
//! the value through `binder` instead (`binder: handle as u64`) puts it where
//! `handle` lies only on a little-endian host; on a big-endian one the handle
//! reads as 0. `new_with_fd`
//! also writes `flags = 0` as AOSP `Parcel::writeFileDescriptor` (kernel arm)
//! does; the kernel ignores the field for FD objects (it rewrites the object
//! on delivery), but the bytes match AOSP exactly.
//!
//! Writing only the u32 `handle` variant would leave the upper half
//! uninitialised, an uninitialised read plus a 4-byte stack leak to the peer.
//! For FD objects, a value such as `0x7F | ACCEPTS_FDS` (`0x17F`) breaks
//! nothing functionally but diverges from AOSP, whose `writeFileDescriptor`
//! bypasses `flattenBinder` and so writes no sched bits or `ACCEPTS_FDS`. An
//! rsbinder↔rsbinder round trip cannot catch either, since both sides ignore
//! those bytes, so `new_with_fd_flags_zero_and_full_width_init` and
//! `new_handle_full_width_init` (which covers the `From<&SIBinder>` proxy arm)
//! assert the AOSP bytes directly.

use std::sync::Arc;

pub(crate) use crate::sys::binder::flat_binder_object;
use crate::{binder::*, error::*, process_state, sys::*};

impl Default for flat_binder_object {
    /// Every field set explicitly: a safe alternative to `std::mem::zeroed()`.
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
        let mut obj = flat_binder_object {
            hdr: binder_object_header {
                type_: BINDER_TYPE_FD,
            },
            // AOSP `writeFileDescriptor` bypasses `flattenBinder`: no schedBits, no ACCEPTS_FDS.
            flags: 0,
            // Zeroed, then `handle` set: see module doc "Union initialisation".
            __bindgen_anon_1: flat_binder_object__bindgen_ty_1 { binder: 0 },
            cookie: if take_ownership { 1 } else { 0 },
        };
        obj.set_handle(fd as u32);
        obj
    }

    /// Creates a new flat_binder_object for a binder with the specified flags.
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

    /// Creates a new flat_binder_object for a remote handle (`BINDER_TYPE_HANDLE`).
    pub(crate) fn new_handle(handle: u32, flags: u32) -> Self {
        let mut obj = flat_binder_object {
            hdr: binder_object_header {
                type_: BINDER_TYPE_HANDLE,
            },
            flags,
            // Zeroed, then `handle` set: see module doc "Union initialisation".
            __bindgen_anon_1: flat_binder_object__bindgen_ty_1 { binder: 0 },
            cookie: 0,
        };
        obj.set_handle(handle);
        obj
    }

    pub(crate) fn header_type(&self) -> u32 {
        self.hdr.type_
    }

    pub(crate) fn handle(&self) -> u32 {
        // SAFETY: integer union, every bit pattern valid; caller picks `.handle` by `hdr.type`.
        unsafe { self.__bindgen_anon_1.handle }
    }

    pub(crate) fn set_handle(&mut self, handle: u32) {
        self.__bindgen_anon_1.handle = handle
    }

    pub(crate) fn pointer(&self) -> binder_uintptr_t {
        // SAFETY: integer union read (see `handle`); meaningful only for (WEAK_)BINDER objects.
        unsafe { self.__bindgen_anon_1.binder }
    }

    pub(crate) fn set_cookie(&mut self, cookie: binder_uintptr_t) {
        self.cookie = cookie;
    }

    pub(crate) fn acquire(&self) -> Result<()> {
        match self.hdr.type_ {
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
                log::error!("Invalid object type {:08x}", self.hdr.type_);
                Err(StatusCode::InvalidOperation)
            }
        }
    }

    pub(crate) fn release(&self) -> Result<()> {
        match self.hdr.type_ {
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
                log::error!("Invalid object type {:08x}", self.hdr.type_);
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

impl From<&SIBinder> for flat_binder_object {
    fn from(binder: &SIBinder) -> Self {
        let sched_bits = if !process_state::ProcessState::as_self().background_scheduling_disabled()
        {
            sched_policy_mask(SCHED_NORMAL, 19)
        } else {
            0
        };

        if let Some(proxy) = binder.as_proxy() {
            // AOSP-correct `sched_bits`, not 0: see module doc "Scheduler bits". Do not "fix".
            flat_binder_object::new_handle(proxy.handle(), sched_bits)
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

/// Copy a flat_binder_object out of `data` with `read_unaligned` (parcels are 4-byte aligned).
pub(crate) fn read_flat_binder(data: &[u8], offset: usize) -> Result<flat_binder_object> {
    let size = std::mem::size_of::<flat_binder_object>();
    let bytes = data
        .get(offset..offset + size)
        .ok_or(StatusCode::NotEnoughData)?;
    // SAFETY: `bytes` is exactly one object long; `#[repr(C)]` POD, any bit pattern valid.
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
    // SAFETY: `bytes` is exactly one object long; `write_unaligned` covers the parcel alignment.
    unsafe { std::ptr::write_unaligned(bytes.as_mut_ptr() as *mut flat_binder_object, *obj) };
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
        let obj = flat_binder_object::new_with_fd(fd, false);

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
        let obj = flat_binder_object::new_handle(handle, 0);

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
        assert_eq!(flat_binder_object::new_with_fd(3, true).cookie, 1);
        assert_eq!(flat_binder_object::new_with_fd(3, false).cookie, 0);
    }
}
