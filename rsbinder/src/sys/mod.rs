// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Binder kernel UAPI: bindgen output plus typed ioctl wrappers.
//!
//! # ioctl safety
//!
//! Shared SAFETY rationale for every `ioctl::ioctl(fd, ctl)` in `binder`:
//! `rustix::ioctl::ioctl` is unsafe because it cannot verify that the
//! opcode matches the argument type or that the request is valid for
//! the fd. In each wrapper the opcode is built at compile time from
//! the binder UAPI request number (`b'b'`, N) and the exact argument
//! type via `ioctl::opcode::{read_write,write}::<T>`, the typed
//! `Setter`/`Updater` holds a value/buffer of precisely that `T`, and
//! the `Fd: AsFd` bound guarantees a valid borrowed fd for the call's
//! duration. So the opcode <-> arg-type <-> fd precondition holds.
//! Each block notes only its request.
//!
//! These three conditions cover every request whose argument is plain
//! data. `BINDER_WRITE_READ` is the exception: `binder_write_read`
//! carries two raw addresses, and the kernel reads `write_size` bytes
//! from `write_buffer` and writes up to `read_size` bytes to
//! `read_buffer`. The argument type cannot state that, so
//! `binder::write_read` is an `unsafe fn` whose caller guarantees that
//! `write_buffer` is readable for `write_size` bytes, `read_buffer` is
//! writable for `read_size` bytes, and both stay allocated and
//! unaliased by other writers until the ioctl returns.

// Allowances scoped to the bindgen output: `dead_code` still flags unused wrappers below.
#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    non_snake_case,
    unused_qualifications,
    clippy::unreadable_literal,
    clippy::missing_safety_doc
)]
mod raw {
    include!("sys.rs");
}
pub use raw::*;

pub mod binder {
    pub use crate::sys::*;

    pub const BR_ERROR: binder_driver_return_protocol = binder_driver_return_protocol_BR_ERROR;
    pub const BR_OK: binder_driver_return_protocol = binder_driver_return_protocol_BR_OK;
    pub const BR_TRANSACTION_SEC_CTX: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_TRANSACTION_SEC_CTX;
    pub const BR_TRANSACTION: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_TRANSACTION;
    pub const BR_REPLY: binder_driver_return_protocol = binder_driver_return_protocol_BR_REPLY;
    pub const BR_ACQUIRE_RESULT: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_ACQUIRE_RESULT;
    pub const BR_DEAD_REPLY: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_DEAD_REPLY;
    pub const BR_TRANSACTION_COMPLETE: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_TRANSACTION_COMPLETE;
    pub const BR_INCREFS: binder_driver_return_protocol = binder_driver_return_protocol_BR_INCREFS;
    pub const BR_ACQUIRE: binder_driver_return_protocol = binder_driver_return_protocol_BR_ACQUIRE;
    pub const BR_RELEASE: binder_driver_return_protocol = binder_driver_return_protocol_BR_RELEASE;
    pub const BR_DECREFS: binder_driver_return_protocol = binder_driver_return_protocol_BR_DECREFS;
    pub const BR_ATTEMPT_ACQUIRE: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_ATTEMPT_ACQUIRE;
    pub const BR_NOOP: binder_driver_return_protocol = binder_driver_return_protocol_BR_NOOP;
    pub const BR_SPAWN_LOOPER: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_SPAWN_LOOPER;
    pub const BR_FINISHED: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_FINISHED;
    pub const BR_DEAD_BINDER: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_DEAD_BINDER;
    pub const BR_CLEAR_DEATH_NOTIFICATION_DONE: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_CLEAR_DEATH_NOTIFICATION_DONE;
    pub const BR_FAILED_REPLY: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_FAILED_REPLY;
    pub const BR_FROZEN_REPLY: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_FROZEN_REPLY;
    pub const BR_ONEWAY_SPAM_SUSPECT: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_ONEWAY_SPAM_SUSPECT;

    // Freeze BRs (6.5+): `wait_for_response` handles PENDING_FROZEN; the rest log BAD COMMAND.
    pub const BR_TRANSACTION_PENDING_FROZEN: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_TRANSACTION_PENDING_FROZEN;
    #[allow(dead_code)] // freeze dispatch not wired yet (see above)
    pub const BR_FROZEN_BINDER: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_FROZEN_BINDER;
    #[allow(dead_code)] // freeze dispatch not wired yet (see above)
    pub const BR_CLEAR_FREEZE_NOTIFICATION_DONE: binder_driver_return_protocol =
        binder_driver_return_protocol_BR_CLEAR_FREEZE_NOTIFICATION_DONE;

    pub const BC_TRANSACTION: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_TRANSACTION;
    pub const BC_REPLY: binder_driver_command_protocol = binder_driver_command_protocol_BC_REPLY;
    pub const BC_ACQUIRE_RESULT: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_ACQUIRE_RESULT;
    pub const BC_FREE_BUFFER: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_FREE_BUFFER;
    pub const BC_INCREFS: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_INCREFS;
    pub const BC_ACQUIRE: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_ACQUIRE;
    pub const BC_RELEASE: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_RELEASE;
    pub const BC_DECREFS: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_DECREFS;
    pub const BC_INCREFS_DONE: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_INCREFS_DONE;
    pub const BC_ACQUIRE_DONE: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_ACQUIRE_DONE;
    // Defined by the driver, never sent by AOSP libbinder or rsbinder; kept to match the UAPI.
    #[allow(dead_code)]
    pub const BC_ATTEMPT_ACQUIRE: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_ATTEMPT_ACQUIRE;
    pub const BC_REGISTER_LOOPER: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_REGISTER_LOOPER;
    pub const BC_ENTER_LOOPER: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_ENTER_LOOPER;
    pub const BC_EXIT_LOOPER: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_EXIT_LOOPER;
    pub const BC_REQUEST_DEATH_NOTIFICATION: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_REQUEST_DEATH_NOTIFICATION;
    pub const BC_CLEAR_DEATH_NOTIFICATION: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_CLEAR_DEATH_NOTIFICATION;
    pub const BC_DEAD_BINDER_DONE: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_DEAD_BINDER_DONE;
    #[allow(dead_code)] // never sent, see BC_ATTEMPT_ACQUIRE
    pub const BC_TRANSACTION_SG: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_TRANSACTION_SG;
    #[allow(dead_code)] // never sent, see BC_ATTEMPT_ACQUIRE
    pub const BC_REPLY_SG: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_REPLY_SG;

    // Freeze BCs and their payload structs, exposed ahead of the unwired send path.
    #[allow(dead_code)] // freeze dispatch not wired yet (see above)
    pub const BC_REQUEST_FREEZE_NOTIFICATION: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_REQUEST_FREEZE_NOTIFICATION;
    #[allow(dead_code)] // freeze dispatch not wired yet (see above)
    pub const BC_CLEAR_FREEZE_NOTIFICATION: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_CLEAR_FREEZE_NOTIFICATION;
    #[allow(dead_code)] // freeze dispatch not wired yet (see above)
    pub const BC_FREEZE_NOTIFICATION_DONE: binder_driver_command_protocol =
        binder_driver_command_protocol_BC_FREEZE_NOTIFICATION_DONE;

    use rustix::{io, ioctl};
    use std::os::fd::AsFd;

    // Shared SAFETY rationale for every ioctl below: module doc, "ioctl safety".

    /// Safety: both buffer addresses must satisfy the module doc's `BINDER_WRITE_READ` contract.
    pub(crate) unsafe fn write_read<Fd: AsFd>(
        fd: Fd,
        write_read: &mut binder_write_read,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: shared rationale above; the buffer addresses are the caller's contract.
            let ctl = ioctl::Updater::<
                { ioctl::opcode::read_write::<binder_write_read>(b'b', 1) },
                _,
            >::new(write_read);
            ioctl::ioctl(fd, ctl)
        }
    }

    pub(crate) fn set_max_threads<Fd: AsFd>(
        fd: Fd,
        max_threads: u32,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_SET_MAX_THREADS.
            let ctl =
                ioctl::Setter::<{ ioctl::opcode::write::<__u32>(b'b', 5) }, _>::new(max_threads);
            ioctl::ioctl(fd, ctl)
        }
    }

    pub(crate) fn set_context_mgr<Fd: AsFd>(
        fd: Fd,
        pid: i32,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_SET_CONTEXT_MGR.
            let ctl = ioctl::Setter::<{ ioctl::opcode::write::<__s32>(b'b', 7) }, _>::new(pid);
            ioctl::ioctl(fd, ctl)
        }
    }

    pub(crate) fn version<Fd: AsFd>(
        fd: Fd,
        ver: &mut binder_version,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_VERSION.
            let ctl =
                ioctl::Updater::<{ ioctl::opcode::read_write::<binder_version>(b'b', 9) }, _>::new(
                    ver,
                );
            ioctl::ioctl(fd, ctl)
        }
    }

    pub(crate) fn set_context_mgr_ext<Fd: AsFd>(
        fd: Fd,
        obj: flat_binder_object,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_SET_CONTEXT_MGR_EXT.
            let ctl =
                ioctl::Setter::<{ ioctl::opcode::write::<flat_binder_object>(b'b', 13) }, _>::new(
                    obj,
                );
            ioctl::ioctl(fd, ctl)
        }
    }

    pub(crate) fn enable_oneway_spam_detection<Fd: AsFd>(
        fd: Fd,
        enable: __u32,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_ENABLE_ONEWAY_SPAM_DETECTION.
            let ctl = ioctl::Setter::<{ ioctl::opcode::write::<__u32>(b'b', 16) }, _>::new(enable);
            ioctl::ioctl(fd, ctl)
        }
    }

    // `binderfs::add_device` substitutes a mock under `cfg(test)`.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn binder_ctl_add<Fd: AsFd>(
        fd: Fd,
        device: &mut binderfs_device,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_CTL_ADD.
            let ctl =
                ioctl::Updater::<{ ioctl::opcode::read_write::<binderfs_device>(b'b', 1) }, _>::new(
                    device,
                );
            ioctl::ioctl(fd, ctl)
        }
    }

    // BINDER_SET_IDLE_TIMEOUT is a no-op in every shipping driver; kept for UAPI completeness.
    #[allow(dead_code)]
    pub(crate) fn set_idle_timeout<Fd: AsFd>(
        fd: Fd,
        timeout: i64,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_SET_IDLE_TIMEOUT.
            let ctl = ioctl::Setter::<{ ioctl::opcode::write::<__s64>(b'b', 3) }, _>::new(timeout);
            ioctl::ioctl(fd, ctl)
        }
    }

    // BINDER_SET_IDLE_PRIORITY is a no-op in every shipping driver; kept for UAPI completeness.
    #[allow(dead_code)]
    pub(crate) fn set_idle_priority<Fd: AsFd>(
        fd: Fd,
        priority: i32,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_SET_IDLE_PRIORITY.
            let ctl = ioctl::Setter::<{ ioctl::opcode::write::<__s32>(b'b', 6) }, _>::new(priority);
            ioctl::ioctl(fd, ctl)
        }
    }

    pub(crate) fn thread_exit<Fd: AsFd>(fd: Fd, pid: i32) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_THREAD_EXIT.
            let ctl = ioctl::Setter::<{ ioctl::opcode::write::<__s32>(b'b', 8) }, _>::new(pid);
            ioctl::ioctl(fd, ctl)
        }
    }

    // debug-only ioctl; no crate path issues it yet.
    #[allow(dead_code)]
    pub(crate) fn get_node_debug_info<Fd: AsFd>(
        fd: Fd,
        node_debug_info: &mut binder_node_debug_info,
    ) -> std::result::Result<(), rustix::io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_GET_NODE_DEBUG_INFO.
            let ctl = ioctl::Updater::<
                { ioctl::opcode::read_write::<binder_node_debug_info>(b'b', 11) },
                _,
            >::new(node_debug_info);
            ioctl::ioctl(fd, ctl)
        }
    }

    pub(crate) fn get_node_info_for_ref<Fd: AsFd>(
        fd: Fd,
        node_info: &mut binder_node_info_for_ref,
    ) -> std::result::Result<(), rustix::io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_GET_NODE_INFO_FOR_REF.
            let ctl = ioctl::Updater::<
                { ioctl::opcode::read_write::<binder_node_info_for_ref>(b'b', 12) },
                _,
            >::new(node_info);
            ioctl::ioctl(fd, ctl)
        }
    }

    // plan 4-3 Phase B (freeze) will issue this; declared ahead so the opcode is pinned.
    #[allow(dead_code)]
    pub(crate) fn freeze<Fd: AsFd>(
        fd: Fd,
        info: binder_freeze_info,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_FREEZE.
            let ctl =
                ioctl::Setter::<{ ioctl::opcode::write::<binder_freeze_info>(b'b', 14) }, _>::new(
                    info,
                );
            ioctl::ioctl(fd, ctl)
        }
    }

    // plan 4-3 Phase B (freeze) will issue this; declared ahead so the opcode is pinned.
    #[allow(dead_code)]
    pub(crate) fn get_frozen_info<Fd: AsFd>(
        fd: Fd,
        frozen_info: &mut binder_frozen_status_info,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_GET_FROZEN_INFO.
            let ctl = ioctl::Updater::<
                { ioctl::opcode::read_write::<binder_frozen_status_info>(b'b', 15) },
                _,
            >::new(frozen_info);
            ioctl::ioctl(fd, ctl)
        }
    }

    // 12 bytes (id/command/errno) of the thread's last failure; a driver without it: `EINVAL`.
    pub(crate) fn get_extended_error<Fd: AsFd>(
        fd: Fd,
        ee: &mut binder_extended_error,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_GET_EXTENDED_ERROR.
            let ctl = ioctl::Updater::<
                { ioctl::opcode::read_write::<binder_extended_error>(b'b', 17) },
                _,
            >::new(ee);
            ioctl::ioctl(fd, ctl)
        }
    }
}
