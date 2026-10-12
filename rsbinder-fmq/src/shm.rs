// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Shared-memory primitives: the pieces a queue is built from, usable on
//! their own.
//!
//! [`MessageQueue`](crate::MessageQueue) is one protocol over memory two
//! processes map. This module is what lies under it, without the protocol:
//!
//! | Need | Item |
//! |---|---|
//! | Make the memory | [`create`] (a memfd), [`add_seals`], [`seals`], [`Seals`] |
//! | Map it | [`Mapping`], [`Access`] |
//! | Copy bytes in and out without a data race | [`Run`] |
//! | Sleep until a peer changes a word, and wake a peer | [`futex_wait`], [`futex_wake`], [`Deadline`] |
//! | Ask about an fd that arrived from a peer | [`region_size`], [`shrink_sealed`], [`is_ashmem_fd`], [`ashmem_size`] |
//!
//! The last row is what libcutils answers for libfmq (`ashmem-dev.cpp`),
//! with the same answers; `rsbinder`'s shared-memory code calls
//! [`is_ashmem_fd`] and [`ashmem_size`] from here rather than keeping a
//! second copy that could drift.
//!
//! # What is safe and what is not
//!
//! Creating, sealing and mapping are safe: they hand back an fd or an
//! address. The `unsafe` boundary is where an address becomes a reference —
//! [`Run::new`], or a `&AtomicU32` the caller forms for [`futex_wait`]. The
//! rule there is the same for every user of shared memory in Rust: all
//! access is atomic, and no byte is accessed at two sizes at once through
//! one mapping (see [`Mapping`]). A peer that ignores the protocol can then
//! make the data wrong, not the program undefined.
//!
//! Nothing here validates what a peer sent. A mapping of a peer's fd is as
//! large as the caller asks, readable or writable as the caller asks; check
//! size and seals first ([`region_size`], [`seals`]).
//!
//! # Platforms
//!
//! memfd, seals and futex are Linux and Android; elsewhere those functions
//! return [`Error::Unsupported`](crate::Error::Unsupported). None of the
//! module needs 64-bit atomics, so a Linux target without them
//! (`armv5te-unknown-linux-gnueabi`) has all of it. [`futex_wake`] has one
//! more condition on a 32-bit Linux target that rustix serves through libc
//! instead of its own system calls (x32 and AArch64 ILP32 excepted): there
//! the call is the `futex_time64` system call, which Linux has from 5.1, and
//! an older kernel answers `ENOSYS`. rustix goes through libc by default for
//! PowerPC, MIPS and every architecture it has no inline assembly for
//! (`powerpc-unknown-linux-gnu`, `mips-unknown-linux-gnu`,
//! `sparc-unknown-linux-gnu`), and for any other target when it is built
//! with its `use-libc` feature or `--cfg rustix_use_libc`.
//!
//! [`Mapping`] needs only `mmap` and [`Deadline`] only `CLOCK_MONOTONIC`,
//! which macOS has too. On WASI (`wasm32-wasip1`), where this crate has
//! neither, [`Mapping::map`] returns `Unsupported` and [`Deadline::after`]
//! returns `None`. [`Run`] is atomic loads and stores and has no condition
//! of its own.

use std::os::fd::BorrowedFd;

use crate::error::Result;

mod futex;
mod mapping;
mod memfd;
mod run;

pub use futex::{futex_wait, futex_wake, Deadline};
pub use mapping::{Access, Mapping};
pub use memfd::{add_seals, create, seals, CreateOptions, Seals};
pub use run::Run;

/// libcutils `__ashmem_is_ashmem`: is `fd` open on the ashmem character
/// device? Always `false` off Android. A peer-supplied fd must pass this
/// before any ashmem ioctl is sent to it — another driver could interpret
/// the request number its own way.
#[cfg(target_os = "android")]
pub fn is_ashmem_fd(fd: BorrowedFd<'_>) -> bool {
    let Ok(st) = rustix::fs::fstat(fd) else {
        return false;
    };
    if !is_char_device(&st) {
        return false;
    }
    ashmem_rdev().is_some_and(|rdev| st.st_rdev == rdev)
}

/// libcutils `__ashmem_is_ashmem`: is `fd` open on the ashmem character
/// device? Always `false` off Android.
#[cfg(not(target_os = "android"))]
pub fn is_ashmem_fd(_fd: BorrowedFd<'_>) -> bool {
    false
}

/// The ashmem rdev as libcutils `__init_ashmem_rdev` finds and caches it (boot_id node first).
#[cfg(target_os = "android")]
fn ashmem_rdev() -> Option<u64> {
    static RDEV: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    if let Some(rdev) = RDEV.get() {
        return Some(*rdev);
    }
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok();
    let with_boot_id = boot_id.map(|id| format!("/dev/ashmem{}", id.trim()));
    let rdev = with_boot_id
        .iter()
        .map(String::as_str)
        .chain(["/dev/ashmem"])
        .find_map(|path| rustix::fs::stat(path).ok())
        .filter(is_char_device)
        .map(|dev| dev.st_rdev)?;
    let _ = RDEV.set(rdev);
    Some(rdev)
}

// Not `st_mode & libc::S_IFMT`: the two differ in width on 32-bit Android.
#[cfg(target_os = "android")]
fn is_char_device(st: &rustix::fs::Stat) -> bool {
    rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::CharacterDevice
}

/// libcutils `ashmem_get_size_region`: the size of an ashmem region, read
/// with `ASHMEM_GET_SIZE`. `BadValue` when `fd` is not ashmem — which is
/// always the case off Android.
#[cfg(target_os = "android")]
pub fn ashmem_size(fd: BorrowedFd<'_>) -> Result<u64> {
    use std::os::fd::AsRawFd;
    if !is_ashmem_fd(fd) {
        return Err(crate::Error::BadValue("not an ashmem fd"));
    }
    // `ASHMEM_GET_SIZE` = `_IO(0x77, 4)`; the size is the ioctl's return value.
    const ASHMEM_GET_SIZE: libc::c_ulong = 0x7704;
    // SAFETY: `fd` is a verified ashmem fd; the request takes no pointer (`0` fills the vararg).
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), ASHMEM_GET_SIZE as _, 0) };
    if r < 0 {
        return Err(crate::error::errno_of(&std::io::Error::last_os_error()).into());
    }
    Ok(r as u64)
}

/// libcutils `ashmem_get_size_region`. `BadValue` when `fd` is not ashmem —
/// which is always the case off Android.
#[cfg(not(target_os = "android"))]
pub fn ashmem_size(_fd: BorrowedFd<'_>) -> Result<u64> {
    Err(crate::Error::BadValue("not an ashmem fd"))
}

/// The byte length of the object behind `fd`: `st_size` when it is
/// non-zero, otherwise the ashmem size (ashmem reports `st_size == 0`).
/// `BadValue` for an fd that is neither.
pub fn region_size(fd: BorrowedFd<'_>) -> Result<u64> {
    let st = rustix::fs::fstat(fd)?;
    if st.st_size > 0 {
        return Ok(st.st_size as u64);
    }
    ashmem_size(fd)
}

/// Whether `fd` carries `F_SEAL_SHRINK`, so no holder can truncate it under
/// a live mapping. `false` when the fd does not support seals at all, which
/// is every fd where memfd seals do not exist.
pub fn shrink_sealed(fd: BorrowedFd<'_>) -> bool {
    seals(fd).is_ok_and(|s| s.contains(Seals::SHRINK))
}
