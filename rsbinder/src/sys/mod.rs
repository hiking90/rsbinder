// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Binder kernel UAPI: bindgen output plus typed ioctl wrappers.
//!
//! `sys.rs` is unmodified bindgen output; `generate.sh` regenerates it from
//! the verbatim kernel headers under `include/` and states the options.
//!
//! # UAPI items not used yet
//!
//! The bindings keep every item of the two headers, including ones no
//! rsbinder path uses, so that the crate matches the UAPI:
//!
//! - `BC_ATTEMPT_ACQUIRE`, `BC_TRANSACTION_SG`, `BC_REPLY_SG`: defined by
//!   the driver and sent by neither AOSP libbinder nor rsbinder.
//! - The `set_idle_timeout`, `set_idle_priority` and `get_node_debug_info`
//!   wrappers below, each with its reason.
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
//! carries two raw addresses, and the kernel reads from `write_buffer`
//! and writes into `read_buffer`.
//!
//! `binder::write_read` takes the two buffers as slices and sets the
//! addresses and sizes itself, so the borrows keep both allocated and
//! unaliased for the call. It refuses a consumed count past its buffer
//! or a read count that is not a whole 4-byte word, and rounds the read
//! buffer down to whole words. `binder_thread_read`
//! (`drivers/android/binder.c`) starts at `read_buffer + read_consumed`,
//! writes `BR_NOOP` there before any room check when `read_consumed` is
//! 0, and its later room check (`end - ptr` against an unsigned size)
//! passes once `ptr` is past `end`; every write after that is a whole
//! word. With these checks the driver writes only inside `read`.
//!
//! The slices cannot vouch for the commands in `write`. A
//! `BC_FREE_BUFFER` naming a receive buffer that a live `Parcel` still
//! reads lets the driver hand that buffer to the next transaction and
//! overwrite it under the parcel's `&[u8]`. So `write_read` stays an
//! `unsafe fn`: its caller guarantees that `write` frees no buffer a
//! live `Parcel` reads. The command stream queues `BC_FREE_BUFFER` only
//! from `thread_state::free_buffer`, called when a parcel drops or for a
//! status reply no parcel ever wrapped.

#[expect(
    non_camel_case_types,
    dead_code,
    reason = "bindgen output: C type names, and UAPI items not used yet (module doc)"
)]
mod raw {
    include!("sys.rs");
}
pub use raw::*;

pub mod binder {
    pub use crate::sys::*;

    /// `binder_transaction_data.sender_pid`'s type, under its libc name.
    pub use crate::sys::__kernel_pid_t as pid_t;
    /// `binder_transaction_data.sender_euid`'s type, under its libc name.
    pub use crate::sys::__kernel_uid32_t as uid_t;

    use rustix::{io, ioctl};
    use std::os::fd::AsFd;

    // Shared SAFETY rationale for every ioctl below: module doc, "ioctl safety".

    /// Safety: `write` frees no buffer a live `Parcel` reads (module doc `BINDER_WRITE_READ`).
    pub(crate) unsafe fn write_read<Fd: AsFd>(
        fd: Fd,
        write: &[u8],
        read: &mut [u8],
        progress: &mut binder_write_read,
    ) -> std::result::Result<(), io::Errno> {
        let words = read.len() & !3;
        let read = &mut read[..words];
        if progress.write_consumed > write.len() as binder_size_t
            || progress.read_consumed > read.len() as binder_size_t
            || progress.read_consumed % 4 != 0
        {
            return Err(io::Errno::INVAL);
        }
        progress.write_size = write.len() as binder_size_t;
        progress.write_buffer = write.as_ptr() as binder_uintptr_t;
        progress.read_size = read.len() as binder_size_t;
        progress.read_buffer = read.as_mut_ptr() as binder_uintptr_t;
        unsafe {
            // SAFETY: shared rationale above; buffers checked here, commands the caller's contract.
            let ctl = ioctl::Updater::<
                { ioctl::opcode::read_write::<binder_write_read>(b'b', 1) },
                _,
            >::new(progress);
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
    #[cfg(not(test))]
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

    #[expect(
        dead_code,
        reason = "BINDER_SET_IDLE_TIMEOUT is a no-op in every shipping driver; kept for the UAPI"
    )]
    pub(crate) fn set_idle_timeout<Fd: AsFd>(
        fd: Fd,
        timeout: i64,
    ) -> std::result::Result<(), io::Errno> {
        unsafe {
            // SAFETY: see shared rationale above. Request: BINDER_SET_IDLE_TIMEOUT.
            let ctl = ioctl::Setter::<{ ioctl::opcode::write::<i64>(b'b', 3) }, _>::new(timeout);
            ioctl::ioctl(fd, ctl)
        }
    }

    #[expect(
        dead_code,
        reason = "BINDER_SET_IDLE_PRIORITY is a no-op in every shipping driver; kept for the UAPI"
    )]
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

    #[expect(dead_code, reason = "debug-only ioctl; no crate path issues it yet")]
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

    #[cfg(test)]
    mod tests {
        use super::*;

        fn progress(write_consumed: u64, read_consumed: u64) -> binder_write_read {
            binder_write_read {
                write_size: 0,
                write_consumed,
                write_buffer: 0,
                read_size: 0,
                read_consumed,
                read_buffer: 0,
            }
        }

        // /dev/null: a refused count never reaches the ioctl; an accepted one gets ENOTTY.
        fn call(write: &[u8], read: &mut [u8], bwr: &mut binder_write_read) -> io::Errno {
            let null = std::fs::File::open("/dev/null").expect("/dev/null");
            // SAFETY: `write` holds no command the driver could act on; the fd is not binder.
            unsafe { write_read(&null, write, read, bwr) }.expect_err("not a binder fd")
        }

        #[test]
        fn write_read_refuses_a_consumed_count_outside_its_buffer() {
            let mut read = [0u8; 8];
            assert_eq!(
                call(&[0; 4], &mut read, &mut progress(5, 0)),
                io::Errno::INVAL
            );
            assert_eq!(call(&[], &mut read, &mut progress(0, 12)), io::Errno::INVAL);
            assert_eq!(call(&[], &mut read, &mut progress(0, 2)), io::Errno::INVAL);
            assert_ne!(
                call(&[0; 4], &mut read, &mut progress(4, 8)),
                io::Errno::INVAL
            );
        }

        #[test]
        fn write_read_lends_the_read_buffer_in_whole_words() {
            let mut read = [0u8; 7];
            let mut bwr = progress(0, 0);
            call(&[0; 4], &mut read, &mut bwr);
            assert_eq!((bwr.write_size, bwr.read_size), (4, 4));
            // Under a word: no read at all, so the driver's unchecked `BR_NOOP` never lands.
            call(&[], &mut read[..3], &mut bwr);
            assert_eq!(bwr.read_size, 0);
        }
    }
}
