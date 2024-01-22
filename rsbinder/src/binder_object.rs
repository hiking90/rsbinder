// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::mem::ManuallyDrop;

pub(crate) use crate::sys::binder::flat_binder_object;
use crate::{
    binder::*,
    error::*,
    process_state,
    sys::*,
};

impl Default for flat_binder_object {
    fn default() -> Self {
        flat_binder_object {
            hdr: binder_object_header {
                type_: BINDER_TYPE_BINDER
            },
            flags: 0,
            __bindgen_anon_1: flat_binder_object__bindgen_ty_1 {
                binder: 0,
            },
            cookie: 0,
        }
    }
}

impl flat_binder_object {
    pub(crate) fn new_with_fd(fd: i32, take_ownership: bool) -> Self {
        flat_binder_object {
            hdr: binder_object_header {
                type_: BINDER_TYPE_FD
            },
            flags: 0x7F & FLAT_BINDER_FLAG_ACCEPTS_FDS,
            __bindgen_anon_1: flat_binder_object__bindgen_ty_1 {
                handle: fd as _,
            },
            cookie: if take_ownership { 1 } else { 0 },
        }
    }

    pub(crate) fn header_type(&self) -> u32 {
        self.hdr.type_
    }

    pub(crate) fn handle(&self) -> u32 {
        unsafe { self.__bindgen_anon_1.handle }
    }

    pub(crate) fn pointer(&self) -> binder_uintptr_t {
        unsafe { self.__bindgen_anon_1.binder }
    }

    pub(crate) fn cookie(&self) -> binder_uintptr_t {
        self.cookie
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
            BINDER_TYPE_HANDLE => {
                process_state::ProcessState::as_self().strong_proxy_for_handle(self.handle())?.increase()
            }
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
            BINDER_TYPE_HANDLE => {
                process_state::ProcessState::as_self().strong_proxy_for_handle(self.handle())?.decrease()
            }
            BINDER_TYPE_FD => {
                if self.cookie != 0 {   // owned
                    nix::unistd::close(self.handle() as _)?;
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
    unsafe {
        std::mem::transmute(ptr)
    }
}

fn make_fat_pointer(raw_pointer: (binder_uintptr_t, binder_uintptr_t)) -> *const dyn IBinder {
    unsafe {
        std::mem::transmute(raw_pointer)
    }
}

const SCHED_NORMAL:u32 = 0;
const FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT:u32 = 9;

fn sched_policy_mask(policy: u32, priority: u32) -> u32 {
    (priority & FLAT_BINDER_FLAG_PRIORITY_MASK) | ((policy & 3) << FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT)
}

impl From<&SIBinder> for flat_binder_object {
    fn from(binder: &SIBinder) -> Self {
        let sched_bits = if !process_state::ProcessState::as_self().background_scheduling_disabled() {
            sched_policy_mask(SCHED_NORMAL, 19)
        } else {
            0
        };

        if let Some(proxy) = binder.as_proxy() {
            flat_binder_object {
                hdr: binder_object_header {
                    type_: BINDER_TYPE_HANDLE
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
                    type_: BINDER_TYPE_BINDER
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

impl From<*const u8> for flat_binder_object {
    fn from(raw_pointer: *const u8) -> Self {
        // To avoid the runtime error "misaligned pointer dereference", memory copy is used.
        let mut obj: flat_binder_object = unsafe { std::mem::zeroed() };
        unsafe {
            std::ptr::copy_nonoverlapping(
                raw_pointer,
                &mut obj as *mut _ as *mut u8,
                std::mem::size_of::<flat_binder_object>(),
            );
        }
        obj
    }
}

pub(crate) fn raw_pointer_to_strong_binder(raw_pointer: (binder_uintptr_t, binder_uintptr_t)) -> ManuallyDrop<SIBinder> {
    assert!(raw_pointer.0 != 0, "raw_pointer_to_strong_binder(): raw_pointer is null");
    ManuallyDrop::new(SIBinder::from_raw(make_fat_pointer(raw_pointer)))
}
