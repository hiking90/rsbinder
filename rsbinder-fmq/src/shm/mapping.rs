// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::os::fd::BorrowedFd;
use std::ptr::NonNull;

use crate::error::{Error, Result};

/// The protection a [`Mapping`] asks `mmap` for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Access {
    /// `PROT_READ`. A store through such a mapping is a `SIGSEGV`, so only
    /// loads may be made through pointers derived from it.
    ///
    /// Of atomic loads, only some are defined on read-only memory
    /// (`core::sync::atomic`, "Atomic accesses to read-only memory"): a
    /// `Relaxed` load no wider than the limit that section gives for the
    /// target — 4 bytes on `x86` and `arm`, 8 bytes on `x86_64` and
    /// `aarch64`. The standard library promises nothing for a load with any
    /// other ordering or a wider one, since either may be compiled as a
    /// compare-exchange, which writes. [`Run::load`](super::Run::load) makes
    /// only loads within the rule. For an acquire load, make a `Relaxed`
    /// load and then call `fence(Ordering::Acquire)`. A 64-bit counter read
    /// on a 32-bit target is wider than the limit. `RoWord64` is the type
    /// for it on the 32-bit targets its documentation names; on any other
    /// the counter needs [`Access::ReadWrite`] and a target that has
    /// `AtomicU64`.
    ///
    /// Whether the kernel makes this mapping of a memfd sealed
    /// [`WRITE`](super::Seals::WRITE), through an fd that is open for
    /// writing as the one `memfd_create` returns is, depends on the kernel,
    /// and its release number alone does not say which way: Linux 5.10,
    /// 5.15, 6.1 and 6.6 as released refuse it (`EPERM`), 6.7 allows it,
    /// 6.12 as released refuses it again, 6.13 allows it, and later stable
    /// updates of 5.10, 5.15, 6.1, 6.6 and 6.12 allow it too (the Android
    /// common kernels `android12-5.10` through `android15-6.6` carry the
    /// update). A caller that maps such a memfd read-only handles `EPERM`
    /// on any device.
    /// [`FUTURE_WRITE`](super::Seals::FUTURE_WRITE) has no such limit.
    ReadOnly,
    /// `PROT_READ | PROT_WRITE`. The kernel refuses it (`EPERM`) on a memfd
    /// sealed [`WRITE`](super::Seals::WRITE) or
    /// [`FUTURE_WRITE`](super::Seals::FUTURE_WRITE), and (`EACCES`) on an fd
    /// that was not opened for writing.
    ReadWrite,
}

/// One `MAP_SHARED` mapping of a byte range of an fd, unmapped on drop.
///
/// `mmap` takes a page-aligned file offset. [`map`](Self::map) accepts any
/// offset: it maps from the page boundary below it and [`as_ptr`](Self::as_ptr)
/// points at the requested byte, so a range that starts in the middle of a
/// page (an FMQ grantor, a word at a fixed layout offset) needs no arithmetic
/// at the call site.
///
/// A mapping needs only the target's `mmap`, which macOS has as Linux and
/// Android do. On a target without one (WASI) [`map`](Self::map) returns
/// [`Error::Unsupported`].
///
/// A hugetlbfs file (a memfd created `MFD_HUGETLB`, a file on a hugetlbfs
/// mount) is refused on Linux and Android. The kernel rounds the length of
/// such a mapping up to the huge page size and then refuses (`EINVAL`) the
/// `munmap` of the length that was asked for, so dropping the `Mapping`
/// could not release it. [`map`](Self::map) asks with `fstatfs` and refuses
/// only an fd whose filesystem the kernel reports as hugetlbfs. Where the
/// `fstatfs` itself fails — SELinux denies `getattr` on that filesystem, a
/// seccomp filter answers the call with an error — the fd is mapped like
/// any other, and a mapping of a hugetlbfs file then stays until the
/// process exits.
///
/// # What this type does not guard
///
/// Handing out the address is safe; using it is not. The memory is shared
/// with whoever else holds the fd, and it stays valid only while this value
/// lives. Code that turns [`as_ptr`](Self::as_ptr) into references takes on
/// the contract of [`Run::new`](super::Run::new), which holds for an
/// `&Atomic*` it builds itself as well. For a `Mapping` that contract comes
/// to this:
///
/// * the reference covers only bytes within [`len`](Self::len) of `as_ptr`
///   and does not outlive the `Mapping`;
/// * every access this process makes to those bytes is atomic;
/// * through this one `Mapping` no byte is accessed at two different sizes
///   at once (an `AtomicU8` view and an `AtomicUsize` view of the same byte,
///   say). Unless both accesses are loads, Rust's memory model makes that
///   undefined behavior whatever the peer process does. The rule is per
///   mapping — one `mmap` result is one
///   allocation to Rust — so a second `Mapping` of the same bytes, even one
///   this process makes, counts as the peer below;
/// * through an [`Access::ReadOnly`] mapping the only atomic accesses are
///   the `Relaxed` loads that variant describes.
///
/// An `&AtomicU32` or `&AtomicU64` the code builds itself must also be
/// aligned, which `Run::new` does not ask. The mapping starts on a page
/// boundary, so the address is aligned exactly as the `offset` given to
/// [`map`](Self::map) is: the check is `offset % align_of::<T>() == 0`.
///
/// What the peer does with its own mapping of the same memory is, to Rust,
/// another thread's atomic store: it can make the contents wrong, never
/// undefined — provided every access on this side is atomic.
///
/// Truncating the file under a live mapping makes the next access a
/// `SIGBUS`. A memfd sealed [`SHRINK`](super::Seals::SHRINK) rules that out;
/// [`shrink_sealed`](super::shrink_sealed) answers whether a received fd is.
#[derive(Debug)]
pub struct Mapping {
    base: NonNull<u8>,
    len: usize,
    delta: usize,
}

// SAFETY: an address range without thread affinity; `Mapping` itself never dereferences it.
unsafe impl Send for Mapping {}
// SAFETY: as for `Send`; `&Mapping` gives out the address and the length, nothing else.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Map `extent` bytes of `fd` starting at byte `offset`.
    ///
    /// # Errors
    ///
    /// [`Error::BadValue`] when `extent` is zero or the bytes to map — from
    /// the page boundary below `offset` to the end of the range — are more
    /// than `isize::MAX`, the most a Rust slice may span, and on Linux and
    /// Android when the kernel reports `fd`'s filesystem as hugetlbfs, whose
    /// mapping `Drop` could not release (see the type's documentation);
    /// [`Error::Unsupported`] on a target without `mmap`, for an `offset`
    /// and `extent` that pass those checks; otherwise `mmap`'s error as
    /// [`Error::Os`] (`EPERM` for [`Access::ReadWrite`] on a
    /// write-sealed memfd, `EACCES` when the fd's open mode does not allow
    /// the access, `ENODEV` for an fd that cannot be mapped). A failure of
    /// the `fstatfs` that looks for hugetlbfs is not an error of this call:
    /// the fd goes on to `mmap`.
    ///
    /// A range that reaches past the end of the file maps without error on
    /// Linux and faults on first touch; check the size
    /// ([`region_size`](super::region_size)) before trusting an offset and
    /// extent that came from a peer.
    pub fn map(fd: BorrowedFd<'_>, offset: u64, extent: u64, access: Access) -> Result<Self> {
        let page = rustix::param::page_size() as u64;
        let map_offset = offset - offset % page;
        let delta = usize::try_from(offset - map_offset)
            .map_err(|_| Error::BadValue("mapping offset exceeds the address space"))?;
        let len = usize::try_from(extent)
            .ok()
            .and_then(|e| e.checked_add(delta))
            // Non-empty keeps `delta < len` for `as_ptr`; `isize::MAX` is what a slice may span.
            .filter(|len| extent > 0 && *len <= isize::MAX as usize)
            .ok_or(Error::BadValue(
                "mapping extent is zero or exceeds the address space",
            ))?;
        // The kernel maps hugetlbfs in whole huge pages; `Drop`'s `munmap` of `len` would fail.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if is_hugetlbfs(fd) {
            return Err(Error::BadValue("mapping of a hugetlbfs file"));
        }
        let base = mmap_shared(fd, len, access, map_offset)?;
        Ok(Self { base, len, delta })
    }

    /// The address of the byte at the `offset` given to [`map`](Self::map).
    /// [`len`](Self::len) bytes from here are mapped.
    #[inline]
    pub fn as_ptr(&self) -> NonNull<u8> {
        // `map` refuses an empty extent: `delta < len`, so the add never saturates.
        self.base.map_addr(|addr| addr.saturating_add(self.delta))
    }

    /// The `extent` given to [`map`](Self::map): the mapped bytes from
    /// [`as_ptr`](Self::as_ptr) on. Never zero, and at most `isize::MAX`.
    #[inline]
    #[expect(
        clippy::len_without_is_empty,
        reason = "`map` refuses an empty extent, so `is_empty` would be a constant"
    )]
    pub fn len(&self) -> usize {
        self.len - self.delta
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `mmap`'s own `base`/`len`; the type doc binds references to this value's life.
        #[cfg(not(any(
            windows,
            target_os = "espidf",
            target_os = "horizon",
            target_os = "vita",
            target_os = "wasi"
        )))]
        let _ = unsafe { rustix::mm::munmap(self.base.as_ptr().cast(), self.len) };
    }
}

/// `mmap(MAP_SHARED)` of `len > 0` bytes of `fd`, on the targets rustix has `mm` for.
#[cfg(not(any(
    windows,
    target_os = "espidf",
    target_os = "horizon",
    target_os = "vita",
    target_os = "wasi"
)))]
fn mmap_shared(
    fd: BorrowedFd<'_>,
    len: usize,
    access: Access,
    map_offset: u64,
) -> Result<NonNull<u8>> {
    use rustix::mm::{MapFlags, ProtFlags};
    let prot = match access {
        Access::ReadOnly => ProtFlags::READ,
        Access::ReadWrite => ProtFlags::READ | ProtFlags::WRITE,
    };
    // SAFETY: null hint, `len > 0`, `fd` open; the `Mapping` made of it unmaps it once, in `Drop`.
    let raw = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            len,
            prot,
            MapFlags::SHARED,
            fd,
            map_offset,
        )
    }?;
    NonNull::new(raw.cast::<u8>()).ok_or(Error::Os(rustix::io::Errno::NOMEM))
}

#[cfg(any(
    windows,
    target_os = "espidf",
    target_os = "horizon",
    target_os = "vita",
    target_os = "wasi"
))]
fn mmap_shared(
    _fd: BorrowedFd<'_>,
    _len: usize,
    _access: Access,
    _map_offset: u64,
) -> Result<NonNull<u8>> {
    Err(Error::Unsupported)
}

/// `HUGETLBFS_MAGIC` (`include/uapi/linux/magic.h`).
#[cfg(any(target_os = "linux", target_os = "android"))]
const HUGETLBFS_MAGIC: i128 = 0x9584_58f6;

/// Whether the kernel reports `fd`'s filesystem as hugetlbfs; `false` when `fstatfs` fails.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn is_hugetlbfs(fd: BorrowedFd<'_>) -> bool {
    rustix::fs::fstatfs(fd).is_ok_and(|fs| {
        // `f_type` is `i32`, `u32`, `i64` or `u64` by ABI; as `i32` this magic reads as negative.
        let f_type = i128::from(fs.f_type);
        f_type == HUGETLBFS_MAGIC || f_type == HUGETLBFS_MAGIC - (1 << 32)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::sync::atomic::{AtomicU8, Ordering};

    /// An unlinked two-page file, opened as `options` says; a plain file maps off Linux too.
    fn two_pages(options: &std::fs::OpenOptions) -> (std::fs::File, usize) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let page = rustix::param::page_size();
        let path = std::env::temp_dir().join(format!(
            "rsbinder-fmq-mapping-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, vec![0u8; 2 * page]).unwrap();
        let file = options.open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        (file, page)
    }

    fn read_write() -> std::fs::OpenOptions {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true);
        options
    }

    fn byte(at: NonNull<u8>, index: usize) -> &'static AtomicU8 {
        // SAFETY: the tests keep every index inside a live mapping and use atomics only.
        unsafe { &*at.as_ptr().add(index).cast::<AtomicU8>() }
    }

    #[test]
    fn an_offset_inside_a_page_points_at_that_byte() {
        let (file, page) = two_pages(&read_write());
        let whole = Mapping::map(file.as_fd(), 0, 2 * page as u64, Access::ReadWrite).unwrap();
        assert_eq!(whole.len(), 2 * page);
        let offset = page + 24;
        byte(whole.as_ptr(), offset).store(0x5a, Ordering::Relaxed);
        let part = Mapping::map(file.as_fd(), offset as u64, 8, Access::ReadWrite).unwrap();
        assert_eq!(part.len(), 8);
        assert_eq!(part.as_ptr().as_ptr() as usize % page, 24);
        assert_eq!(byte(part.as_ptr(), 0).load(Ordering::Relaxed), 0x5a);
        byte(part.as_ptr(), 7).store(0xa5, Ordering::Relaxed);
        assert_eq!(
            byte(whole.as_ptr(), offset + 7).load(Ordering::Relaxed),
            0xa5
        );
    }

    #[test]
    fn a_read_only_mapping_sees_what_a_writable_one_stores() {
        let (file, page) = two_pages(&read_write());
        let rw = Mapping::map(file.as_fd(), 0, page as u64, Access::ReadWrite).unwrap();
        let ro = Mapping::map(file.as_fd(), 0, page as u64, Access::ReadOnly).unwrap();
        byte(rw.as_ptr(), 3).store(7, Ordering::Relaxed);
        assert_eq!(byte(ro.as_ptr(), 3).load(Ordering::Relaxed), 7);
    }

    #[test]
    fn a_read_only_run_loads_what_a_writable_run_stores() {
        use crate::shm::{RoRun, Run};
        let (file, page) = two_pages(&read_write());
        let rw = Mapping::map(file.as_fd(), 0, page as u64, Access::ReadWrite).unwrap();
        let ro = Mapping::map(file.as_fd(), 0, page as u64, Access::ReadOnly).unwrap();
        // A head, whole words and a tail; two mappings, so each run is alone in its own.
        let (at, len) = (3, 4 * std::mem::size_of::<usize>() + 5);
        let src: Vec<u8> = (0..len).map(|i| i as u8 ^ 0x3c).collect();
        // SAFETY: `at..at + len` inside each live one-page mapping, reached through atomics only.
        let (writer, reader) = unsafe {
            (
                Run::new(rw.as_ptr().add(at), len),
                RoRun::new(ro.as_ptr().add(at), len),
            )
        };
        writer.store_release(&src);
        let mut out = vec![0u8; len];
        reader.load_acquire(&mut out);
        assert_eq!(out, src);
        out.fill(0);
        reader.load(&mut out);
        assert_eq!(out, src);
    }

    #[test]
    fn an_empty_extent_is_refused() {
        let (file, _) = two_pages(&read_write());
        let err = Mapping::map(file.as_fd(), 0, 0, Access::ReadOnly).unwrap_err();
        assert!(matches!(err, Error::BadValue(_)), "{err:?}");
    }

    #[test]
    fn an_extent_past_what_a_slice_may_span_is_refused() {
        let (file, page) = two_pages(&read_write());
        let most = isize::MAX as u64;
        for (offset, extent) in [(0, most + 1), (page as u64 + 1, most)] {
            let err = Mapping::map(file.as_fd(), offset, extent, Access::ReadOnly).unwrap_err();
            assert!(
                matches!(err, Error::BadValue(_)),
                "{offset} {extent}: {err:?}"
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_hugetlbfs_file_is_refused() {
        use rustix::fs::{memfd_create, MemfdFlags};
        use rustix::io::Errno;
        let page = rustix::param::page_size() as u64;
        let plain = memfd_create("plain", MemfdFlags::CLOEXEC).unwrap();
        rustix::fs::ftruncate(&plain, page).unwrap();
        let huge = match memfd_create("huge", MemfdFlags::CLOEXEC | MemfdFlags::HUGETLB) {
            Ok(fd) => fd,
            // No such fd to refuse: no hugetlbfs (`ENOSYS`), no huge page size, or no such flag.
            Err(Errno::NOSYS | Errno::NODEV | Errno::NOENT | Errno::INVAL) => return,
            Err(e) => panic!("memfd_create(MFD_HUGETLB): {e}"),
        };
        // Where the kernel does not answer `fstatfs` for this fd, `map` has nothing to refuse by.
        if rustix::fs::fstatfs(&huge).is_err() {
            return;
        }
        for access in [Access::ReadOnly, Access::ReadWrite] {
            let err = Mapping::map(huge.as_fd(), 0, page, access).unwrap_err();
            assert!(matches!(err, Error::BadValue(_)), "{access:?}: {err:?}");
            Mapping::map(plain.as_fd(), 0, page, access).unwrap();
        }
    }

    /// Makes `fstatfs` fail with `EACCES` on this thread for good; `false` where it still runs.
    #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
    fn deny_fstatfs() -> bool {
        // `AUDIT_ARCH_X86_64` and `AUDIT_ARCH_I386` (`include/uapi/linux/audit.h`).
        #[cfg(target_arch = "x86_64")]
        const ARCH: u32 = 0xc000_003e;
        #[cfg(target_arch = "x86")]
        const ARCH: u32 = 0x4000_0003;
        #[cfg(target_arch = "x86_64")]
        const FSTATFS: &[libc::c_long] = &[libc::SYS_fstatfs];
        #[cfg(target_arch = "x86")]
        const FSTATFS: &[libc::c_long] = &[libc::SYS_fstatfs, libc::SYS_fstatfs64];
        let op = |code: u32, jf: u8, k: u32| libc::sock_filter {
            code: code as u16,
            jt: 0,
            jf,
            k,
        };
        let load = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
        let jeq = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
        let ret = libc::BPF_RET | libc::BPF_K;
        // `seccomp_data.arch` is the word at offset 4; another one's numbers go to the last `ret`.
        let to_allow = 1 + 2 * FSTATFS.len() as u8;
        let mut filter = vec![op(load, 0, 4), op(jeq, to_allow, ARCH)];
        // `seccomp_data.nr` is the word at offset 0; each number skips its `ret` unless equal.
        filter.push(op(load, 0, 0));
        for nr in FSTATFS {
            filter.push(op(jeq, 1, *nr as u32));
            filter.push(op(ret, 0, libc::SECCOMP_RET_ERRNO | libc::EACCES as u32));
        }
        filter.push(op(ret, 0, libc::SECCOMP_RET_ALLOW));
        let program = libc::sock_fprog {
            len: filter.len() as libc::c_ushort,
            filter: filter.as_mut_ptr(),
        };
        let mode = libc::SECCOMP_MODE_FILTER as libc::c_ulong;
        let loaded = rustix::thread::set_no_new_privs(true).is_ok()
            // SAFETY: `program` points at `filter`, alive across the call; the kernel copies it.
            && unsafe { libc::prctl(libc::PR_SET_SECCOMP, mode, &program) == 0 };
        // An emulated x86 on another architecture's kernel loads the filter and matches nothing.
        loaded
            && FSTATFS.iter().all(|nr| {
                // SAFETY: fd -1 and a null buffer: denied or not, the kernel writes no memory.
                let r = unsafe { libc::syscall(*nr, -1, 0usize, 0usize) };
                r == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EACCES)
            })
    }

    // x86 only: the filter names system call numbers, which differ by architecture.
    #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn an_fd_whose_filesystem_cannot_be_queried_is_mapped() {
        use rustix::fs::{memfd_create, MemfdFlags};
        let page = rustix::param::page_size() as u64;
        let plain = memfd_create("plain", MemfdFlags::CLOEXEC).unwrap();
        rustix::fs::ftruncate(&plain, page).unwrap();
        // A thread of its own: a seccomp filter stays for the life of the thread that loads it.
        let filtered = std::thread::spawn(move || {
            // No seccomp filters, a sandbox that forbids one, or a foreign kernel: nothing staged.
            if !deny_fstatfs() {
                return;
            }
            let denied = rustix::fs::fstatfs(&plain).unwrap_err();
            assert_eq!(denied, rustix::io::Errno::ACCESS);
            for access in [Access::ReadOnly, Access::ReadWrite] {
                Mapping::map(plain.as_fd(), 0, page, access).unwrap();
            }
        });
        filtered.join().unwrap();
    }

    #[test]
    fn a_read_only_fd_refuses_a_writable_mapping() {
        let (file, page) = two_pages(std::fs::OpenOptions::new().read(true));
        let err = Mapping::map(file.as_fd(), 0, page as u64, Access::ReadWrite).unwrap_err();
        assert_eq!(err, Error::Os(rustix::io::Errno::ACCESS));
        Mapping::map(file.as_fd(), 0, page as u64, Access::ReadOnly).unwrap();
    }
}
