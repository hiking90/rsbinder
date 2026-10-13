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
//! | Read memory this process may not write | [`RoRun`], [`RoWord32`], `RoWord64` |
//! | Refuse a peer's fd whose pages the peer could take away | [`verify_received`] |
//! | Ask about an fd that arrived from a peer | [`region_size`], [`shrink_sealed`], [`is_ashmem_fd`], [`ashmem_size`], [`ashmem_write_protected`] |
//!
//! The last row is what libcutils answers for libfmq (`ashmem-dev.cpp`),
//! with the same answers; `rsbinder`'s shared-memory code calls
//! [`is_ashmem_fd`], [`ashmem_size`] and [`ashmem_write_protected`] from here rather than keeping a
//! second copy that could drift, and makes, seals and maps its heaps with
//! the first two rows.
//!
//! # What is safe and what is not
//!
//! Creating, sealing and mapping are safe: they hand back an fd or an
//! address. The `unsafe` boundary is where an address becomes a reference —
//! [`Run::new`], [`RoWord32::from_ptr`], or a `&AtomicU32` the caller forms
//! for [`futex_wait`]. The rule there is the same for every user of shared
//! memory in Rust: all access is atomic, and no byte is accessed at two
//! sizes at once through one mapping (see [`Mapping`]). A peer that ignores
//! the protocol can then make the data wrong, not the program undefined.
//!
//! [`Mapping::map`] validates nothing a peer sent: the mapping is as large
//! as the caller asks, readable or writable as the caller asks. Check the fd
//! first — [`verify_received`] for a memfd whose pages the peer must not be
//! able to take away, or size and seals one by one ([`region_size`],
//! [`seals`]) for a protocol with other rules, as libfmq's are.
//!
//! # Platforms
//!
//! memfd, seals and futex are Linux and Android; elsewhere those functions
//! return [`Error::Unsupported`](crate::Error::Unsupported). `RoWord64`
//! apart, none of the module needs 64-bit atomics, so a Linux target without
//! them (`armv5te-unknown-linux-gnueabi`) has the rest. [`futex_wake`] has one
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
//! of its own. `RoWord64` exists where an 8-byte load does not write: the
//! 64-bit architectures, 32-bit x86 with SSE2 and 32-bit ARM with 64-bit
//! atomics (its documentation gives the grounds for each).

use std::os::fd::BorrowedFd;

use crate::error::Result;

mod futex;
mod mapping;
mod memfd;
mod run;
mod word;

pub use futex::{futex_wait, futex_wake, Deadline};
pub use mapping::{Access, Mapping};
pub use memfd::{add_seals, create, seals, verify_received, CreateOptions, Seals};
pub use run::{RoRun, Run};
// A glob: `RoWord64` is there only on the architectures `word.rs` names.
pub use word::*;

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

/// An ashmem `_IO(0x77, nr)` request, whose answer is the ioctl's return value.
/// `BadValue` when `fd` is not ashmem.
#[cfg(target_os = "android")]
fn ashmem_query(fd: BorrowedFd<'_>, request: libc::c_ulong) -> Result<u64> {
    use std::os::fd::AsRawFd;
    if !is_ashmem_fd(fd) {
        return Err(crate::Error::BadValue("not an ashmem fd"));
    }
    // SAFETY: `fd` is a verified ashmem fd; the request takes no pointer (`0` fills the vararg).
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), request as _, 0) };
    if r < 0 {
        return Err(crate::error::errno_of(&std::io::Error::last_os_error()).into());
    }
    Ok(r as u64)
}

/// libcutils `ashmem_get_size_region`: the size of an ashmem region, read
/// with `ASHMEM_GET_SIZE`. `BadValue` when `fd` is not ashmem — which is
/// always the case off Android.
#[cfg(target_os = "android")]
pub fn ashmem_size(fd: BorrowedFd<'_>) -> Result<u64> {
    // `ASHMEM_GET_SIZE` = `_IO(0x77, 4)`.
    ashmem_query(fd, 0x7704)
}

/// libcutils `ashmem_get_size_region`. `BadValue` when `fd` is not ashmem —
/// which is always the case off Android.
#[cfg(not(target_os = "android"))]
pub fn ashmem_size(_fd: BorrowedFd<'_>) -> Result<u64> {
    Err(crate::Error::BadValue("not an ashmem fd"))
}

/// Whether an ashmem region can no longer be mapped for writing: libcutils
/// `ashmem_get_prot_region` (`ASHMEM_GET_PROT_MASK`) without `PROT_WRITE`.
/// It is to ashmem what [`Seals::WRITE`] is to a memfd, which ashmem has no
/// seals to answer with. `BadValue` when `fd` is not ashmem — which is
/// always the case off Android.
#[cfg(target_os = "android")]
pub fn ashmem_write_protected(fd: BorrowedFd<'_>) -> Result<bool> {
    // `ASHMEM_GET_PROT_MASK` = `_IO(0x77, 6)`.
    Ok(ashmem_query(fd, 0x7706)? & libc::PROT_WRITE as u64 == 0)
}

/// libcutils `ashmem_get_prot_region` without `PROT_WRITE`. `BadValue` when
/// `fd` is not ashmem — which is always the case off Android.
#[cfg(not(target_os = "android"))]
pub fn ashmem_write_protected(_fd: BorrowedFd<'_>) -> Result<bool> {
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

#[cfg(all(test, target_os = "android"))]
mod ashmem_tests {
    use super::*;
    use std::os::fd::{AsFd, AsRawFd};

    /// `_IOW(0x77, nr, size_t)`: `ASHMEM_SET_SIZE` is 3, `ASHMEM_SET_PROT_MASK` is 5.
    fn set(file: &std::fs::File, nr: libc::c_ulong, value: usize) {
        let request =
            (1 << 30) | ((std::mem::size_of::<usize>() as libc::c_ulong) << 16) | 0x7700 | nr;
        // SAFETY: an open ashmem fd; both requests take their argument by value, not a pointer.
        let r = unsafe { libc::ioctl(file.as_raw_fd(), request as _, value) };
        assert_eq!(
            r,
            0,
            "ashmem request {nr}: {}",
            std::io::Error::last_os_error()
        );
    }

    /// A one-page region as libcutils `__ashmem_open` makes it, or `None` on a device without ashmem.
    fn region() -> Option<std::fs::File> {
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let open = |path: &str| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
        };
        let file = open(&format!("/dev/ashmem{}", boot_id.trim()))
            .or_else(|_| open("/dev/ashmem"))
            .ok()?;
        set(&file, 3, rustix::param::page_size());
        Some(file)
    }

    #[test]
    fn a_region_answers_its_size_and_whether_it_takes_writes() {
        let Some(file) = region() else {
            eprintln!("SKIP: no ashmem device");
            return;
        };
        let fd = file.as_fd();
        assert!(is_ashmem_fd(fd));
        assert_eq!(ashmem_size(fd), Ok(rustix::param::page_size() as u64));
        assert_eq!(ashmem_write_protected(fd), Ok(false));
        set(&file, 5, libc::PROT_READ as usize);
        assert_eq!(ashmem_write_protected(fd), Ok(true));
        eprintln!("RAN: ashmem prot mask");
    }

    #[test]
    fn a_memfd_is_not_asked() {
        let memfd = create(4096, &CreateOptions::new("not-ashmem")).unwrap();
        let not_ashmem = crate::Error::BadValue("not an ashmem fd");
        assert_eq!(ashmem_size(memfd.as_fd()), Err(not_ashmem));
        assert_eq!(ashmem_write_protected(memfd.as_fd()), Err(not_ashmem));
    }
}
