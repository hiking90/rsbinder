// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Concrete shared-memory heaps (Plan 4-7a Phase A).
//!
//! * [`MemoryHeapBase`] — the *owner* side. Allocates an anonymous
//!   shared region and (by default) maps it into this process.
//! * [`MappedHeap`] — the *receiver* side. Wraps an fd that arrived
//!   through a parcel plus the wire geometry (`size`, `offset`,
//!   `flags`) and maps it.
//!
//! Backing store by target:
//!
//! | target | create | size guard |
//! |---|---|---|
//! | linux / android | `memfd_create(MFD_CLOEXEC \| MFD_ALLOW_SEALING)` | `F_SEAL_GROW \| F_SEAL_SHRINK` (+`F_SEAL_FUTURE_WRITE` for read-only, +`F_SEAL_SEAL` unless [`FLAG_MEMFD_ALLOW_SEALING`]) — the AOSP `MemoryHeapBase::FORCE_MEMFD` path |
//! | macos | `shm_open` + immediate `shm_unlink` (two fds: `O_RDWR` for the owner, `O_RDONLY` to export) | kernel-inherent: a POSIX shm object accepts exactly **one** `ftruncate` (≡ `F_SEAL_GROW \| F_SEAL_SHRINK`), and an `O_RDONLY` fd refuses `PROT_WRITE` mappings and `mprotect` upgrades (≡ `F_SEAL_FUTURE_WRITE`). See `plan/4-7b-macos-shared-memory.md` |
//! | other | unsupported — every constructor returns `InvalidOperation` | — |
//!
//! The owner mapping is always created *before* seals are applied so
//! that `F_SEAL_FUTURE_WRITE` leaves the owner's own mapping writable,
//! exactly like AOSP `MemoryHeapBase.cpp`. Receivers map `PROT_READ`
//! only when [`FLAG_READ_ONLY`] is set — and on every backend the
//! kernel refuses a writable mapping to a peer that ignores the flag.
//!
//! `seals()` reports the **effective protections** of the fd a heap
//! exports as `SEAL_*` bits: `F_GET_SEALS` on Linux/Android, synthesized
//! from the kernel semantics above on macOS (`SEAL_SHRINK | SEAL_GROW`,
//! plus `SEAL_FUTURE_WRITE` when the exported fd is read-only; never
//! `SEAL_SEAL`).
//!
//! **Aliasing.** A shared mapping is, by definition, writable by other
//! processes at any time. The safe accessors [`read_at`](MemoryHeapBase::read_at)
//! / [`write_at`](MemoryHeapBase::write_at) copy through raw pointers
//! and are always sound; [`IMemoryHeap::base`] hands out `&[u8]` for
//! convenience and is only sound while no other party writes to the
//! region for the lifetime of the borrow (the same caveat AOSP attaches
//! to `IMemory::unsecurePointer()`).

use std::os::fd::{AsFd, OwnedFd};
use std::ptr::NonNull;
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicU32;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{IMemoryHeap, FLAG_READ_ONLY};
use crate::error::{Result, StatusCode};
use crate::file_descriptor::ParcelFileDescriptor;

/// AOSP `MemoryHeapBase::DONT_MAP_LOCALLY`: allocate the region but do
/// not map it into the owning process.
pub const FLAG_DONT_MAP_LOCALLY: u32 = 0x0000_0100;
/// AOSP `MemoryHeapBase::NO_CACHING`. Accepted for flag parity; it only
/// affects device-backed heaps, which rsbinder does not provide.
pub const FLAG_NO_CACHING: u32 = 0x0000_0200;
/// AOSP `MemoryHeapBase::FORCE_MEMFD`. Accepted for flag parity; every
/// rsbinder heap on Linux/Android is memfd-backed, so this is a no-op.
pub const FLAG_FORCE_MEMFD: u32 = 0x0000_0400;
/// AOSP `MemoryHeapBase::MEMFD_ALLOW_SEALING_FLAG`: leave the memfd
/// sealable by later holders (omit `F_SEAL_SEAL`).
pub const FLAG_MEMFD_ALLOW_SEALING: u32 = 0x0000_0800;

/// `F_SEAL_SEAL` bit as reported by [`MemoryHeapBase::seals`] (Linux/Android only).
pub const SEAL_SEAL: u32 = 0x0001;
/// `F_SEAL_SHRINK`.
pub const SEAL_SHRINK: u32 = 0x0002;
/// `F_SEAL_GROW`.
pub const SEAL_GROW: u32 = 0x0004;
/// `F_SEAL_WRITE`.
pub const SEAL_WRITE: u32 = 0x0008;
/// `F_SEAL_FUTURE_WRITE` (Linux 5.1+).
pub const SEAL_FUTURE_WRITE: u32 = 0x0010;

#[cfg(target_os = "macos")]
static HEAP_COUNTER: AtomicU32 = AtomicU32::new(0);

fn page_size() -> usize {
    rustix::param::page_size()
}

fn round_up_to_page(size: usize) -> Option<usize> {
    let ps = page_size();
    size.checked_add(ps - 1).map(|s| s & !(ps - 1))
}

/// One `mmap` region; unmapped on drop.
struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

impl Mapping {
    fn map(fd: &OwnedFd, len: usize, offset: usize, writable: bool) -> Result<Self> {
        use rustix::mm::{MapFlags, ProtFlags};
        if len == 0 {
            return Err(StatusCode::BadValue);
        }
        let mut prot = ProtFlags::READ;
        if writable {
            prot |= ProtFlags::WRITE;
        }
        // SAFETY: `ptr` is null (kernel picks the address), `len` is
        // non-zero, and `fd` is a live fd owned by the caller for the
        // duration of the call. The returned region is owned by this
        // `Mapping` and unmapped exactly once in `Drop`.
        let raw = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                prot,
                MapFlags::SHARED,
                fd,
                offset as u64,
            )
        }?;
        let ptr = NonNull::new(raw.cast::<u8>()).ok_or(StatusCode::NoMemory)?;
        Ok(Self { ptr, len })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` came from a successful `mmap` in
        // `Mapping::map` and nothing else unmaps them.
        let _ = unsafe { rustix::mm::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// Shared state behind both heap types.
struct HeapInner {
    fd: OwnedFd,
    /// macOS owner only: the `O_RDONLY` handle on the same object,
    /// exported instead of `fd` once the heap is read-only for peers.
    ro_fd: Option<OwnedFd>,
    /// Export the read-only handle (macOS) — set by `FLAG_READ_ONLY` at
    /// construction or by a later `seal_future_write`.
    export_ro: AtomicBool,
    map: Option<Mapping>,
    size: usize,
    offset: usize,
    flags: u32,
    writable: bool,
}

// SAFETY: the mapping is plain shared memory with no thread affinity;
// the raw pointer is only dereferenced through bounds-checked copies
// (or the documented `base()` borrow).
unsafe impl Send for HeapInner {}
unsafe impl Sync for HeapInner {}

impl HeapInner {
    fn check_range(&self, off: usize, len: usize) -> Result<NonNull<u8>> {
        let map = self.map.as_ref().ok_or(StatusCode::InvalidOperation)?;
        let end = off.checked_add(len).ok_or(StatusCode::BadValue)?;
        if end > map.len {
            return Err(StatusCode::BadValue);
        }
        // SAFETY: `off <= map.len` was just verified, so the offset stays
        // inside (or one-past) the mapped allocation.
        Ok(unsafe { NonNull::new_unchecked(map.ptr.as_ptr().add(off)) })
    }

    fn read_at(&self, off: usize, dst: &mut [u8]) -> Result<()> {
        let src = self.check_range(off, dst.len())?;
        // SAFETY: `src..src+len` is inside the live mapping (checked
        // above) and `dst` is a distinct Rust buffer, so the ranges do
        // not overlap.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), dst.len()) };
        Ok(())
    }

    fn write_at(&self, off: usize, src: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(StatusCode::PermissionDenied);
        }
        let dst = self.check_range(off, src.len())?;
        // SAFETY: as in `read_at`, with the mapping created `PROT_WRITE`
        // (guarded by `writable`).
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_ptr(), src.len()) };
        Ok(())
    }

    fn base(&self) -> Option<&[u8]> {
        // SAFETY: the mapping is live for `&self`; see the module doc
        // for the single-writer caveat this borrow carries.
        self.map
            .as_ref()
            .map(|m| unsafe { std::slice::from_raw_parts(m.ptr.as_ptr(), m.len) })
    }

    fn as_ptr(&self) -> Option<*mut u8> {
        self.map.as_ref().map(|m| m.ptr.as_ptr())
    }

    /// The fd peers receive: the read-only handle when one exists and
    /// the heap is read-only for peers, else the primary fd.
    fn export_fd(&self) -> &OwnedFd {
        match &self.ro_fd {
            Some(ro) if self.export_ro.load(Ordering::Relaxed) => ro,
            _ => &self.fd,
        }
    }

    fn to_parcel_fd(&self) -> Result<ParcelFileDescriptor> {
        Ok(ParcelFileDescriptor::new(self.export_fd().try_clone()?))
    }

    fn seals(&self) -> Option<u32> {
        backend::get_seals(self.export_fd())
    }

    /// Make every *future* exported mapping read-only
    /// (`F_SEAL_FUTURE_WRITE` on Linux/Android; switch to the
    /// `O_RDONLY` handle on macOS). Existing mappings stay as they are.
    fn seal_future_write(&self) -> Result<()> {
        backend::seal_future_write(&self.fd, self.ro_fd.as_ref())?;
        self.export_ro.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// AOSP `getHeapID()` — the fd that goes on the wire, i.e. the
    /// exported (possibly read-only) handle.
    fn impl_heap_id(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.export_fd().as_raw_fd()
    }
}

// ---------------------------------------------------------------------
// Platform backends
// ---------------------------------------------------------------------

/// What a backend allocates: the owner's fd plus, where the platform
/// expresses read-only as a separate handle (macOS), an `O_RDONLY` fd
/// on the same object.
struct Created {
    fd: OwnedFd,
    ro_fd: Option<OwnedFd>,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod backend {
    use super::*;
    use rustix::fs::{MemfdFlags, SealFlags};

    pub(super) fn create(name: &str, size: usize) -> Result<Created> {
        let fd = rustix::fs::memfd_create(name, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)?;
        rustix::fs::ftruncate(&fd, size as u64)?;
        Ok(Created { fd, ro_fd: None })
    }

    pub(super) fn seal_future_write(fd: &OwnedFd, _ro: Option<&OwnedFd>) -> Result<()> {
        rustix::fs::fcntl_add_seals(fd, SealFlags::FUTURE_WRITE)?;
        Ok(())
    }

    /// Apply the AOSP `MemoryHeapBase` seal set. Must run *after* the
    /// owner mapping exists so `F_SEAL_FUTURE_WRITE` does not revoke it.
    pub(super) fn seal(fd: &OwnedFd, flags: u32) -> Result<()> {
        let mut seals = SealFlags::GROW | SealFlags::SHRINK;
        if flags & FLAG_READ_ONLY != 0 {
            seals |= SealFlags::FUTURE_WRITE;
        }
        if flags & FLAG_MEMFD_ALLOW_SEALING == 0 {
            seals |= SealFlags::SEAL;
        }
        match rustix::fs::fcntl_add_seals(fd, seals) {
            Ok(()) => Ok(()),
            // Pre-5.1 kernels reject F_SEAL_FUTURE_WRITE with EINVAL;
            // fall back to the remaining seals (degraded read-only
            // enforcement, observable via `seals()`).
            Err(rustix::io::Errno::INVAL) if seals.contains(SealFlags::FUTURE_WRITE) => {
                log::warn!("memfd F_SEAL_FUTURE_WRITE unsupported by this kernel; read-only heap is not write-sealed");
                rustix::fs::fcntl_add_seals(fd, seals - SealFlags::FUTURE_WRITE)?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    pub(super) fn get_seals(fd: &OwnedFd) -> Option<u32> {
        rustix::fs::fcntl_get_seals(fd).ok().map(|s| s.bits())
    }
}

#[cfg(target_os = "macos")]
mod backend {
    use super::*;
    use rustix::fs::Mode;
    use rustix::shm::OFlags;

    /// Opens the object twice — `O_RDWR` for the owner's mapping and
    /// `O_RDONLY` to hand to read-only peers — then unlinks the name.
    pub(super) fn create(name: &str, size: usize) -> Result<Created> {
        // PSHMNAMLEN = 31 on darwin; names are "/rsb<pid>-<ctr>-<tag>"
        // with the tag reduced to ASCII alphanumerics so truncation
        // never lands inside a multi-byte char.
        let pid = rustix::process::getpid().as_raw_nonzero().get();
        let tag: String = name.chars().filter(char::is_ascii_alphanumeric).collect();
        let mut last = StatusCode::Unknown;
        for _ in 0..3 {
            let ctr = HEAP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut shm_name = format!("/rsb{pid}-{ctr}-{tag}");
            shm_name.truncate(31);
            let fd = match rustix::shm::open(
                shm_name.as_str(),
                OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
                Mode::RUSR | Mode::WUSR,
            ) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::EXIST) => {
                    last = StatusCode::AlreadyExists;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            // The RO re-open must precede unlink; EXCL above guarantees
            // the name is ours for that window.
            let ro = rustix::shm::open(shm_name.as_str(), OFlags::RDONLY, Mode::empty());
            let _ = rustix::shm::unlink(shm_name.as_str());
            let ro = ro?;
            // The one and only size set the kernel allows on this object.
            rustix::fs::ftruncate(&fd, size as u64)?;
            return Ok(Created {
                fd,
                ro_fd: Some(ro),
            });
        }
        Err(last)
    }

    /// Size is fixed by the kernel after the single `ftruncate`; the
    /// read-only export is selected per fd, so nothing to do here.
    pub(super) fn seal(_fd: &OwnedFd, _flags: u32) -> Result<()> {
        Ok(())
    }

    /// An owner switches its export to the `O_RDONLY` handle; a received
    /// fd cannot be re-opened, but one that is already read-only is
    /// trivially sealed (idempotent, like `F_ADD_SEALS`).
    pub(super) fn seal_future_write(fd: &OwnedFd, ro: Option<&OwnedFd>) -> Result<()> {
        if ro.is_some() || is_read_only_fd(fd) {
            Ok(())
        } else {
            Err(StatusCode::InvalidOperation)
        }
    }

    /// `O_ACCMODE == O_RDONLY` — the darwin equivalent of a write seal.
    pub(super) fn is_read_only_fd<F: AsFd>(fd: F) -> bool {
        use rustix::fs::OFlags;
        rustix::fs::fcntl_getfl(fd)
            .map(|fl| fl & OFlags::ACCMODE == OFlags::RDONLY)
            .unwrap_or(false)
    }

    /// Effective protections synthesized from darwin semantics (plan
    /// 4-7b §1): a POSIX shm object's size is immutable after creation,
    /// and an `O_RDONLY` fd cannot be mapped or upgraded to `PROT_WRITE`.
    /// Only a shm object qualifies — it has no vnode, so `st_mode` carries
    /// no `S_IFMT` bits; a regular file (resizable by any holder) yields
    /// `None`, matching `F_GET_SEALS` failing on non-memfd fds on Linux.
    pub(super) fn get_seals(fd: &OwnedFd) -> Option<u32> {
        let st = rustix::fs::fstat(fd).ok()?;
        if u32::from(st.st_mode) & u32::from(libc::S_IFMT) != 0 {
            return None;
        }
        let mut seals = SEAL_SHRINK | SEAL_GROW;
        if is_read_only_fd(fd) {
            seals |= SEAL_FUTURE_WRITE;
        }
        Some(seals)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
mod backend {
    use super::*;

    pub(super) fn create(_name: &str, _size: usize) -> Result<Created> {
        Err(StatusCode::InvalidOperation)
    }
    pub(super) fn seal(_fd: &OwnedFd, _flags: u32) -> Result<()> {
        Err(StatusCode::InvalidOperation)
    }
    pub(super) fn seal_future_write(_fd: &OwnedFd, _ro: Option<&OwnedFd>) -> Result<()> {
        Err(StatusCode::InvalidOperation)
    }
    pub(super) fn get_seals(_fd: &OwnedFd) -> Option<u32> {
        None
    }
}

/// macOS: whether `fd` is an `O_RDONLY` handle (the darwin write seal).
#[cfg(target_os = "macos")]
pub(super) fn is_read_only_fd<F: AsFd>(fd: F) -> bool {
    backend::is_read_only_fd(fd)
}

/// Whether this build has a shared-memory backing store at all.
pub const fn is_supported() -> bool {
    cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos"
    ))
}

// ---------------------------------------------------------------------
// MemoryHeapBase — owner side
// ---------------------------------------------------------------------

/// Owner-side heap: AOSP `MemoryHeapBase` (anonymous / `FORCE_MEMFD`
/// constructor). See the [module doc](self) for the per-target backing
/// store and the aliasing contract.
///
/// `size` is rounded up to a whole number of pages, as in AOSP. The
/// owner's own mapping is always writable, even with [`FLAG_READ_ONLY`]
/// — that flag restricts *receivers* (and, on Linux/Android, any future
/// mapping via `F_SEAL_FUTURE_WRITE`).
pub struct MemoryHeapBase(HeapInner);

impl std::fmt::Debug for MemoryHeapBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryHeapBase")
            .field("fd", &self.0.impl_heap_id())
            .field("size", &self.0.size)
            .field("flags", &format_args!("{:#x}", self.0.flags))
            .field("mapped", &self.0.map.is_some())
            .finish()
    }
}

impl MemoryHeapBase {
    /// Allocate a `size`-byte (page-rounded) shared region named
    /// `"MemoryHeapBase"` (the AOSP default name).
    ///
    /// Errors: `BadValue` for `size == 0` or overflow; `InvalidOperation`
    /// on targets with no backing store (see [`is_supported`]); the
    /// mapped OS error otherwise.
    pub fn new(size: usize, flags: u32) -> Result<Self> {
        Self::new_named(size, flags, "MemoryHeapBase")
    }

    /// [`new`](Self::new) with an explicit debug name (shows up in
    /// `/proc/<pid>/fd` as `/memfd:<name>` on Linux).
    pub fn new_named(size: usize, flags: u32, name: &str) -> Result<Self> {
        if size == 0 {
            return Err(StatusCode::BadValue);
        }
        let size = round_up_to_page(size).ok_or(StatusCode::BadValue)?;
        let Created { fd, ro_fd } = backend::create(name, size)?;
        let map = if flags & FLAG_DONT_MAP_LOCALLY == 0 {
            Some(Mapping::map(&fd, size, 0, true)?)
        } else {
            None
        };
        backend::seal(&fd, flags)?;
        Ok(Self(HeapInner {
            fd,
            ro_fd,
            export_ro: AtomicBool::new(flags & FLAG_READ_ONLY != 0),
            map,
            size,
            offset: 0,
            flags,
            writable: true,
        }))
    }

    /// Raw base pointer of the local mapping (`None` with
    /// [`FLAG_DONT_MAP_LOCALLY`]). AOSP `unsecurePointer()` equivalent —
    /// the caller owns all synchronisation with other mappers.
    pub fn as_ptr(&self) -> Option<*mut u8> {
        self.0.as_ptr()
    }

    /// Copy `dst.len()` bytes out of the region starting at `off`.
    /// `BadValue` if the range is out of bounds, `InvalidOperation` if
    /// the heap is not mapped locally.
    pub fn read_at(&self, off: usize, dst: &mut [u8]) -> Result<()> {
        self.0.read_at(off, dst)
    }

    /// Copy `src` into the region at `off`. Same errors as
    /// [`read_at`](Self::read_at).
    pub fn write_at(&self, off: usize, src: &[u8]) -> Result<()> {
        self.0.write_at(off, src)
    }

    /// Effective protections (`SEAL_*` bits) of the fd this heap
    /// exports; `None` only if the query fails. Linux/Android read
    /// `F_GET_SEALS`; macOS synthesizes them (see the module doc).
    pub fn seals(&self) -> Option<u32> {
        self.0.seals()
    }

    /// Make every mapping created from fds exported **after** this
    /// call read-only (`F_SEAL_FUTURE_WRITE` on Linux/Android; the
    /// `O_RDONLY` handle on macOS). The owner's own mapping stays
    /// writable. Unlike Linux, macOS cannot revoke an `O_RDWR` fd that
    /// was already handed out — create the heap with
    /// [`FLAG_READ_ONLY`] when peers must never write.
    pub fn seal_future_write(&self) -> Result<()> {
        self.0.seal_future_write()
    }

    /// `dup` the heap fd (`F_DUPFD_CLOEXEC`) into a parcelable handle —
    /// the read-only handle when the heap is read-only for peers. The
    /// heap keeps its own fd; the returned one can be written to a
    /// parcel and is closed by the receiver.
    pub fn to_parcel_fd(&self) -> Result<ParcelFileDescriptor> {
        self.0.to_parcel_fd()
    }
}

/// The fd peers receive (see [`MemoryHeapBase::to_parcel_fd`]) — on
/// macOS the `O_RDONLY` handle for a read-only heap, never the owner's
/// writable one.
impl AsFd for MemoryHeapBase {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.export_fd().as_fd()
    }
}

impl IMemoryHeap for MemoryHeapBase {
    fn heap_id(&self) -> i32 {
        self.0.impl_heap_id()
    }
    fn size(&self) -> usize {
        self.0.size
    }
    fn flags(&self) -> u32 {
        self.0.flags
    }
    fn offset(&self) -> usize {
        self.0.offset
    }
    fn base(&self) -> Option<&[u8]> {
        self.0.base()
    }
}

// ---------------------------------------------------------------------
// MappedHeap — receiver side
// ---------------------------------------------------------------------

/// Receiver-side heap: an fd that arrived through a parcel, mapped with
/// the wire geometry. AOSP `BpMemoryHeap::assertReallyMapped()`
/// equivalent (minus the transaction, which lives in the wire layer).
///
/// The mapping is `PROT_READ` when [`FLAG_READ_ONLY`] is set, else
/// read/write — the flag is taken at face value, as AOSP does. Use
/// [`from_fd_strict`](Self::from_fd_strict) to additionally require the
/// sender to have write-sealed a read-only fd.
pub struct MappedHeap(HeapInner);

impl std::fmt::Debug for MappedHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedHeap")
            .field("fd", &self.0.impl_heap_id())
            .field("size", &self.0.size)
            .field("offset", &self.0.offset)
            .field("flags", &format_args!("{:#x}", self.0.flags))
            .finish()
    }
}

impl MappedHeap {
    /// Map `size` bytes at `offset` of `fd`. `offset` must be
    /// page-aligned and `size` non-zero (`BadValue` otherwise). When the
    /// fd reports a non-zero `st_size` (memfd, shm) the range must fit
    /// inside it; ashmem char-device fds report `0` and are trusted.
    pub fn from_fd(fd: OwnedFd, size: usize, offset: usize, flags: u32) -> Result<Self> {
        if size == 0 || offset % page_size() != 0 {
            return Err(StatusCode::BadValue);
        }
        let end = offset.checked_add(size).ok_or(StatusCode::BadValue)?;
        let st_size = rustix::fs::fstat(&fd)?.st_size;
        if st_size > 0 && (end as u64) > st_size as u64 {
            return Err(StatusCode::BadValue);
        }
        let writable = flags & FLAG_READ_ONLY == 0;
        let map = Mapping::map(&fd, size, offset, writable)?;
        Ok(Self(HeapInner {
            fd,
            ro_fd: None,
            export_ro: AtomicBool::new(false),
            map: Some(map),
            size,
            offset,
            flags,
            writable,
        }))
    }

    /// [`from_fd`](Self::from_fd) plus protection verification: the fd
    /// must be shrink-protected, and a [`FLAG_READ_ONLY`] heap must
    /// also be write-protected (`F_SEAL_WRITE` / `F_SEAL_FUTURE_WRITE`
    /// on Linux/Android, an `O_RDONLY` fd on macOS). `BadValue` when
    /// the sender's claims are not backed by the kernel;
    /// `InvalidOperation` on targets with no backing store.
    pub fn from_fd_strict(fd: OwnedFd, size: usize, offset: usize, flags: u32) -> Result<Self> {
        let seals = backend::get_seals(&fd).ok_or(StatusCode::InvalidOperation)?;
        if seals & SEAL_SHRINK == 0 {
            return Err(StatusCode::BadValue);
        }
        if flags & FLAG_READ_ONLY != 0 && seals & (SEAL_WRITE | SEAL_FUTURE_WRITE) == 0 {
            return Err(StatusCode::BadValue);
        }
        Self::from_fd(fd, size, offset, flags)
    }

    /// Consume a parcel-received fd. See [`from_fd`](Self::from_fd).
    pub fn from_parcel_fd(
        pfd: ParcelFileDescriptor,
        size: usize,
        offset: usize,
        flags: u32,
    ) -> Result<Self> {
        Self::from_fd(OwnedFd::from(pfd), size, offset, flags)
    }

    /// Raw base pointer of the mapping. See [`MemoryHeapBase::as_ptr`].
    pub fn as_ptr(&self) -> Option<*mut u8> {
        self.0.as_ptr()
    }

    /// See [`MemoryHeapBase::read_at`].
    pub fn read_at(&self, off: usize, dst: &mut [u8]) -> Result<()> {
        self.0.read_at(off, dst)
    }

    /// See [`MemoryHeapBase::write_at`]. `PermissionDenied` on a
    /// [`FLAG_READ_ONLY`] heap.
    pub fn write_at(&self, off: usize, src: &[u8]) -> Result<()> {
        self.0.write_at(off, src)
    }

    /// See [`MemoryHeapBase::seals`].
    pub fn seals(&self) -> Option<u32> {
        self.0.seals()
    }

    /// See [`MemoryHeapBase::seal_future_write`]. On macOS a received
    /// fd cannot be re-opened read-only, so this is `InvalidOperation`
    /// there.
    pub fn seal_future_write(&self) -> Result<()> {
        self.0.seal_future_write()
    }

    /// See [`MemoryHeapBase::to_parcel_fd`].
    pub fn to_parcel_fd(&self) -> Result<ParcelFileDescriptor> {
        self.0.to_parcel_fd()
    }
}

/// The fd peers receive (see [`MappedHeap::to_parcel_fd`]).
impl AsFd for MappedHeap {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.export_fd().as_fd()
    }
}

impl IMemoryHeap for MappedHeap {
    fn heap_id(&self) -> i32 {
        self.0.impl_heap_id()
    }
    fn size(&self) -> usize {
        self.0.size
    }
    fn flags(&self) -> u32 {
        self.0.flags
    }
    fn offset(&self) -> usize {
        self.0.offset
    }
    fn base(&self) -> Option<&[u8]> {
        self.0.base()
    }
}

#[cfg(all(
    test,
    any(target_os = "linux", target_os = "android", target_os = "macos")
))]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn new_rounds_size_up_to_page_and_maps() {
        let h = MemoryHeapBase::new(100, 0).unwrap();
        assert_eq!(h.size(), page_size());
        assert_eq!(h.offset(), 0);
        assert!(h.as_ptr().is_some());
        assert_eq!(h.base().unwrap().len(), page_size());
        assert!(h.heap_id() >= 0);
    }

    #[test]
    fn zero_size_is_bad_value() {
        assert_eq!(MemoryHeapBase::new(0, 0).unwrap_err(), StatusCode::BadValue);
    }

    #[test]
    fn dont_map_locally_skips_mapping() {
        let h = MemoryHeapBase::new(4096, FLAG_DONT_MAP_LOCALLY).unwrap();
        assert!(h.as_ptr().is_none());
        assert!(h.base().is_none());
        assert_eq!(
            h.read_at(0, &mut [0u8; 4]).unwrap_err(),
            StatusCode::InvalidOperation
        );
        assert_eq!(h.flags(), FLAG_DONT_MAP_LOCALLY);
    }

    #[test]
    fn write_then_read_roundtrip_and_bounds() {
        let h = MemoryHeapBase::new(1, 0).unwrap();
        let ps = page_size();
        h.write_at(10, b"hello").unwrap();
        let mut buf = [0u8; 5];
        h.read_at(10, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        h.write_at(ps - 5, b"12345").unwrap();
        assert_eq!(
            h.write_at(ps - 4, b"12345").unwrap_err(),
            StatusCode::BadValue
        );
        assert_eq!(
            h.read_at(usize::MAX, &mut buf).unwrap_err(),
            StatusCode::BadValue
        );
        assert_eq!(&h.base().unwrap()[10..15], b"hello");
    }

    #[test]
    fn owner_of_read_only_heap_can_still_write() {
        let h = MemoryHeapBase::new(4096, FLAG_READ_ONLY).unwrap();
        h.write_at(0, b"owner").unwrap();
    }

    /// Leak check: with the default macOS `ulimit -n` (256) this loop
    /// exhausts fds within the first few hundred iterations if `Drop`
    /// failed to close the backing fd or unmap the region.
    #[test]
    fn drop_releases_fd_and_mapping() {
        for _ in 0..2048 {
            let h = MemoryHeapBase::new(1, 0).unwrap();
            let _rx =
                MappedHeap::from_parcel_fd(h.to_parcel_fd().unwrap(), h.size(), 0, 0).unwrap();
        }
    }

    #[test]
    fn dup_fd_maps_the_same_pages_in_process() {
        let ps = page_size();
        let owner = MemoryHeapBase::new(ps * 2, 0).unwrap();
        let pfd = owner.to_parcel_fd().unwrap();
        let rx = MappedHeap::from_parcel_fd(pfd, owner.size(), 0, owner.flags()).unwrap();
        owner.write_at(ps, b"shared-page").unwrap();
        let mut buf = [0u8; 11];
        rx.read_at(ps, &mut buf).unwrap();
        assert_eq!(&buf, b"shared-page");
        rx.write_at(0, b"back").unwrap();
        let mut back = [0u8; 4];
        owner.read_at(0, &mut back).unwrap();
        assert_eq!(&back, b"back");
    }

    #[test]
    fn receiver_honours_read_only_flag() {
        let owner = MemoryHeapBase::new(4096, FLAG_READ_ONLY).unwrap();
        owner.write_at(0, b"ro").unwrap();
        let rx = MappedHeap::from_parcel_fd(
            owner.to_parcel_fd().unwrap(),
            owner.size(),
            0,
            FLAG_READ_ONLY,
        )
        .unwrap();
        let mut b = [0u8; 2];
        rx.read_at(0, &mut b).unwrap();
        assert_eq!(&b, b"ro");
        assert_eq!(
            rx.write_at(0, b"xx").unwrap_err(),
            StatusCode::PermissionDenied
        );
    }

    #[test]
    fn from_fd_rejects_bad_geometry() {
        let owner = MemoryHeapBase::new(1, 0).unwrap();
        let ps = page_size();
        let fd = || OwnedFd::from(owner.to_parcel_fd().unwrap());
        assert_eq!(
            MappedHeap::from_fd(fd(), 0, 0, 0).unwrap_err(),
            StatusCode::BadValue
        );
        assert_eq!(
            MappedHeap::from_fd(fd(), ps, 1, 0).unwrap_err(),
            StatusCode::BadValue
        );
        // beyond st_size
        assert_eq!(
            MappedHeap::from_fd(fd(), ps * 2, 0, 0).unwrap_err(),
            StatusCode::BadValue
        );
        // second page offset is aligned but past the end
        assert_eq!(
            MappedHeap::from_fd(fd(), ps, ps, 0).unwrap_err(),
            StatusCode::BadValue
        );
    }

    #[test]
    fn to_parcel_fd_is_cloexec_and_independent() {
        let owner = MemoryHeapBase::new(4096, 0).unwrap();
        let pfd = owner.to_parcel_fd().unwrap();
        assert_ne!(pfd.as_raw_fd(), owner.heap_id());
        let flags = rustix::io::fcntl_getfd(pfd.as_ref()).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod linux {
        use super::*;

        #[test]
        fn default_seals_are_grow_shrink_seal() {
            let h = MemoryHeapBase::new(4096, 0).unwrap();
            let s = h.seals().unwrap();
            assert_eq!(
                s & (SEAL_GROW | SEAL_SHRINK | SEAL_SEAL),
                SEAL_GROW | SEAL_SHRINK | SEAL_SEAL
            );
            assert_eq!(s & SEAL_FUTURE_WRITE, 0);
        }

        #[test]
        fn read_only_adds_future_write_and_allow_sealing_omits_seal() {
            let h = MemoryHeapBase::new(4096, FLAG_READ_ONLY | FLAG_MEMFD_ALLOW_SEALING).unwrap();
            let s = h.seals().unwrap();
            assert_ne!(s & SEAL_FUTURE_WRITE, 0);
            assert_eq!(s & SEAL_SEAL, 0);
        }

        #[test]
        fn sealed_fd_cannot_grow() {
            let h = MemoryHeapBase::new(1, 0).unwrap();
            assert!(rustix::fs::ftruncate(&h, (h.size() * 2) as u64).is_err());
        }

        #[test]
        fn strict_accepts_sealed_and_rejects_unsealed() {
            let h = MemoryHeapBase::new(4096, FLAG_READ_ONLY).unwrap();
            let fd = OwnedFd::from(h.to_parcel_fd().unwrap());
            MappedHeap::from_fd_strict(fd, h.size(), 0, FLAG_READ_ONLY).unwrap();

            // A plain memfd with no seals must be rejected.
            let ps = page_size();
            let raw =
                rustix::fs::memfd_create("unsealed", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
            rustix::fs::ftruncate(&raw, ps as u64).unwrap();
            assert_eq!(
                MappedHeap::from_fd_strict(raw, ps, 0, 0).unwrap_err(),
                StatusCode::BadValue
            );

            // Shrink-sealed but not write-sealed: ok for RW, rejected for RO.
            let rw = MemoryHeapBase::new(4096, 0).unwrap();
            let fd = || OwnedFd::from(rw.to_parcel_fd().unwrap());
            MappedHeap::from_fd_strict(fd(), ps, 0, 0).unwrap();
            assert_eq!(
                MappedHeap::from_fd_strict(fd(), ps, 0, FLAG_READ_ONLY).unwrap_err(),
                StatusCode::BadValue
            );
        }

        #[test]
        fn future_write_seal_blocks_new_writable_mapping() {
            let h = MemoryHeapBase::new(4096, FLAG_READ_ONLY).unwrap();
            let fd = OwnedFd::from(h.to_parcel_fd().unwrap());
            // A receiver that lies about the flag is stopped by the kernel.
            assert!(MappedHeap::from_fd(fd, h.size(), 0, 0).is_err());
        }
    }

    /// macOS: the protections are synthesized from darwin semantics —
    /// size is fixed after creation and a read-only heap exports an
    /// `O_RDONLY` fd the kernel refuses to map writable.
    #[cfg(target_os = "macos")]
    mod macos {
        use super::*;

        #[test]
        fn seals_are_synthesized_from_fd_mode() {
            let rw = MemoryHeapBase::new(1, 0).unwrap();
            assert_eq!(rw.seals(), Some(SEAL_SHRINK | SEAL_GROW));
            let ro = MemoryHeapBase::new(1, FLAG_READ_ONLY).unwrap();
            assert_eq!(
                ro.seals(),
                Some(SEAL_SHRINK | SEAL_GROW | SEAL_FUTURE_WRITE)
            );
            // The exported fd is the O_RDONLY handle, not the owner's.
            let pfd = ro.to_parcel_fd().unwrap();
            let fl = rustix::fs::fcntl_getfl(pfd.as_ref()).unwrap();
            assert_eq!(fl & rustix::fs::OFlags::ACCMODE, rustix::fs::OFlags::RDONLY);
        }

        #[test]
        fn regular_file_is_not_shrink_protected() {
            let f = std::fs::File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(std::env::temp_dir().join(format!("rsb-shm-probe-{}", std::process::id())))
                .unwrap();
            f.set_len(page_size() as u64).unwrap();
            let fd = OwnedFd::from(f);
            let probe = MappedHeap::from_fd(fd.try_clone().unwrap(), page_size(), 0, 0).unwrap();
            assert_eq!(probe.seals(), None);
            assert_eq!(
                MappedHeap::from_fd_strict(fd, page_size(), 0, 0).unwrap_err(),
                StatusCode::InvalidOperation
            );
        }

        #[test]
        fn second_ftruncate_is_refused_by_the_kernel() {
            let h = MemoryHeapBase::new(1, 0).unwrap();
            let fd = OwnedFd::from(h.to_parcel_fd().unwrap());
            assert!(rustix::fs::ftruncate(&fd, (h.size() / 2) as u64).is_err());
            assert!(rustix::fs::ftruncate(&fd, (h.size() * 2) as u64).is_err());
        }

        #[test]
        fn lying_peer_cannot_map_read_only_export_writable() {
            let h = MemoryHeapBase::new(1, FLAG_READ_ONLY).unwrap();
            let fd = OwnedFd::from(h.to_parcel_fd().unwrap());
            // flags=0 claims RW; the kernel refuses PROT_WRITE on an O_RDONLY fd.
            assert!(MappedHeap::from_fd(fd, h.size(), 0, 0).is_err());
            // Honest RO mapping works and reads what the owner writes.
            h.write_at(0, b"ro-ok").unwrap();
            let rx =
                MappedHeap::from_parcel_fd(h.to_parcel_fd().unwrap(), h.size(), 0, FLAG_READ_ONLY)
                    .unwrap();
            let mut b = [0u8; 5];
            rx.read_at(0, &mut b).unwrap();
            assert_eq!(&b, b"ro-ok");
            assert_eq!(
                rx.seals(),
                Some(SEAL_SHRINK | SEAL_GROW | SEAL_FUTURE_WRITE)
            );
        }

        #[test]
        fn strict_accepts_ro_export_and_rejects_rw_for_read_only_claim() {
            let ro = MemoryHeapBase::new(1, FLAG_READ_ONLY).unwrap();
            MappedHeap::from_fd_strict(
                OwnedFd::from(ro.to_parcel_fd().unwrap()),
                ro.size(),
                0,
                FLAG_READ_ONLY,
            )
            .unwrap();
            let rw = MemoryHeapBase::new(1, 0).unwrap();
            MappedHeap::from_fd_strict(OwnedFd::from(rw.to_parcel_fd().unwrap()), rw.size(), 0, 0)
                .unwrap();
            assert_eq!(
                MappedHeap::from_fd_strict(
                    OwnedFd::from(rw.to_parcel_fd().unwrap()),
                    rw.size(),
                    0,
                    FLAG_READ_ONLY
                )
                .unwrap_err(),
                StatusCode::BadValue
            );
        }

        #[test]
        fn seal_future_write_switches_export_to_read_only() {
            let h = MemoryHeapBase::new(1, 0).unwrap();
            let before = OwnedFd::from(h.to_parcel_fd().unwrap());
            h.seal_future_write().unwrap();
            h.write_at(0, b"owner").unwrap(); // owner mapping unaffected
            let after = OwnedFd::from(h.to_parcel_fd().unwrap());
            assert!(MappedHeap::from_fd(after, h.size(), 0, 0).is_err());
            // Documented gap: an fd exported earlier stays writable.
            assert!(MappedHeap::from_fd(before, h.size(), 0, 0).is_ok());
            // A received RW fd cannot be sealed; a received RO fd already is.
            let rw = MemoryHeapBase::new(1, 0).unwrap();
            let rx_rw =
                MappedHeap::from_parcel_fd(rw.to_parcel_fd().unwrap(), rw.size(), 0, 0).unwrap();
            assert_eq!(
                rx_rw.seal_future_write().unwrap_err(),
                StatusCode::InvalidOperation
            );
            let rx_ro =
                MappedHeap::from_parcel_fd(h.to_parcel_fd().unwrap(), h.size(), 0, FLAG_READ_ONLY)
                    .unwrap();
            rx_ro.seal_future_write().unwrap();
            // `AsFd` exports the same (read-only) handle as `to_parcel_fd`.
            assert!(backend::is_read_only_fd(h.as_fd()));
            assert!(!backend::is_read_only_fd(rw.as_fd()));
        }
    }
}
