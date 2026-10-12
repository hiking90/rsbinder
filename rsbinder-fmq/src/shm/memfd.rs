// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::os::fd::{BorrowedFd, OwnedFd};

use crate::error::Result;

/// A set of memfd seals: the `F_SEAL_*` bits of `fcntl(F_ADD_SEALS)` and
/// `fcntl(F_GET_SEALS)`.
///
/// The constants carry the kernel's bit values (`include/uapi/linux/fcntl.h`),
/// which are the same on every architecture, so [`bits`](Self::bits) can go
/// on the wire or into a log as is. A bit this type has no name for is kept:
/// [`seals`] reports whatever the kernel reports.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Seals(u32);

impl Seals {
    /// `F_SEAL_SEAL`: no further seal can be added.
    pub const SEAL: Seals = Seals(0x0001);
    /// `F_SEAL_SHRINK`: the file cannot be made smaller, so a mapping of it
    /// cannot lose pages.
    pub const SHRINK: Seals = Seals(0x0002);
    /// `F_SEAL_GROW`: the file cannot be made larger.
    pub const GROW: Seals = Seals(0x0004);
    /// `F_SEAL_WRITE`: no write and no writable mapping. The kernel refuses
    /// to add it (`EBUSY`) while a shared mapping made through a writable fd
    /// exists, read-only ones included.
    pub const WRITE: Seals = Seals(0x0008);
    /// `F_SEAL_FUTURE_WRITE`: no new write and no new writable mapping;
    /// writable mappings made before the seal keep working. Linux 5.1, and
    /// the Android common kernels from 4.14 on. An older kernel answers
    /// `EINVAL` to [`add_seals`].
    pub const FUTURE_WRITE: Seals = Seals(0x0010);

    /// No seal.
    pub const fn empty() -> Self {
        Seals(0)
    }

    /// The set with exactly these kernel bits, unknown ones included.
    pub const fn from_bits(bits: u32) -> Self {
        Seals(bits)
    }

    /// The kernel's bit values.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether no seal is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every seal of `other` is in this set.
    pub const fn contains(self, other: Seals) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any seal of `other` is in this set.
    pub const fn intersects(self, other: Seals) -> bool {
        self.0 & other.0 != 0
    }

    /// The seals of either set (`|` in a `const` context).
    pub const fn union(self, other: Seals) -> Seals {
        Seals(self.0 | other.0)
    }

    /// This set without the seals of `other`.
    pub const fn difference(self, other: Seals) -> Seals {
        Seals(self.0 & !other.0)
    }
}

impl std::ops::BitOr for Seals {
    type Output = Seals;
    fn bitor(self, other: Seals) -> Seals {
        self.union(other)
    }
}

impl std::ops::BitOrAssign for Seals {
    fn bitor_assign(&mut self, other: Seals) {
        *self = self.union(other);
    }
}

impl fmt::Debug for Seals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMES: [(Seals, &str); 5] = [
            (Seals::SEAL, "SEAL"),
            (Seals::SHRINK, "SHRINK"),
            (Seals::GROW, "GROW"),
            (Seals::WRITE, "WRITE"),
            (Seals::FUTURE_WRITE, "FUTURE_WRITE"),
        ];
        let mut rest = *self;
        let mut set = f.debug_set();
        for (seal, name) in NAMES {
            if rest.contains(seal) {
                set.entry(&format_args!("{name}"));
                rest = rest.difference(seal);
            }
        }
        if !rest.is_empty() {
            set.entry(&format_args!("{:#x}", rest.bits()));
        }
        set.finish()
    }
}

/// How [`create`] makes a memfd.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct CreateOptions<'a> {
    /// The name `memfd_create` gives the file. It shows up as
    /// `/memfd:<name>` under `/proc/<pid>/fd` and in `/proc/<pid>/maps`,
    /// and has no other meaning: two memfds may share a name.
    pub name: &'a str,
    /// Allocate every page now (`fallocate`). The memory is then charged to
    /// the creating process, and a peer that touches a page for the first
    /// time cannot be the one that runs out of memory — or the one a memory
    /// cgroup kills. Without it the file is sparse and pages appear on
    /// first touch.
    pub allocate: bool,
}

impl<'a> CreateOptions<'a> {
    /// A sparse memfd named `name`.
    pub const fn new(name: &'a str) -> Self {
        Self {
            name,
            allocate: false,
        }
    }

    /// Set [`allocate`](field@Self::allocate).
    pub const fn allocate(mut self, allocate: bool) -> Self {
        self.allocate = allocate;
        self
    }
}

/// A new memfd of exactly `size` bytes that accepts seals
/// (`MFD_CLOEXEC | MFD_ALLOW_SEALING`), with none of the seals [`Seals`]
/// names applied yet. A kernel run with `vm.memfd_noexec` set adds
/// `F_SEAL_EXEC` (`0x20`) of its own, which [`seals`] then reports.
///
/// `size` is used as given; round it to the page size first when the peer
/// expects that (libfmq and `MemoryHeapBase` both do). Seals go on with
/// [`add_seals`] — after this process has made its own writable
/// [`Mapping`](super::Mapping), when [`Seals::FUTURE_WRITE`] is among them,
/// since that seal refuses the mapping too.
///
/// # Errors
///
/// The failing system call's error as [`Error::Os`](crate::Error::Os):
/// `memfd_create` (`EINVAL` for a name with an interior NUL or longer than
/// 249 bytes, `EMFILE`), `ftruncate`, or with
/// [`allocate`](field@CreateOptions::allocate) `fallocate` (`ENOMEM`/`ENOSPC` when
/// the pages cannot be had, `EINVAL` for a `size` of zero).
/// [`Error::Unsupported`](crate::Error::Unsupported) off Linux and Android.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn create(size: u64, options: &CreateOptions<'_>) -> Result<OwnedFd> {
    use rustix::fs::{FallocateFlags, MemfdFlags};
    let fd = rustix::fs::memfd_create(
        options.name,
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;
    rustix::fs::ftruncate(&fd, size)?;
    if options.allocate {
        rustix::fs::fallocate(&fd, FallocateFlags::empty(), 0, size)?;
    }
    Ok(fd)
}

/// A new memfd. [`Error::Unsupported`](crate::Error::Unsupported) on this
/// target: memfd is a Linux facility.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn create(_size: u64, _options: &CreateOptions<'_>) -> Result<OwnedFd> {
    Err(crate::Error::Unsupported)
}

/// Add `seals` to a memfd (`fcntl(F_ADD_SEALS)`).
///
/// # Errors
///
/// The kernel's answer, unchanged, as [`Error::Os`](crate::Error::Os), so
/// that the caller decides what each one means:
///
/// * `EINVAL` — the kernel does not know one of the seals (in practice
///   [`Seals::FUTURE_WRITE`] before Linux 5.1), or `fd` is a writable fd of
///   a file that has no seals (a pipe, a device). Nothing was applied. A
///   caller that can do without the unknown seal retries with
///   [`Seals::difference`]; one that cannot refuses.
/// * `EPERM` — `fd` is not open for writing (the kernel checks this first,
///   so a read-only fd answers it whatever the file is), or the file
///   carries [`Seals::SEAL`], as a memfd created without
///   `MFD_ALLOW_SEALING` does ([`seals`] names the exception).
/// * `EBUSY` — [`Seals::WRITE`] while a shared mapping made through an fd
///   that is open for writing exists, an
///   [`Access::ReadOnly`](super::Access::ReadOnly) one included: the kernel
///   counts every mapping `mprotect` could still make writable. Add the
///   seal before mapping, or unmap first.
///
/// [`Error::Unsupported`](crate::Error::Unsupported) off Linux and Android.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn add_seals(fd: BorrowedFd<'_>, seals: Seals) -> Result<()> {
    use rustix::fs::SealFlags;
    rustix::fs::fcntl_add_seals(fd, SealFlags::from_bits_retain(seals.bits()))?;
    Ok(())
}

/// Add seals to a memfd. [`Error::Unsupported`](crate::Error::Unsupported)
/// on this target.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn add_seals(_fd: BorrowedFd<'_>, _seals: Seals) -> Result<()> {
    Err(crate::Error::Unsupported)
}

/// The seals `fd` carries (`fcntl(F_GET_SEALS)`).
///
/// # Errors
///
/// `EINVAL` as [`Error::Os`](crate::Error::Os) for an fd that has no seals
/// to report: a file on a disk filesystem, a pipe, a socket, an ashmem
/// region. The kernel keeps seals for every regular file on tmpfs, not for
/// a memfd only: such a file answers [`Seals::SEAL`] alone, as a memfd
/// created without `MFD_ALLOW_SEALING` does. The exception is a memfd
/// created `MFD_NOEXEC_SEAL`, which a kernel run with `vm.memfd_noexec` set
/// makes the default: it answers `F_SEAL_EXEC` (`0x20`) in place of `SEAL`
/// and accepts further seals.
/// [`Error::Unsupported`](crate::Error::Unsupported) off Linux and Android.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn seals(fd: BorrowedFd<'_>) -> Result<Seals> {
    Ok(Seals(rustix::fs::fcntl_get_seals(fd)?.bits()))
}

/// The seals `fd` carries. [`Error::Unsupported`](crate::Error::Unsupported)
/// on this target.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn seals(_fd: BorrowedFd<'_>) -> Result<Seals> {
    Err(crate::Error::Unsupported)
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::super::{Access, Mapping};
    use super::*;
    use crate::Error;
    use rustix::io::Errno;
    use std::os::fd::AsFd;
    use std::sync::atomic::{AtomicU8, Ordering};

    fn page() -> u64 {
        rustix::param::page_size() as u64
    }

    /// `seals`, less `F_SEAL_EXEC`: a host with `vm.memfd_noexec` set puts it on every new memfd.
    fn named_seals(fd: &OwnedFd) -> Seals {
        seals(fd.as_fd())
            .unwrap()
            .difference(Seals::from_bits(0x20))
    }

    #[test]
    fn the_constants_are_the_kernel_bits() {
        use rustix::fs::SealFlags;
        for (ours, kernel) in [
            (Seals::SEAL, SealFlags::SEAL),
            (Seals::SHRINK, SealFlags::SHRINK),
            (Seals::GROW, SealFlags::GROW),
            (Seals::WRITE, SealFlags::WRITE),
            (Seals::FUTURE_WRITE, SealFlags::FUTURE_WRITE),
        ] {
            assert_eq!(ours.bits(), kernel.bits(), "{ours:?}");
        }
    }

    #[test]
    fn a_new_memfd_has_the_size_asked_for_and_no_seal() {
        let fd = create(page() + 1, &CreateOptions::new("unit")).unwrap();
        assert_eq!(rustix::fs::fstat(&fd).unwrap().st_size as u64, page() + 1);
        assert_eq!(named_seals(&fd), Seals::empty());
    }

    #[test]
    fn allocate_backs_every_page_and_the_default_backs_none() {
        let blocks = |fd: &OwnedFd| rustix::fs::fstat(fd).unwrap().st_blocks as u64 * 512;
        let sparse = create(4 * page(), &CreateOptions::new("sparse")).unwrap();
        assert_eq!(blocks(&sparse), 0);
        let full = create(4 * page(), &CreateOptions::new("full").allocate(true)).unwrap();
        // At least: a host with shmem huge pages enabled may back the range with a larger folio.
        assert!(blocks(&full) >= 4 * page(), "{}", blocks(&full));
    }

    #[test]
    fn added_seals_are_reported_and_seal_refuses_more() {
        let fd = create(page(), &CreateOptions::new("unit")).unwrap();
        add_seals(fd.as_fd(), Seals::GROW | Seals::SHRINK).unwrap();
        assert_eq!(named_seals(&fd), Seals::GROW | Seals::SHRINK);
        add_seals(fd.as_fd(), Seals::SEAL).unwrap();
        assert_eq!(
            add_seals(fd.as_fd(), Seals::WRITE).unwrap_err(),
            Error::Os(Errno::PERM)
        );
    }

    #[test]
    fn an_unknown_seal_is_einval_and_applies_nothing() {
        let fd = create(page(), &CreateOptions::new("unit")).unwrap();
        let unknown = Seals::from_bits(1 << 30);
        assert_eq!(
            add_seals(fd.as_fd(), Seals::GROW | unknown).unwrap_err(),
            Error::Os(Errno::INVAL)
        );
        assert_eq!(named_seals(&fd), Seals::empty());
    }

    #[test]
    fn future_write_keeps_the_earlier_mapping_and_refuses_a_new_writable_one() {
        let fd = create(page(), &CreateOptions::new("unit")).unwrap();
        let owner = Mapping::map(fd.as_fd(), 0, page(), Access::ReadWrite).unwrap();
        add_seals(fd.as_fd(), Seals::FUTURE_WRITE).unwrap();
        assert_eq!(
            Mapping::map(fd.as_fd(), 0, page(), Access::ReadWrite).unwrap_err(),
            Error::Os(Errno::PERM)
        );
        let reader = Mapping::map(fd.as_fd(), 0, page(), Access::ReadOnly).unwrap();
        // SAFETY: byte 0 of two live one-page mappings, reached through atomics only.
        let (stored, loaded) = unsafe {
            (
                owner.as_ptr().cast::<AtomicU8>().as_ref(),
                reader.as_ptr().cast::<AtomicU8>().as_ref(),
            )
        };
        stored.store(0x5a, Ordering::Relaxed);
        assert_eq!(loaded.load(Ordering::Relaxed), 0x5a);
    }

    #[test]
    fn write_is_ebusy_while_a_read_only_mapping_through_a_writable_fd_lives() {
        let fd = create(page(), &CreateOptions::new("unit")).unwrap();
        let reader = Mapping::map(fd.as_fd(), 0, page(), Access::ReadOnly).unwrap();
        assert_eq!(
            add_seals(fd.as_fd(), Seals::WRITE).unwrap_err(),
            Error::Os(Errno::BUSY)
        );
        drop(reader);
        add_seals(fd.as_fd(), Seals::WRITE).unwrap();
    }

    #[test]
    fn an_fd_without_seals_is_einval() {
        // Not a file: a regular tmpfs file has seals, and the test binary may sit on tmpfs.
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(seals(socket.as_fd()).unwrap_err(), Error::Os(Errno::INVAL));
    }

    #[test]
    fn debug_names_the_seals() {
        let set = Seals::GROW | Seals::FUTURE_WRITE | Seals::from_bits(0x100);
        assert_eq!(format!("{set:?}"), "{GROW, FUTURE_WRITE, 0x100}");
        assert_eq!(format!("{:?}", Seals::empty()), "{}");
    }
}
