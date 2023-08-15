// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    non_snake_case,
    unused_qualifications,
)]
#![allow(clippy::unreadable_literal, clippy::missing_safety_doc)]


include!("sys.rs");

pub mod binder {
    pub use crate::sys::*;

    pub const BR_ERROR: binder_driver_return_protocol = binder_driver_return_protocol_BR_ERROR;
    pub const BR_OK: binder_driver_return_protocol = binder_driver_return_protocol_BR_OK;
    pub const BR_TRANSACTION_SEC_CTX: binder_driver_return_protocol = binder_driver_return_protocol_BR_TRANSACTION_SEC_CTX;
    pub const BR_TRANSACTION: binder_driver_return_protocol = binder_driver_return_protocol_BR_TRANSACTION;
    pub const BR_REPLY: binder_driver_return_protocol = binder_driver_return_protocol_BR_REPLY;
    pub const BR_ACQUIRE_RESULT: binder_driver_return_protocol = binder_driver_return_protocol_BR_ACQUIRE_RESULT;
    pub const BR_DEAD_REPLY: binder_driver_return_protocol = binder_driver_return_protocol_BR_DEAD_REPLY;
    pub const BR_TRANSACTION_COMPLETE: binder_driver_return_protocol = binder_driver_return_protocol_BR_TRANSACTION_COMPLETE;
    pub const BR_INCREFS: binder_driver_return_protocol = binder_driver_return_protocol_BR_INCREFS;
    pub const BR_ACQUIRE: binder_driver_return_protocol = binder_driver_return_protocol_BR_ACQUIRE;
    pub const BR_RELEASE: binder_driver_return_protocol = binder_driver_return_protocol_BR_RELEASE;
    pub const BR_DECREFS: binder_driver_return_protocol = binder_driver_return_protocol_BR_DECREFS;
    pub const BR_ATTEMPT_ACQUIRE: binder_driver_return_protocol = binder_driver_return_protocol_BR_ATTEMPT_ACQUIRE;
    pub const BR_NOOP: binder_driver_return_protocol = binder_driver_return_protocol_BR_NOOP;
    pub const BR_SPAWN_LOOPER: binder_driver_return_protocol = binder_driver_return_protocol_BR_SPAWN_LOOPER;
    pub const BR_FINISHED: binder_driver_return_protocol = binder_driver_return_protocol_BR_FINISHED;
    pub const BR_DEAD_BINDER: binder_driver_return_protocol = binder_driver_return_protocol_BR_DEAD_BINDER;
    pub const BR_CLEAR_DEATH_NOTIFICATION_DONE: binder_driver_return_protocol = binder_driver_return_protocol_BR_CLEAR_DEATH_NOTIFICATION_DONE;
    pub const BR_FAILED_REPLY: binder_driver_return_protocol = binder_driver_return_protocol_BR_FAILED_REPLY;
    pub const BR_FROZEN_REPLY: binder_driver_return_protocol = binder_driver_return_protocol_BR_FROZEN_REPLY;
    pub const BR_ONEWAY_SPAM_SUSPECT: binder_driver_return_protocol = binder_driver_return_protocol_BR_ONEWAY_SPAM_SUSPECT;

    pub const BC_TRANSACTION: binder_driver_command_protocol = binder_driver_command_protocol_BC_TRANSACTION;
    pub const BC_REPLY: binder_driver_command_protocol = binder_driver_command_protocol_BC_REPLY;
    pub const BC_ACQUIRE_RESULT: binder_driver_command_protocol = binder_driver_command_protocol_BC_ACQUIRE_RESULT;
    pub const BC_FREE_BUFFER: binder_driver_command_protocol = binder_driver_command_protocol_BC_FREE_BUFFER;
    pub const BC_INCREFS: binder_driver_command_protocol = binder_driver_command_protocol_BC_INCREFS;
    pub const BC_ACQUIRE: binder_driver_command_protocol = binder_driver_command_protocol_BC_ACQUIRE;
    pub const BC_RELEASE: binder_driver_command_protocol = binder_driver_command_protocol_BC_RELEASE;
    pub const BC_DECREFS: binder_driver_command_protocol = binder_driver_command_protocol_BC_DECREFS;
    pub const BC_INCREFS_DONE: binder_driver_command_protocol = binder_driver_command_protocol_BC_INCREFS_DONE;
    pub const BC_ACQUIRE_DONE: binder_driver_command_protocol = binder_driver_command_protocol_BC_ACQUIRE_DONE;
    pub const BC_ATTEMPT_ACQUIRE: binder_driver_command_protocol = binder_driver_command_protocol_BC_ATTEMPT_ACQUIRE;
    pub const BC_REGISTER_LOOPER: binder_driver_command_protocol = binder_driver_command_protocol_BC_REGISTER_LOOPER;
    pub const BC_ENTER_LOOPER: binder_driver_command_protocol = binder_driver_command_protocol_BC_ENTER_LOOPER;
    pub const BC_EXIT_LOOPER: binder_driver_command_protocol = binder_driver_command_protocol_BC_EXIT_LOOPER;
    pub const BC_REQUEST_DEATH_NOTIFICATION: binder_driver_command_protocol = binder_driver_command_protocol_BC_REQUEST_DEATH_NOTIFICATION;
    pub const BC_CLEAR_DEATH_NOTIFICATION: binder_driver_command_protocol = binder_driver_command_protocol_BC_CLEAR_DEATH_NOTIFICATION;
    pub const BC_DEAD_BINDER_DONE: binder_driver_command_protocol = binder_driver_command_protocol_BC_DEAD_BINDER_DONE;
    pub const BC_TRANSACTION_SG: binder_driver_command_protocol = binder_driver_command_protocol_BC_TRANSACTION_SG;
    pub const BC_REPLY_SG: binder_driver_command_protocol = binder_driver_command_protocol_BC_REPLY_SG;

    nix::ioctl_readwrite!(write_read, b'b', 1, binder_write_read);
    nix::ioctl_write_ptr!(set_idle_timeout, b'b', 3, __s64);

    nix::ioctl_write_ptr!(set_max_threads, b'b', 5, __u32);
    nix::ioctl_write_ptr!(set_idle_priority, b'b', 6, __s32);
    nix::ioctl_write_ptr!(set_context_mgr, b'b', 7, __s32);

    nix::ioctl_write_ptr!(thread_exit, b'b', 8, __s32);
    nix::ioctl_readwrite!(version, b'b', 9, binder_version);

    nix::ioctl_readwrite!(get_node_debug_info, b'b', 11, binder_node_debug_info);
    nix::ioctl_readwrite!(get_node_info_for_ref, b'b', 12, binder_node_info_for_ref);
    nix::ioctl_write_ptr!(set_context_mgr_ext, b'b', 13, flat_binder_object);
    nix::ioctl_write_ptr!(freeze, b'b', 14, binder_freeze_info);
    nix::ioctl_readwrite!(get_frozen_info, b'b', 15, binder_frozen_status_info);
    nix::ioctl_write_ptr!(enable_oneway_spam_detection, b'b', 16, __u32);

    nix::ioctl_readwrite!(binder_ctl_add, b'b', 1, binderfs_device);
}