// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Questions about a shared-memory fd that arrived from a peer: how large it
//! is, whether it is ashmem, whether it can still shrink. libcutils answers
//! them for libfmq (`ashmem-dev.cpp`); these are the same answers, so
//! `rsbinder`'s shared-memory code uses them too rather than keeping a second
//! copy that could drift.

use std::os::fd::BorrowedFd;

use crate::error::Result;

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
        return Err(rustix::io::Errno::from_raw_os_error(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        )
        .into());
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
/// a live mapping. `false` when the fd does not support seals at all.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn shrink_sealed(fd: BorrowedFd<'_>) -> bool {
    rustix::fs::fcntl_get_seals(fd)
        .map(|s| s.contains(rustix::fs::SealFlags::SHRINK))
        .unwrap_or(false)
}

/// Whether `fd` carries `F_SEAL_SHRINK`. Always `false` where memfd seals
/// do not exist.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn shrink_sealed(_fd: BorrowedFd<'_>) -> bool {
    false
}
