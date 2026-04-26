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
            flags: 0x7F & FLAT_BINDER_FLAG_ACCEPTS_FDS,
            __bindgen_anon_1: flat_binder_object__bindgen_ty_1 { handle: fd as _ },
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

    pub(crate) fn header_type(&self) -> u32 {
        self.hdr.type_
    }

    pub(crate) fn handle(&self) -> u32 {
        unsafe { self.__bindgen_anon_1.handle }
    }

    pub(crate) fn borrowed_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.handle() as _) }
    }

    pub(crate) fn owned_fd(&self) -> OwnedFd {
        unsafe { OwnedFd::from_raw_fd(self.handle() as _) }
    }

    pub(crate) fn set_handle(&mut self, handle: u32) {
        self.__bindgen_anon_1.handle = handle
    }

    pub(crate) fn pointer(&self) -> binder_uintptr_t {
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

fn sched_policy_mask(policy: u32, priority: u32) -> u32 {
    (priority & FLAT_BINDER_FLAG_PRIORITY_MASK)
        | ((policy & 3) << FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT)
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
            flat_binder_object {
                hdr: binder_object_header {
                    type_: BINDER_TYPE_HANDLE,
                },
                flags: sched_bits,
                __bindgen_anon_1: flat_binder_object__bindgen_ty_1 {
                    handle: proxy.handle(),
                },
                cookie: 0,
            }
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
            // process-fatal anyway — see plan §5 #11.
            let id =
                process_state::ProcessState::as_self().publish_native(Arc::clone(binder.as_arc()));

            flat_binder_object {
                hdr: binder_object_header {
                    type_: BINDER_TYPE_BINDER,
                },
                flags: FLAT_BINDER_FLAG_ACCEPTS_FDS | sched_bits,
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
    unsafe { std::ptr::write_unaligned(bytes.as_mut_ptr() as *mut flat_binder_object, *obj) };
    Ok(())
}
