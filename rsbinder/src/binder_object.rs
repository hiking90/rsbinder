// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::mem::ManuallyDrop;

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

    pub(crate) fn borrowed_fd(&self) -> BorrowedFd {
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

    pub(crate) fn cookie(&self) -> binder_uintptr_t {
        self.cookie
    }

    pub(crate) fn set_cookie(&mut self, cookie: binder_uintptr_t) {
        self.cookie = cookie;
    }

    pub(crate) fn acquire(&self) -> Result<()> {
        match self.hdr.type_ {
            BINDER_TYPE_BINDER => {
                if self.pointer() != 0 {
                    let strong = raw_pointer_to_strong_binder((self.pointer(), self.cookie()));
                    strong.increase()?;
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
                if self.pointer() != 0 {
                    let strong = raw_pointer_to_strong_binder((self.pointer(), self.cookie()));
                    strong.decrease()?;
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

fn split_fat_pointer(ptr: *const dyn IBinder) -> (u64, u64) {
    unsafe { std::mem::transmute(ptr) }
}

fn make_fat_pointer(raw_pointer: (binder_uintptr_t, binder_uintptr_t)) -> *const dyn IBinder {
    unsafe { std::mem::transmute(raw_pointer) }
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
            let strong = binder.clone();
            let (binder, cookie) = split_fat_pointer(strong.into_raw());

            flat_binder_object {
                hdr: binder_object_header {
                    type_: BINDER_TYPE_BINDER,
                },
                flags: FLAT_BINDER_FLAG_ACCEPTS_FDS | sched_bits,
                __bindgen_anon_1: flat_binder_object__bindgen_ty_1 {
                    binder: binder as _,
                },
                cookie: cookie as _,
            }
        }
    }
}

impl From<(*const u8, usize)> for &flat_binder_object {
    fn from(pointer: (*const u8, usize)) -> Self {
        unsafe {
            #[allow(clippy::transmute_ptr_to_ref)]
            std::mem::transmute::<*const u8, &flat_binder_object>(&*(pointer.0.add(pointer.1)))
        }
    }
}

impl From<(*mut u8, usize)> for &mut flat_binder_object {
    fn from(pointer: (*mut u8, usize)) -> Self {
        unsafe { 
            #[allow(clippy::transmute_ptr_to_ref)]
            std::mem::transmute::<*const u8, &mut flat_binder_object>(&*(pointer.0.add(pointer.1)))
        }
    }
}

pub(crate) fn raw_pointer_to_strong_binder(
    raw_pointer: (binder_uintptr_t, binder_uintptr_t),
) -> ManuallyDrop<SIBinder> {
    assert!(
        raw_pointer.0 != 0,
        "raw_pointer_to_strong_binder(): raw_pointer is null"
    );
    ManuallyDrop::new(SIBinder::from_raw(make_fat_pointer(raw_pointer)))
}
