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
    /// Write zeros over the whole file now, so that every page is allocated
    /// and holds data. This is what [`verify_received`] asks of a peer's
    /// memfd. `allocate` alone does not give it: the kernel reports a page
    /// that `fallocate` reserved and nothing has touched yet as a hole
    /// (`lseek(SEEK_HOLE)`), as it does one that was never allocated. The
    /// cost is one pass of writes over the file's size.
    pub populate: bool,
}

impl<'a> CreateOptions<'a> {
    /// A sparse memfd named `name`.
    pub const fn new(name: &'a str) -> Self {
        Self {
            name,
            allocate: false,
            populate: false,
        }
    }

    /// Set [`allocate`](field@Self::allocate).
    pub const fn allocate(mut self, allocate: bool) -> Self {
        self.allocate = allocate;
        self
    }

    /// Set [`populate`](field@Self::populate).
    pub const fn populate(mut self, populate: bool) -> Self {
        self.populate = populate;
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
/// the pages cannot be had, `EINVAL` for a `size` of zero), or with
/// [`populate`](field@CreateOptions::populate) `pwrite` (`ENOSPC`, `ENOMEM`).
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
    if options.populate {
        // 64 KiB a call: zeros the kernel copies into each page, which marks the page as data.
        let zeros = vec![0u8; size.min(1 << 16) as usize];
        let mut at = 0;
        while at < size {
            let chunk = &zeros[..(size - at).min(zeros.len() as u64) as usize];
            match rustix::io::pwrite(&fd, chunk, at)? {
                0 => return Err(rustix::io::Errno::NOSPC.into()),
                written => at += written as u64,
            }
        }
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

/// `TMPFS_MAGIC` (`include/uapi/linux/magic.h`): the filesystem a memfd lives on.
#[cfg(any(target_os = "linux", target_os = "android"))]
const TMPFS_MAGIC: i128 = 0x0102_1994;

/// Check an fd that came from a peer before mapping it, and return the size
/// of its file: it must be a memfd whose pages are all there and can no
/// longer be taken away.
///
/// A mapping of a peer's fd is memory the peer still has a hand on. These
/// four checks leave the peer no way to take a page of that memory away: a
/// later access finds neither a shorter file nor a missing page.
///
/// 1. The file answers `F_GET_SEALS`. Only shmem files, and hugetlbfs files
///    from Linux 4.16, do, so a file on a disk or on FUSE — where touching a
///    page waits for a daemon the peer runs — stops here, as does a pipe, a
///    socket or an ashmem region.
/// 2. Its filesystem is tmpfs (`fstatfs`). A hugetlbfs memfd of Linux 4.16
///    or later passes the first check, and an access to one raises `SIGBUS`
///    when the huge page pool is empty.
/// 3. It carries [`Seals::SHRINK`] and a write seal, [`Seals::FUTURE_WRITE`]
///    or [`Seals::WRITE`]. `SHRINK` rules out a truncation, which would make
///    an access past the new end a `SIGBUS`; the write seal rules out
///    punching a hole, which the kernel treats as a write. A regular file on
///    a tmpfs mount passes the first two checks and stops here: it carries
///    [`Seals::SEAL`] alone.
/// 4. It is not empty and has no hole (`lseek(SEEK_HOLE)` finds none before
///    the end): every page holds data, in memory or swapped out. With the
///    third check that stays so, and an access never has to allocate a page
///    for a hole — which is charged to the process that touches it, and ends
///    in `SIGBUS` or the OOM killer when it fails.
///
/// The fourth check asks for more than allocation. To the kernel a page that
/// `fallocate` reserved and nothing has read or written yet is a hole too,
/// so a memfd made with [`CreateOptions::allocate`](field@CreateOptions::allocate)
/// alone is refused. The file's block count (`st_blocks`) would accept it,
/// and is not used because it also counts pages allocated past the end of
/// the file (`fallocate` with `FALLOC_FL_KEEP_SIZE`): a peer could present a
/// file that is one hole with the block count of a full one.
///
/// Make a memfd that passes with [`create`] and
/// [`CreateOptions::populate`](field@CreateOptions::populate), then
/// [`add_seals`] with [`Seals::SHRINK`] and a write seal.
///
/// The caller compares the returned size with what its protocol needs and
/// then maps; with the `SHRINK` seal the size cannot go below it afterwards.
/// What the checks do not cover: the content, which is the peer's to write
/// if it kept a writable mapping from before a `FUTURE_WRITE` seal, and a
/// page that is swapped out. An access to one waits for the swap device and
/// allocates a page — charged to the memory cgroup that owned the page when
/// it was swapped out, or to the toucher's once that cgroup is gone — and
/// raises `SIGBUS` when the read fails. The peer can put its pages there
/// itself: `madvise(MADV_PAGEOUT)` swaps out a page that no other process
/// has touched through a mapping yet. A caller that cannot accept a swap-in
/// under its access locks the mapping (`mlock`).
///
/// The fourth check moves the file offset and puts it back. The offset
/// belongs to the open file description, which the peer shares; a peer that
/// reads or writes the fd with `read`/`write` at the same moment may see it
/// moved. A memfd that is only mapped has no use for the offset.
///
/// # Errors
///
/// [`Error::BadValue`](crate::Error::BadValue), naming the check, for an fd
/// that fails one. Any other failure of a system call as
/// [`Error::Os`](crate::Error::Os): the fd is not verified, and that
/// includes an `fstatfs` the kernel denies (SELinux, a seccomp filter).
/// [`Error::Unsupported`](crate::Error::Unsupported) off Linux and Android.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn verify_received(fd: BorrowedFd<'_>) -> Result<u64> {
    use crate::Error;
    use rustix::fs::SeekFrom;
    let seals = match seals(fd) {
        Ok(seals) => seals,
        Err(Error::Os(rustix::io::Errno::INVAL)) => {
            return Err(Error::BadValue("received fd is not a sealable file"));
        }
        Err(e) => return Err(e),
    };
    if i128::from(rustix::fs::fstatfs(fd)?.f_type) != TMPFS_MAGIC {
        return Err(Error::BadValue("received fd is not a tmpfs file"));
    }
    if !seals.contains(Seals::SHRINK) || !seals.intersects(Seals::WRITE.union(Seals::FUTURE_WRITE))
    {
        return Err(Error::BadValue(
            "received memfd is not sealed against shrinking and writing",
        ));
    }
    let size = u64::try_from(rustix::fs::fstat(fd)?.st_size)
        .map_err(|_| Error::BadValue("received memfd has a negative size"))?;
    if size == 0 {
        return Err(Error::BadValue("received memfd is empty"));
    }
    let offset = rustix::fs::tell(fd)?;
    let first_hole = rustix::fs::seek(fd, SeekFrom::Hole(0));
    rustix::fs::seek(fd, SeekFrom::Start(offset))?;
    if first_hole? != size {
        return Err(Error::BadValue("received memfd has a page without data"));
    }
    Ok(size)
}

/// Check an fd that came from a peer.
/// [`Error::Unsupported`](crate::Error::Unsupported) on this target.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn verify_received(_fd: BorrowedFd<'_>) -> Result<u64> {
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

    fn byte(map: &Mapping, index: usize) -> &AtomicU8 {
        assert!(index < map.len());
        // SAFETY: a byte inside a live mapping that outlives the borrow; tests use atomics only.
        unsafe { map.as_ptr().add(index).cast::<AtomicU8>().as_ref() }
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
        byte(&owner, 0).store(0x5a, Ordering::Relaxed);
        assert_eq!(byte(&reader, 0).load(Ordering::Relaxed), 0x5a);
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

    const PEER_SEALS: Seals = Seals::SHRINK.union(Seals::FUTURE_WRITE);

    /// The check that refused `fd`, by the name `verify_received` gives it.
    fn refusal(fd: BorrowedFd<'_>) -> &'static str {
        match verify_received(fd).unwrap_err() {
            Error::BadValue(check) => check,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_populated_sealed_memfd_is_accepted_and_its_offset_kept() {
        // More than one `pwrite` chunk, and an end inside a page.
        let size = (1 << 16) + 3 * page() + 128;
        let fd = create(size, &CreateOptions::new("unit").populate(true)).unwrap();
        add_seals(fd.as_fd(), PEER_SEALS).unwrap();
        rustix::fs::seek(&fd, rustix::fs::SeekFrom::Start(77)).unwrap();
        assert_eq!(verify_received(fd.as_fd()).unwrap(), size);
        assert_eq!(rustix::fs::tell(&fd).unwrap(), 77);
        // `WRITE` is a write seal too.
        let fd = create(page(), &CreateOptions::new("unit").populate(true)).unwrap();
        add_seals(fd.as_fd(), Seals::SHRINK | Seals::WRITE).unwrap();
        assert_eq!(verify_received(fd.as_fd()).unwrap(), page());
    }

    #[test]
    fn pages_that_were_only_reserved_are_refused() {
        let no_data = "received memfd has a page without data";
        let fd = create(4 * page(), &CreateOptions::new("unit").allocate(true)).unwrap();
        add_seals(fd.as_fd(), PEER_SEALS).unwrap();
        assert_eq!(refusal(fd.as_fd()), no_data);
        // Blocks past the end give a file that is one hole the block count of a full one.
        let fd = create(4 * page(), &CreateOptions::new("unit")).unwrap();
        let keep_size = rustix::fs::FallocateFlags::KEEP_SIZE;
        rustix::fs::fallocate(&fd, keep_size, 4 * page(), 4 * page()).unwrap();
        let st = rustix::fs::fstat(&fd).unwrap();
        assert!(st.st_blocks as u64 * 512 >= st.st_size as u64, "{st:?}");
        add_seals(fd.as_fd(), PEER_SEALS).unwrap();
        assert_eq!(refusal(fd.as_fd()), no_data);
    }

    #[test]
    fn pages_that_were_written_count_as_allocated() {
        let fd = create(2 * page(), &CreateOptions::new("unit")).unwrap();
        let owner = Mapping::map(fd.as_fd(), 0, 2 * page(), Access::ReadWrite).unwrap();
        for index in [0, page() as usize] {
            byte(&owner, index).store(1, Ordering::Relaxed);
        }
        add_seals(fd.as_fd(), PEER_SEALS).unwrap();
        assert_eq!(verify_received(fd.as_fd()).unwrap(), 2 * page());
    }

    #[test]
    fn a_memfd_with_a_hole_is_refused() {
        // Sparse: no page yet.
        let sparse = create(4 * page(), &CreateOptions::new("unit")).unwrap();
        add_seals(sparse.as_fd(), PEER_SEALS).unwrap();
        assert_eq!(
            refusal(sparse.as_fd()),
            "received memfd has a page without data"
        );
        // Pages 0, 1 and 3 written, page 2 left out.
        let fd = create(4 * page(), &CreateOptions::new("unit")).unwrap();
        let owner = Mapping::map(fd.as_fd(), 0, 4 * page(), Access::ReadWrite).unwrap();
        for index in [0, 1, 3] {
            byte(&owner, index * page() as usize).store(1, Ordering::Relaxed);
        }
        add_seals(fd.as_fd(), PEER_SEALS).unwrap();
        assert_eq!(
            refusal(fd.as_fd()),
            "received memfd has a page without data"
        );
    }

    #[test]
    fn a_memfd_without_the_two_seals_is_refused() {
        let unsealed = "received memfd is not sealed against shrinking and writing";
        for seals in [
            Seals::empty(),
            Seals::SHRINK,
            Seals::FUTURE_WRITE,
            Seals::GROW | Seals::SEAL,
        ] {
            let fd = create(page(), &CreateOptions::new("unit").populate(true)).unwrap();
            add_seals(fd.as_fd(), seals).unwrap();
            assert_eq!(refusal(fd.as_fd()), unsealed, "{seals:?}");
        }
    }

    #[test]
    fn an_empty_memfd_is_refused() {
        let fd = rustix::fs::memfd_create(
            "empty",
            rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
        )
        .unwrap();
        add_seals(fd.as_fd(), PEER_SEALS).unwrap();
        assert_eq!(refusal(fd.as_fd()), "received memfd is empty");
    }

    #[test]
    fn an_fd_that_is_not_a_memfd_is_refused() {
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(
            refusal(socket.as_fd()),
            "received fd is not a sealable file"
        );
        // A regular file: no seals on a disk, `SEAL` alone on a tmpfs mount.
        let path = std::env::temp_dir().join(format!("rsbinder-fmq-verify-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; page() as usize]).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let check = refusal(file.as_fd());
        assert!(
            [
                "received fd is not a sealable file",
                "received fd is not a tmpfs file",
                "received memfd is not sealed against shrinking and writing",
            ]
            .contains(&check),
            "{check}"
        );
    }

    #[test]
    fn a_hugetlbfs_memfd_is_refused_as_not_tmpfs() {
        use rustix::fs::{memfd_create, MemfdFlags};
        let huge = match memfd_create("huge", MemfdFlags::CLOEXEC | MemfdFlags::HUGETLB) {
            Ok(fd) => fd,
            // No such fd to refuse: no hugetlbfs (`ENOSYS`), no huge page size, or no such flag.
            Err(Errno::NOSYS | Errno::NODEV | Errno::NOENT | Errno::INVAL) => {
                eprintln!("skipped: this kernel makes no MFD_HUGETLB memfd");
                return;
            }
            Err(e) => panic!("memfd_create(MFD_HUGETLB): {e}"),
        };
        // Seals on a hugetlbfs memfd came with Linux 4.16; before, it is refused as unsealable.
        if let Err(Error::Os(Errno::INVAL)) = seals(huge.as_fd()) {
            assert_eq!(refusal(huge.as_fd()), "received fd is not a sealable file");
            return;
        }
        // It has seals to report, which is why the filesystem is asked as well.
        seals(huge.as_fd()).unwrap();
        assert_eq!(refusal(huge.as_fd()), "received fd is not a tmpfs file");
    }
}
