// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `android.os.SharedMemory` / NDK `ASharedMemory` equivalent (Plan
//! 4-7a Phase C).
//!
//! On the wire a `SharedMemory` is **one file descriptor** and nothing
//! else (`SharedMemory.java` `writeToParcel` → `writeFileDescriptor`);
//! the receiver recovers the size from the fd itself. So an AIDL
//! `ParcelFileDescriptor` argument/return carrying a memfd or ashmem fd
//! is byte-identical to a Java/NDK `SharedMemory`, and this type is the
//! convenience wrapper on both ends.
//!
//! Size recovery follows libcutils `ashmem_get_size_region`: `fstat`
//! for a memfd / POSIX shm object, and — on Android — the
//! `ASHMEM_GET_SIZE` ioctl for a legacy `/dev/ashmem` fd, which reports
//! `st_size == 0`.

use std::os::fd::{AsFd, OwnedFd};

use super::heap::{MappedHeap, MemoryHeapBase};
use super::{IMemoryHeap, FLAG_MEMFD_ALLOW_SEALING, FLAG_READ_ONLY};
use crate::error::{Result, StatusCode};
use crate::file_descriptor::ParcelFileDescriptor;
use crate::parcel::Parcel;
use crate::parcelable::{Deserialize, DeserializeOption, Serialize, SerializeOption};

/// One anonymous shared region identified by its fd alone.
///
/// Created locally with [`create`](Self::create) (owner, always
/// writable) or adopted from a received fd with
/// [`from_fd`](Self::from_fd) / parcel deserialization. Serializes as a
/// bare `ParcelFileDescriptor`.
#[derive(Debug)]
pub struct SharedMemory(Inner);

#[derive(Debug)]
enum Inner {
    Owner(MemoryHeapBase),
    Mapped(MappedHeap),
}

impl SharedMemory {
    /// Allocate `size` bytes (page-rounded), mapped read/write.
    /// Equivalent to `ASharedMemory_create(name, size)`: the region is
    /// grow/shrink-sealed but stays sealable so
    /// [`seal_read_only`](Self::seal_read_only) works later.
    pub fn create(size: usize) -> Result<Self> {
        Self::create_named(size, "SharedMemory")
    }

    /// [`create`](Self::create) with a debug name.
    pub fn create_named(size: usize, name: &str) -> Result<Self> {
        Ok(Self(Inner::Owner(MemoryHeapBase::new_named(
            size,
            FLAG_MEMFD_ALLOW_SEALING,
            name,
        )?)))
    }

    /// Adopt a received shared-memory fd, recovering its size from the
    /// fd (see module doc) and mapping it read/write — or read-only
    /// when the fd is write-protected (`F_SEAL_WRITE` /
    /// `F_SEAL_FUTURE_WRITE` on Linux/Android — how `ASharedMemory_setProt`
    /// enforces read-only on memfd — or an `O_RDONLY` fd on macOS).
    pub fn from_fd(fd: OwnedFd) -> Result<Self> {
        let size = region_size(&fd)?;
        let flags = if write_sealed(&fd) { FLAG_READ_ONLY } else { 0 };
        Ok(Self(Inner::Mapped(MappedHeap::from_fd(
            fd, size, 0, flags,
        )?)))
    }

    /// Byte length of the region.
    pub fn size(&self) -> usize {
        match &self.0 {
            Inner::Owner(h) => h.size(),
            Inner::Mapped(h) => h.size(),
        }
    }

    /// Whether this mapping is read-only.
    pub fn is_read_only(&self) -> bool {
        match &self.0 {
            Inner::Owner(_) => false,
            Inner::Mapped(h) => h.flags() & FLAG_READ_ONLY != 0,
        }
    }

    /// Copy out of the region. See [`MemoryHeapBase::read_at`].
    pub fn read_at(&self, off: usize, dst: &mut [u8]) -> Result<()> {
        match &self.0 {
            Inner::Owner(h) => h.read_at(off, dst),
            Inner::Mapped(h) => h.read_at(off, dst),
        }
    }

    /// Copy into the region. See [`MemoryHeapBase::write_at`].
    pub fn write_at(&self, off: usize, src: &[u8]) -> Result<()> {
        match &self.0 {
            Inner::Owner(h) => h.write_at(off, src),
            Inner::Mapped(h) => h.write_at(off, src),
        }
    }

    /// Raw base pointer of the local mapping. See
    /// [`MemoryHeapBase::as_ptr`].
    pub fn as_ptr(&self) -> Option<*mut u8> {
        match &self.0 {
            Inner::Owner(h) => h.as_ptr(),
            Inner::Mapped(h) => h.as_ptr(),
        }
    }

    /// Seal the region against **future** writable mappings
    /// (`ASharedMemory_setProt(fd, PROT_READ)` on memfd). Existing
    /// mappings — including this process's — stay writable. See
    /// [`MemoryHeapBase::seal_future_write`] for the macOS caveat.
    pub fn seal_read_only(&self) -> Result<()> {
        match &self.0 {
            Inner::Owner(h) => h.seal_future_write(),
            Inner::Mapped(h) => h.seal_future_write(),
        }
    }

    /// `dup` the fd into a parcelable handle (the wire form).
    pub fn to_parcel_fd(&self) -> Result<ParcelFileDescriptor> {
        match &self.0 {
            Inner::Owner(h) => h.to_parcel_fd(),
            Inner::Mapped(h) => h.to_parcel_fd(),
        }
    }

    /// Borrow the backing heap through the trait surface.
    pub fn as_heap(&self) -> &dyn IMemoryHeap {
        match &self.0 {
            Inner::Owner(h) => h,
            Inner::Mapped(h) => h,
        }
    }
}

impl AsFd for SharedMemory {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        match &self.0 {
            Inner::Owner(h) => h.as_fd(),
            Inner::Mapped(h) => h.as_fd(),
        }
    }
}

impl Serialize for SharedMemory {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        self.to_parcel_fd()?.serialize(parcel)
    }
}

impl SerializeOption for SharedMemory {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        match this {
            Some(s) => s.serialize(parcel),
            None => ParcelFileDescriptor::serialize_option(None, parcel),
        }
    }
}

impl Deserialize for SharedMemory {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let pfd: ParcelFileDescriptor = parcel.read()?;
        Self::from_fd(OwnedFd::from(pfd))
    }
}

impl DeserializeOption for SharedMemory {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        match ParcelFileDescriptor::deserialize_option(parcel)? {
            Some(pfd) => Ok(Some(Self::from_fd(OwnedFd::from(pfd))?)),
            None => Ok(None),
        }
    }
}

/// Size of a shared-memory region from its fd alone (libcutils
/// `ashmem_get_size_region`).
pub fn region_size<F: AsFd>(fd: F) -> Result<usize> {
    let st = rustix::fs::fstat(fd.as_fd())?;
    if st.st_size > 0 {
        return usize::try_from(st.st_size).map_err(|_| StatusCode::BadValue);
    }
    ashmem_size(fd.as_fd())
}

#[cfg(target_os = "android")]
fn ashmem_size(fd: std::os::fd::BorrowedFd<'_>) -> Result<usize> {
    use std::os::fd::AsRawFd;
    // `ASHMEM_GET_SIZE` = `_IO(0x77, 4)`; the size is the ioctl's return
    // value. Only a legacy `/dev/ashmem` fd reports `st_size == 0`.
    const ASHMEM_GET_SIZE: libc::c_ulong = 0x7704;
    // SAFETY: plain ioctl on a borrowed, open fd with no pointer argument.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), ASHMEM_GET_SIZE as _) };
    if r < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    usize::try_from(r).map_err(|_| StatusCode::BadValue)
}

#[cfg(not(target_os = "android"))]
fn ashmem_size(_fd: std::os::fd::BorrowedFd<'_>) -> Result<usize> {
    Err(StatusCode::BadValue)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn write_sealed<F: AsFd>(fd: F) -> bool {
    use rustix::fs::SealFlags;
    rustix::fs::fcntl_get_seals(fd)
        .map(|s| s.intersects(SealFlags::WRITE | SealFlags::FUTURE_WRITE))
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn write_sealed<F: AsFd>(fd: F) -> bool {
    super::heap::is_read_only_fd(fd)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn write_sealed<F: AsFd>(_fd: F) -> bool {
    false
}

#[cfg(all(
    test,
    any(target_os = "linux", target_os = "android", target_os = "macos")
))]
mod tests {
    use super::*;

    fn page() -> usize {
        rustix::param::page_size()
    }

    #[test]
    fn create_then_adopt_by_fd_recovers_size_and_shares_pages() {
        let owner = SharedMemory::create(page() + 1).unwrap();
        assert_eq!(owner.size(), page() * 2);
        assert!(!owner.is_read_only());
        let rx = SharedMemory::from_fd(OwnedFd::from(owner.to_parcel_fd().unwrap())).unwrap();
        assert_eq!(rx.size(), page() * 2);
        assert!(!rx.is_read_only());
        owner.write_at(page(), b"shm").unwrap();
        let mut b = [0u8; 3];
        rx.read_at(page(), &mut b).unwrap();
        assert_eq!(&b, b"shm");
        assert_eq!(rx.as_heap().size(), page() * 2);
    }

    #[test]
    fn region_size_rejects_non_region_fd() {
        // A socket reports st_size 0 and is not ashmem.
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(region_size(&a).is_err());
    }

    /// In-process parcel round trip (kernel-mode parcel, no driver
    /// needed for fd objects): wire form == bare `ParcelFileDescriptor`.
    #[test]
    fn parcel_roundtrip_is_a_bare_fd() {
        let owner = SharedMemory::create(page()).unwrap();
        owner.write_at(0, b"parcel").unwrap();
        let mut p = Parcel::new();
        p.write(&owner).unwrap();
        p.write(&Option::<SharedMemory>::None).unwrap();
        p.set_data_position(0);
        let pfd: ParcelFileDescriptor = p.read().unwrap();
        let rx = SharedMemory::from_fd(OwnedFd::from(pfd)).unwrap();
        let mut b = [0u8; 6];
        rx.read_at(0, &mut b).unwrap();
        assert_eq!(&b, b"parcel");
        let none: Option<SharedMemory> = p.read().unwrap();
        assert!(none.is_none());

        let mut p = Parcel::new();
        p.write(&owner).unwrap();
        p.set_data_position(0);
        let rx2: SharedMemory = p.read().unwrap();
        assert_eq!(rx2.size(), page());
    }

    #[test]
    fn sealed_read_only_is_detected_by_receiver() {
        let owner = SharedMemory::create(page()).unwrap();
        owner.seal_read_only().unwrap();
        owner.write_at(0, b"ok").unwrap(); // existing mapping stays writable
        let rx = SharedMemory::from_fd(OwnedFd::from(owner.to_parcel_fd().unwrap())).unwrap();
        assert!(rx.is_read_only());
        assert_eq!(
            rx.write_at(0, b"xx").unwrap_err(),
            StatusCode::PermissionDenied
        );
        let mut b = [0u8; 2];
        rx.read_at(0, &mut b).unwrap();
        assert_eq!(&b, b"ok");
    }

    #[test]
    fn received_region_cannot_be_sealed_on_macos_but_can_on_linux() {
        let owner = SharedMemory::create(page()).unwrap();
        let rx = SharedMemory::from_fd(OwnedFd::from(owner.to_parcel_fd().unwrap())).unwrap();
        let r = rx.seal_read_only();
        if cfg!(target_os = "macos") {
            assert_eq!(r.unwrap_err(), StatusCode::InvalidOperation);
        } else {
            r.unwrap();
        }
    }
}
