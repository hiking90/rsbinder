// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Large byte payloads in a parcel, the AOSP way
//! (`Parcel::writeBlob` / `readBlob`, Java `Parcel.writeBlob`).
//!
//! A blob is a byte string that travels **inline when small and through
//! shared memory when large** — the writer decides by size, and the
//! reader is told which it got by a tag on the wire. That choice is a
//! wire convention, not an optimization each service invents: a Java peer
//! reading with `Parcel.readBlob` expects exactly these bytes. C++
//! `Parcel::readBlob(len, …)` reads them after the leading `length`,
//! which in AOSP the JNI layer reads and writes, not C++ `Parcel`.
//!
//! ```text
//! int32  length          (-1 = null, which `read_blob` reports as UnexpectedNull)
//! int32  tag             BLOB_INPLACE | BLOB_ASHMEM_IMMUTABLE | BLOB_ASHMEM_MUTABLE
//! BLOB_INPLACE:  `length` bytes, padded to 4
//! otherwise:     a file descriptor object for the shared region
//! ```
//!
//! ```no_run
//! # use rsbinder::*;
//! # fn f(payload: &[u8]) -> Result<()> {
//! let mut parcel = Parcel::new();
//! parcel.write_blob(payload, false)?;      // false = the reader may not write it
//!
//! parcel.set_data_position(0);
//! let blob = parcel.read_blob()?;
//! assert_eq!(blob.len(), payload.len());
//! # Ok(())
//! # }
//! ```
//!
//! **The fd is a memfd, and AOSP accepts it.** AOSP writes an ashmem
//! region or a memfd, as libcutils `use_memfd()` decides; its reader checks the
//! fd with `ashmem_valid()`, which answers yes for a memfd (libcutils
//! `ashmem-dev.cpp` reads `/proc/self/fd/N` and accepts a `/memfd:`
//! link) and takes the size from `fstat`. An immutable region is sealed
//! with `F_SEAL_FUTURE_WRITE` — the same seal AOSP's own memfd path
//! adds for `ashmem_set_prot_region(fd, PROT_READ)`.
//!
//! **Where fds cannot travel, a blob is still a blob.** On a transport
//! that carries no file descriptors — vsock, TLS, or the session-less
//! data-only parcel behind `to_bytes` — the payload goes
//! inline whatever its size, exactly as AOSP does when `mAllowFds` is
//! false. Nothing to configure, and the reader does not care: it follows
//! the tag.

use std::os::fd::OwnedFd;
use std::sync::{Arc, OnceLock};

use crate::error::{Result, StatusCode};
use crate::file_descriptor;
use crate::parcel::Parcel;
use crate::shared_memory::heap::{check_extent, read_memfd, seals_forbid_writing};
use crate::shared_memory::{MappedHeap, MemoryHeapBase, FLAG_READ_ONLY, SEAL_SHRINK};

/// Payloads this long or shorter always go inline; a longer one goes to
/// shared memory when [`Parcel::write_blob`] can use it (see there).
///
/// AOSP `BLOB_INPLACE_LIMIT` (`Parcel.cpp`).
pub const BLOB_INPLACE_LIMIT: usize = 16 * 1024;

/// Tag: the bytes follow in the parcel. AOSP `BLOB_INPLACE`.
pub const BLOB_INPLACE: i32 = 0;
/// Tag: a shared region the reader may only read. AOSP
/// `BLOB_ASHMEM_IMMUTABLE`. This is what Java's `Parcel.writeBlob`
/// always writes.
pub const BLOB_ASHMEM_IMMUTABLE: i32 = 1;
/// Tag: a shared region the reader may write, which the writer can then
/// see. AOSP `BLOB_ASHMEM_MUTABLE`.
pub const BLOB_ASHMEM_MUTABLE: i32 = 2;

/// Shared region name, recognizable in `/proc/<pid>/fd`; AOSP uses the same string.
const BLOB_REGION_NAME: &str = "Parcel Blob";

/// A blob read out of a parcel: the bytes, or the shared region they
/// live in.
///
/// [`to_vec`](Self::to_vec) and [`read_into`](Self::read_into) get the
/// payload whichever form it took. There is deliberately **no**
/// `as_bytes()` spanning both: a shared region is memory another process
/// can still write — always for a mutable blob, and even for an
/// immutable one, whose seal stops new writable mappings but not the
/// writer's existing one — so a `&[u8]` over it would be a promise this
/// module cannot keep. Reach for [`mapped`](Self::mapped) to work in
/// place with that in view.
///
/// A shared blob holds the region's file descriptor and maps it only
/// when it has to: at the first [`mapped`](Self::mapped), and at the
/// first copy out of a region `pread(2)` does not read (an ashmem fd,
/// macOS). A reader that copies a memfd's payload out on Linux or
/// Android pays for no mapping. [`Parcel::read_blob`] checks a
/// blob's shape, not that this process can read its region: a region
/// the kernel will not let it read or map is the `Err` of the first
/// [`to_vec`](Self::to_vec), [`read_into`](Self::read_into) or
/// [`mapped`](Self::mapped), as [`Parcel::read_blob`] states in full.
#[derive(Debug)]
pub struct Blob(Form);

#[derive(Debug)]
enum Form {
    /// Carried in the parcel itself (`BLOB_INPLACE`).
    Inline(Vec<u8>),
    /// Carried as a shared-memory region.
    Shared(Region),
}

/// A shared blob: an fd [`Parcel::read_blob`] has checked holds `len` bytes that cannot shrink.
#[derive(Debug)]
struct Region {
    fd: OwnedFd,
    len: usize,
    /// `BLOB_ASHMEM_MUTABLE`: the writer offered a region it expects to see changes in.
    mutable: bool,
    /// `len` bytes at offset 0, made by the first `mapped`.
    heap: OnceLock<Arc<MappedHeap>>,
}

impl Region {
    fn mapped(&self) -> Result<&Arc<MappedHeap>> {
        if let Some(heap) = self.heap.get() {
            return Ok(heap);
        }
        let flags = if self.mutable { 0 } else { FLAG_READ_ONLY };
        let heap = MappedHeap::from_fd(self.fd.try_clone()?, self.len, 0, flags)?;
        // Two first callers each map; one mapping is kept and the other unmapped.
        Ok(self.heap.get_or_init(|| Arc::new(heap)))
    }

    /// The first `out.len()` bytes: by `pread(2)` where the fd takes it, else the mapping.
    fn copy_out(&self, out: &mut [u8]) -> Result<()> {
        if read_memfd(&self.fd, out)? {
            return Ok(());
        }
        self.mapped()?.read_at(0, out)
    }
}

impl Blob {
    /// Payload length in bytes.
    pub fn len(&self) -> usize {
        match &self.0 {
            Form::Inline(bytes) => bytes.len(),
            Form::Shared(region) => region.len,
        }
    }

    /// Whether the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `true` when the writer sent a region both sides may write
    /// (`BLOB_ASHMEM_MUTABLE`). An inline blob is never mutable — there
    /// is nothing shared to write.
    pub fn is_mutable(&self) -> bool {
        matches!(&self.0, Form::Shared(region) if region.mutable)
    }

    /// The payload, copied out.
    ///
    /// For a shared blob this reads the region *now*; a mutable one the
    /// peer is still writing has no single value, and this returns
    /// whatever is there at the moment of the call.
    ///
    /// The `Vec` is as long as the blob says it is, and the sender chose
    /// that length: [`Parcel::read_blob`] holds it to the region's size
    /// and to `i32::MAX`, not to what this process can allocate. A
    /// reader of an untrusted sender checks [`len`](Self::len) first.
    ///
    /// # Errors
    ///
    /// An inline blob has none. For a shared blob, whatever keeps this
    /// process from reading the region, which [`Parcel::read_blob`] does
    /// not check: the error of the `pread(2)` that copies a memfd out on
    /// Linux and Android, and that of [`mapped`](Self::mapped) where the
    /// copy goes through the mapping (an ashmem fd, macOS).
    pub fn to_vec(&self) -> Result<Vec<u8>> {
        match &self.0 {
            Form::Inline(bytes) => Ok(bytes.clone()),
            Form::Shared(region) => {
                let mut out = vec![0u8; region.len];
                region.copy_out(&mut out)?;
                Ok(out)
            }
        }
    }

    /// The payload copied into `out`, which ends up holding exactly the
    /// payload: [`to_vec`](Self::to_vec) into an allocation the caller
    /// keeps.
    ///
    /// A new `Vec` of a large payload is memory the allocator takes from
    /// the kernel, a page fault for each of its pages when the copy
    /// reaches it. A reader of many large blobs — one per call of a
    /// service, say — that hands the same `Vec` back each time copies
    /// into pages it already has.
    ///
    /// # Errors
    ///
    /// Those of [`to_vec`](Self::to_vec): a shared blob whose region this
    /// process cannot read. On an error `out` holds no payload: its
    /// length and contents are unspecified.
    pub fn read_into(&self, out: &mut Vec<u8>) -> Result<()> {
        match &self.0 {
            Form::Inline(bytes) => {
                out.clear();
                out.extend_from_slice(bytes);
            }
            Form::Shared(region) => {
                // Not cleared first: only the bytes `out` grows by are zeroed.
                out.resize(region.len, 0);
                region.copy_out(out)?;
            }
        }
        Ok(())
    }

    /// The bytes without copying — `Some` only for an inline blob, where
    /// they are this process's own. See the type docs for why a shared
    /// region does not answer here.
    pub fn inline(&self) -> Option<&[u8]> {
        match &self.0 {
            Form::Inline(bytes) => Some(bytes),
            Form::Shared(_) => None,
        }
    }

    /// A shared blob's region mapped, [`len`](Self::len) bytes at offset
    /// 0, for reading it in place or (when
    /// [`is_mutable`](Self::is_mutable)) writing back to the sender.
    /// `Ok(None)` for an inline blob.
    ///
    /// The first call makes the mapping and the blob keeps it: later
    /// calls return the same one, and a clone of the `Arc` outlives the
    /// blob. An immutable blob's mapping is read-only.
    ///
    /// # Errors
    ///
    /// [`Parcel::read_blob`] checks the blob's shape and maps nothing,
    /// so whether this process gets the mapping is first known when
    /// one is made: here, or at an earlier copy that went through it.
    /// Every refusal is this call's `Err`; the blob then holds no
    /// mapping and the next call asks again. Examples, not a complete
    /// list: an immutable blob's descriptor that is not open for
    /// reading; an address space or a descriptor table that is full; a
    /// security module (SELinux) that denies the mapping, which for a
    /// mutable blob asks for write access to the region; a region this
    /// crate does not map, a memfd of `MFD_HUGETLB` pages.
    pub fn mapped(&self) -> Result<Option<&Arc<MappedHeap>>> {
        match &self.0 {
            Form::Inline(_) => Ok(None),
            Form::Shared(region) => region.mapped().map(Some),
        }
    }

    /// A shared blob's file descriptor, to hand the region on without
    /// mapping it; `None` for an inline blob. The payload is the
    /// region's first [`len`](Self::len) bytes, so take the length
    /// before this consumes the blob.
    pub fn into_fd(self) -> Option<OwnedFd> {
        match self.0 {
            Form::Inline(_) => None,
            Form::Shared(region) => Some(region.fd),
        }
    }
}

impl Parcel {
    /// Write `data` as a blob — inline when small, through a shared
    /// region when large. AOSP `Parcel::writeBlob`, with the length
    /// prefix Java's `Parcel.writeBlob` puts in front of it.
    ///
    /// `mutable_copy` picks the tag for the shared form: `false`
    /// (`BLOB_ASHMEM_IMMUTABLE`, what Java always writes) seals the
    /// region against further writes, so the reader gets a snapshot;
    /// `true` (`BLOB_ASHMEM_MUTABLE`) leaves it writable. It has no
    /// effect on a payload that goes inline, where there is nothing
    /// shared to write. The seal is asked for at region creation rather
    /// than added afterwards, because `MemoryHeapBase` seals a region
    /// `F_SEAL_SEAL` unless told otherwise and that would refuse a later
    /// `seal_future_write`.
    ///
    /// `mutable_copy` is meaningful only to the reader. This function
    /// keeps no handle on the region: the owner fd, and the mapping the
    /// region was filled through where there is one (macOS; a region of
    /// huge pages on Linux and Android), are closed before it returns,
    /// only a dup travels in the
    /// parcel, and the return type carries nothing back — so the writer
    /// cannot observe what the reader writes. A sender-side handle would
    /// take a variant returning the heap, the way AOSP's
    /// `writeBlob(len, mutableCopy, WritableBlob*)` hands one back; this
    /// API does not provide one.
    ///
    /// The shared form is used only when all of these hold; otherwise
    /// the payload goes inline and `mutable_copy` has nothing to act on:
    ///
    /// - the payload is longer than [`BLOB_INPLACE_LIMIT`];
    /// - this parcel can carry a file descriptor
    ///   ([`allow_fds`](Parcel::allow_fds)) — false on a TLS or vsock
    ///   session, which is why a blob needs no special handling there;
    /// - the target has a shared-memory backing store
    ///   ([`shared_memory::is_supported`](crate::shared_memory::is_supported));
    /// - for `mutable_copy == false`, the region came back write-sealed.
    ///   A Linux kernel older than 5.1 has no `F_SEAL_FUTURE_WRITE`, and
    ///   an immutable tag over a writable region would let one reader
    ///   change what another reads.
    ///
    /// The reader follows the tag, so it needs to know none of this.
    pub fn write_blob(&mut self, data: &[u8], mutable_copy: bool) -> Result<()> {
        // AOSP rejects a non-int32 length first; the wire carries it as one.
        let len = i32::try_from(data.len()).map_err(|_| StatusCode::BadValue)?;

        let shared = data.len() > BLOB_INPLACE_LIMIT
            && self.allow_fds()
            && crate::shared_memory::is_supported();
        // Fill the region before writing, so a failure leaves the parcel unchanged.
        let heap = if shared {
            let flags = if mutable_copy { 0 } else { FLAG_READ_ONLY };
            let heap = MemoryHeapBase::with_contents(data, flags, BLOB_REGION_NAME)?;
            // Pre-5.1 kernels lack the write seal; never tag a writable region immutable.
            let write_sealed = heap.seals().is_some_and(seals_forbid_writing);
            (mutable_copy || write_sealed).then_some(heap)
        } else {
            None
        };
        let Some(heap) = heap else {
            self.write::<i32>(&len)?;
            self.write::<i32>(&BLOB_INPLACE)?;
            // `writeInplace`: raw bytes, no own length, padded to 4 bytes.
            return self.write_aligned_data(data);
        };
        let fd = heap.to_parcel_fd()?;

        self.write::<i32>(&len)?;
        self.write::<i32>(&if mutable_copy {
            BLOB_ASHMEM_MUTABLE
        } else {
            BLOB_ASHMEM_IMMUTABLE
        })?;
        // Bare fd as AOSP `writeFileDescriptor`: no `ParcelFileDescriptor` not-null marker.
        file_descriptor::write_raw_fd(self, std::os::fd::AsFd::as_fd(fd.as_ref()))
    }

    /// Read a blob written by [`write_blob`](Parcel::write_blob) or Java
    /// `Parcel.writeBlob`.
    ///
    /// AOSP's C++ `Parcel::writeBlob(len, mutableCopy, outBlob)` is *not*
    /// a producer for this reader: it takes the length as an argument and
    /// writes the tag as the first int32, so the int32 length prefix this
    /// reader expects comes from the JNI layer above it
    /// (`android_os_Parcel_writeBlob`), which is also why the C++
    /// `readBlob(len, outBlob)` takes its length as an argument.
    ///
    /// Which form the payload took is the writer's decision, read off
    /// the tag; [`Blob::to_vec`] gets the bytes either way.
    ///
    /// A null blob — Java's `writeBlob(null)`, a `-1` length — is
    /// [`StatusCode::UnexpectedNull`] rather than an empty blob, because
    /// a zero-length blob is a different thing the same reader can meet.
    ///
    /// # What a successful read states
    ///
    /// `read_blob` checks what the parcel and the fd state about the
    /// blob's shape, and a blob that fails a check is
    /// [`StatusCode::BadValue`]:
    ///
    /// - the tag is one of the three;
    /// - the shared form's fd is one that cannot shrink under a mapping
    ///   — a memfd sealed `F_SEAL_SHRINK` or a legacy `/dev/ashmem` fd.
    ///   A regular file the sender truncates afterwards would `SIGBUS`
    ///   this process, which Rust cannot catch;
    /// - the shared form's length is not zero, and the region holds at
    ///   least that many bytes;
    /// - a `BLOB_ASHMEM_MUTABLE` tag does not come with an fd that
    ///   itself reports the kernel will refuse it a writable mapping:
    ///   one not open `O_RDWR` (on macOS, one open `O_RDONLY`), a memfd
    ///   sealed `F_SEAL_WRITE` or `F_SEAL_FUTURE_WRITE`, an ashmem
    ///   region whose `ASHMEM_GET_PROT_MASK` lacks `PROT_WRITE` — the
    ///   test
    ///   [`SharedMemory::from_fd`](crate::shared_memory::SharedMemory::from_fd)
    ///   picks its mapping by.
    ///
    /// It does **not** state that this process can read the region.
    /// `read_blob` makes no mapping and reads no byte of the region, so
    /// a region the kernel will not let this process read or map passes
    /// it, and is reported by the first of [`Blob::to_vec`],
    /// [`Blob::read_into`] and [`Blob::mapped`] to touch it: as that
    /// call's `Err`, never as a panic and never as undefined behaviour.
    /// Examples of such a region, not a complete list: an fd that is
    /// not open for reading, a mapping a security module (SELinux)
    /// denies, an address space with no room for it. The check of a
    /// mutable tag is no exception to this rule: it refuses what the fd
    /// reports without a mapping being asked for, and a mutable blob
    /// that passes it can still be refused its mapping.
    ///
    /// AOSP reports the same condition earlier: `Parcel::readBlob`
    /// (`frameworks/native/libs/binder/Parcel.cpp`) maps the region as
    /// it reads, so a region it cannot map fails that read, where this
    /// reader returns the blob and its first use fails.
    ///
    /// The no-shrink gate is **stricter than AOSP's**. `readBlob` gates on
    /// `ashmem_valid(fd)`, which accepts a memfd on the `/memfd:`
    /// `/proc/self/fd/N` link alone (libcutils `ashmem-dev.cpp`) and
    /// never asks for `F_GET_SEALS`, so an unsealed `memfd_create` fd
    /// passes there and is refused here. The difference reaches a
    /// hand-rolled sender, and an AOSP build that runs with
    /// `sys.use_memfd` set and lacks the libcutils fix "Add seal if
    /// ashmem-dev is backed by memfd" (see
    /// [`SharedMemory::from_fd`](crate::shared_memory::SharedMemory::from_fd)
    /// for the tags): its memfd carries no `F_SEAL_SHRINK`. Every other
    /// AOSP send path — ashmem, libcutils `__memfd_create_region` with
    /// that fix, AOSP's `MemoryHeapBase` and this module's — sends an
    /// fd this gate accepts.
    pub fn read_blob(&mut self) -> Result<Blob> {
        let len = self.read::<i32>()?;
        if len < 0 {
            return Err(StatusCode::UnexpectedNull);
        }
        let len = len as usize;
        let tag = self.read::<i32>()?;

        if tag == BLOB_INPLACE {
            return Ok(Blob(Form::Inline(self.read_aligned_data(len)?.to_vec())));
        }
        if tag != BLOB_ASHMEM_IMMUTABLE && tag != BLOB_ASHMEM_MUTABLE {
            log::error!("read_blob: unknown blob tag {tag}");
            return Err(StatusCode::BadValue);
        }
        let mutable = tag == BLOB_ASHMEM_MUTABLE;
        let fd = file_descriptor::read_raw_fd(self)?;
        // Stricter than AOSP `ashmem_valid`: a peer-truncatable file would SIGBUS on first touch.
        let seals = crate::shared_memory::heap::fd_seals(&fd);
        let shrink_sealed = seals.is_some_and(|s| s & SEAL_SHRINK != 0);
        if !shrink_sealed
            && !crate::shared_memory::shared::is_ashmem_fd(std::os::fd::AsFd::as_fd(&fd))
        {
            log::error!("read_blob: the blob fd is neither a sealed memfd nor ashmem");
            return Err(StatusCode::BadValue);
        }
        // AOSP `readBlob` maps here and fails on the `mmap`; this asks the fd, mapping nothing.
        if mutable && crate::shared_memory::shared::write_sealed(&fd) {
            log::error!("read_blob: a mutable blob's fd cannot be mapped writable");
            return Err(StatusCode::BadValue);
        }
        // A zero-length region cannot be mapped; writers send empty blobs inline.
        if len == 0 {
            log::error!("read_blob: a shared blob of zero length has no region to map");
            return Err(StatusCode::BadValue);
        }
        // Region >= payload (as AOSP), and no 0-length fd but ashmem: what a mapping would need.
        check_extent(&fd, len, 0)?;
        Ok(Blob(Form::Shared(Region {
            fd,
            len,
            mutable,
            heap: OnceLock::new(),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Size aside, can `write_blob` go shared here: a backing store, and write seals if immutable.
    /// `fd` opened again with its own access mode; `None` where SELinux gives no `open` on a memfd.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reopened(fd: std::os::fd::BorrowedFd<'_>, write: bool) -> Option<OwnedFd> {
        use std::os::fd::AsRawFd;
        let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        match std::fs::OpenOptions::new()
            .read(!write)
            .write(write)
            .open(path)
        {
            Ok(file) => Some(OwnedFd::from(file)),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => None,
            Err(e) => panic!("reopen: {e}"),
        }
    }

    fn shared_form_available(mutable_copy: bool) -> bool {
        if !crate::shared_memory::is_supported() {
            return false;
        }
        mutable_copy
            || MemoryHeapBase::new_named(4096, FLAG_READ_ONLY, BLOB_REGION_NAME)
                .ok()
                .and_then(|h| h.seals())
                .is_some_and(seals_forbid_writing)
    }

    /// Plan 10-2 AC-2.1: the boundary is the whole decision; pin one byte either side of it.
    #[test]
    fn the_inplace_limit_decides_the_form() {
        for (len, want_inline) in [
            (0usize, true),
            (1, true),
            (BLOB_INPLACE_LIMIT - 1, true),
            (BLOB_INPLACE_LIMIT, true),
            (BLOB_INPLACE_LIMIT + 1, false),
        ] {
            let data = pattern(len);
            let mut parcel = Parcel::new();
            // A fresh parcel is kernel-marshalled: fds allowed, only the size decides.
            assert!(parcel.allow_fds());
            parcel.write_blob(&data, false).expect("write_blob");
            parcel.set_data_position(0);

            let blob = parcel.read_blob().expect("read_blob");
            assert_eq!(blob.len(), len, "len {len}");
            let want_inline = want_inline || !shared_form_available(false);
            assert_eq!(
                blob.inline().is_some(),
                want_inline,
                "len {len} took the wrong form"
            );
            assert_eq!(blob.to_vec().expect("to_vec"), data, "len {len} round trip");
            assert!(!blob.is_mutable());
        }
    }

    /// Lengths that end inside a page and span many: the shared form carries each exactly.
    #[test]
    fn a_shared_blob_round_trips_at_any_length() {
        for len in [
            BLOB_INPLACE_LIMIT + 1,
            BLOB_INPLACE_LIMIT + 4095,
            1 << 20,
            (3 << 20) + 5,
        ] {
            let data = pattern(len);
            for mutable_copy in [false, true] {
                let mut parcel = Parcel::new();
                parcel.write_blob(&data, mutable_copy).expect("write_blob");
                parcel.set_data_position(0);
                let blob = parcel.read_blob().expect("read_blob");
                assert_eq!(blob.len(), len);
                assert_eq!(blob.to_vec().expect("to_vec"), data, "len {len}");
                // `read_into` leaves exactly the payload, whatever `out` held: longer, shorter.
                for held in [len + 9, len / 2, 0] {
                    let mut out = vec![0xa5u8; held];
                    blob.read_into(&mut out).expect("read_into");
                    assert_eq!(out, data, "len {len} into a Vec of {held}");
                }
                // The mapping a reader may take instead reads the same bytes.
                if let Some(heap) = blob.mapped().expect("mapped") {
                    let mut mapped = vec![0u8; len];
                    heap.read_at(0, &mut mapped).expect("read_at");
                    assert_eq!(mapped, data, "len {len} through the mapping");
                }
            }
        }
    }

    /// The blob maps once and keeps the mapping; its fd carries the region on without one.
    #[test]
    fn a_shared_blob_maps_once_and_gives_up_its_fd() {
        let data = pattern(BLOB_INPLACE_LIMIT + 4096);
        if !shared_form_available(false) {
            return;
        }
        let mut parcel = Parcel::new();
        parcel.write_blob(&data, false).expect("write_blob");
        parcel.set_data_position(0);
        let blob = parcel.read_blob().expect("read_blob");

        let first = blob.mapped().expect("mapped").expect("shared").clone();
        let again = blob.mapped().expect("mapped").expect("shared");
        assert!(Arc::ptr_eq(&first, again));
        assert_eq!(crate::shared_memory::IMemoryHeap::size(&*first), data.len());

        // The mapping outlives the blob, and the fd holds the payload for whoever gets it.
        let fd = blob.into_fd().expect("a shared blob has an fd");
        let mut kept = vec![0u8; data.len()];
        first.read_at(0, &mut kept).expect("read_at");
        assert_eq!(kept, data);
        let passed_on = MappedHeap::from_fd_strict(fd, data.len(), 0, FLAG_READ_ONLY)
            .expect("the fd is a write-sealed region of the payload's length");
        let mut got = vec![0u8; data.len()];
        passed_on.read_at(0, &mut got).expect("read_at");
        assert_eq!(got, data);

        let mut parcel = Parcel::new();
        parcel.write_blob(b"small", false).expect("write_blob");
        parcel.set_data_position(0);
        let blob = parcel.read_blob().expect("read_blob");
        assert!(blob.mapped().expect("mapped").is_none());
        assert!(blob.into_fd().is_none());
    }

    /// The tag says which form the reader gets; only a mutable region is reader-writable.
    #[test]
    fn a_mutable_blob_is_writable_and_an_immutable_one_is_not() {
        let data = pattern(BLOB_INPLACE_LIMIT + 4096);
        if !shared_form_available(true) {
            return;
        }

        let mut parcel = Parcel::new();
        parcel.write_blob(&data, true).expect("write_blob mutable");
        parcel.set_data_position(0);
        let blob = parcel.read_blob().expect("read_blob");
        assert!(blob.is_mutable());
        let heap = blob
            .mapped()
            .expect("mapped")
            .expect("a large blob is shared")
            .clone();
        heap.write_at(0, b"edit")
            .expect("a mutable region takes a write");
        // The edit is visible to a read of the same region.
        assert_eq!(&blob.to_vec().expect("to_vec")[..4], b"edit");

        if !shared_form_available(false) {
            return;
        }
        let mut parcel = Parcel::new();
        parcel
            .write_blob(&data, false)
            .expect("write_blob immutable");
        parcel.set_data_position(0);
        let blob = parcel.read_blob().expect("read_blob");
        assert!(!blob.is_mutable());
        // Plan 10-2 AC-2.3: the region is mapped read-only.
        let heap = blob
            .mapped()
            .expect("mapped")
            .expect("a large blob is shared");
        assert!(
            heap.write_at(0, b"edit").is_err(),
            "an immutable blob must not accept a write"
        );
        // The flag is rsbinder bookkeeping; the seal stops a reader that maps the fd itself.
        assert!(
            seals_forbid_writing(heap.seals().expect("seals")),
            "an immutable blob's fd must be write-sealed"
        );
    }

    /// A `-1` length is Java's null blob, which is not an empty blob.
    #[test]
    fn a_null_blob_is_distinguishable_from_an_empty_one() {
        let mut parcel = Parcel::new();
        parcel.write::<i32>(&-1).expect("write null marker");
        parcel.set_data_position(0);
        assert_eq!(parcel.read_blob().err(), Some(StatusCode::UnexpectedNull));

        let mut parcel = Parcel::new();
        parcel.write_blob(&[], false).expect("write_blob empty");
        parcel.set_data_position(0);
        let blob = parcel.read_blob().expect("read_blob");
        assert!(blob.is_empty());
        assert_eq!(blob.to_vec().expect("to_vec"), Vec::<u8>::new());
        let mut out = vec![1u8; 8];
        blob.read_into(&mut out).expect("read_into");
        assert!(out.is_empty());

        // An inline blob replaces what `out` held as a shared one does.
        let mut parcel = Parcel::new();
        parcel.write_blob(b"inline", false).expect("write_blob");
        parcel.set_data_position(0);
        let blob = parcel.read_blob().expect("read_blob");
        blob.read_into(&mut out).expect("read_into");
        assert_eq!(out, b"inline");
    }

    /// Plan 10-2 AC-2.5: an fd-less parcel (AOSP `!mAllowFds`, `to_bytes`) inlines any size.
    #[test]
    fn a_parcel_that_cannot_carry_an_fd_writes_the_payload_inline() {
        let data = pattern(1024 * 1024);
        let mut parcel = Parcel::new_data_only();
        assert!(!parcel.allow_fds());

        parcel.write_blob(&data, true).expect("write_blob");
        let bytes = parcel.into_bytes().expect("into_bytes");
        // 1 MB inline plus length and tag: no fd, so the bytes carry the whole payload.
        assert!(bytes.len() >= data.len() + 8);

        let mut parcel = Parcel::data_only_from_vec(bytes);
        let blob = parcel.read_blob().expect("read_blob");
        assert_eq!(blob.inline().map(<[u8]>::len), Some(data.len()));
        assert_eq!(blob.to_vec().expect("to_vec"), data);
        // `mutable_copy` could not get a shared region; an inline blob is never mutable.
        assert!(!blob.is_mutable());
    }

    /// A mutable tag over a write-sealed region is refused at the read, not at the first mapping.
    #[test]
    fn a_mutable_tag_over_a_write_sealed_region_is_refused() {
        if !shared_form_available(false) {
            return;
        }
        let len = BLOB_INPLACE_LIMIT + 4096;
        for (tag, refused) in [(BLOB_ASHMEM_MUTABLE, true), (BLOB_ASHMEM_IMMUTABLE, false)] {
            let heap = MemoryHeapBase::new_named(len, FLAG_READ_ONLY, BLOB_REGION_NAME)
                .expect("a write-sealed region");
            let fd = heap.to_parcel_fd().expect("to_parcel_fd");
            let mut parcel = Parcel::new();
            parcel.write::<i32>(&(len as i32)).expect("len");
            parcel.write::<i32>(&tag).expect("tag");
            file_descriptor::write_raw_fd(&mut parcel, std::os::fd::AsFd::as_fd(fd.as_ref()))
                .expect("fd");
            parcel.set_data_position(0);
            let status = parcel.read_blob().err();
            assert_eq!(status, refused.then_some(StatusCode::BadValue), "{tag}");
        }
    }

    /// A mutable tag over an unsealed memfd reopened `O_RDONLY` is refused at the read as well.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_mutable_tag_over_a_read_only_fd_is_refused() {
        use std::os::fd::AsFd;
        let len = BLOB_INPLACE_LIMIT + 4096;
        for (tag, refused) in [(BLOB_ASHMEM_MUTABLE, true), (BLOB_ASHMEM_IMMUTABLE, false)] {
            let heap = MemoryHeapBase::new_named(len, 0, BLOB_REGION_NAME).expect("a region");
            assert!(!seals_forbid_writing(heap.seals().expect("seals")));
            let owner = heap.to_parcel_fd().expect("to_parcel_fd");
            let Some(fd) = reopened(owner.as_ref().as_fd(), false) else {
                return;
            };
            let mut parcel = Parcel::new();
            parcel.write::<i32>(&(len as i32)).expect("len");
            parcel.write::<i32>(&tag).expect("tag");
            file_descriptor::write_raw_fd(&mut parcel, fd.as_fd()).expect("fd");
            parcel.set_data_position(0);
            let status = parcel.read_blob().map(|blob| blob.mapped().map(|_| ()));
            let want = if refused {
                Err(StatusCode::BadValue)
            } else {
                Ok(Ok(()))
            };
            assert_eq!(status, want, "{tag}");
        }
    }

    /// `read_blob` states a blob's shape, not that its region can be read: the first use says so.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_region_that_cannot_be_read_is_the_error_of_each_use() {
        use std::os::fd::AsFd;
        let len = BLOB_INPLACE_LIMIT + 4096;
        let heap = MemoryHeapBase::new_named(len, 0, BLOB_REGION_NAME).expect("a region");
        let owner = heap.to_parcel_fd().expect("to_parcel_fd");
        let Some(fd) = reopened(owner.as_ref().as_fd(), true) else {
            return;
        };
        let mut parcel = Parcel::new();
        parcel.write::<i32>(&(len as i32)).expect("len");
        parcel.write::<i32>(&BLOB_ASHMEM_IMMUTABLE).expect("tag");
        file_descriptor::write_raw_fd(&mut parcel, fd.as_fd()).expect("fd");
        parcel.set_data_position(0);

        // A shrink-sealed memfd of `len` bytes has a blob's shape, whatever its open mode.
        let blob = parcel.read_blob().expect("read_blob checks the shape");
        assert_eq!(blob.len(), len);
        assert!(blob.to_vec().is_err(), "to_vec");
        let mut out = vec![0xa5u8; 16];
        assert!(blob.read_into(&mut out).is_err(), "read_into");
        // A refused mapping is not kept: the second call asks the kernel again.
        assert!(blob.mapped().is_err(), "mapped");
        assert!(blob.mapped().is_err(), "mapped again");
    }

    /// An unknown tag is refused rather than guessed at.
    #[test]
    fn an_unknown_tag_is_refused() {
        let mut parcel = Parcel::new();
        parcel.write::<i32>(&8).expect("len");
        parcel.write::<i32>(&7).expect("bogus tag");
        parcel.set_data_position(0);
        assert_eq!(parcel.read_blob().err(), Some(StatusCode::BadValue));
    }
}
