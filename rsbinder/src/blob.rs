// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Large byte payloads in a parcel, the AOSP way
//! (`Parcel::writeBlob` / `readBlob`, Java `Parcel.writeBlob`).
//!
//! A blob is a byte string that travels **inline when small and through
//! shared memory when large** — the writer decides by size, and the
//! reader is told which it got by a tag on the wire. That choice is a
//! wire convention, not an optimization each service invents: a Java or
//! C++ peer reading with `readBlob` expects exactly these bytes.
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

use std::sync::Arc;

use crate::error::{Result, StatusCode};
use crate::file_descriptor;
use crate::parcel::Parcel;
use crate::shared_memory::{
    IMemoryHeap, MappedHeap, MemoryHeapBase, FLAG_READ_ONLY, SEAL_FUTURE_WRITE, SEAL_SHRINK,
    SEAL_WRITE,
};

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

/// The name the shared region is created under, so it is recognizable in
/// `/proc/<pid>/fd`. AOSP uses the same string.
const BLOB_REGION_NAME: &str = "Parcel Blob";

/// A blob read out of a parcel: the bytes, or the mapping they live in.
///
/// [`to_vec`](Self::to_vec) gets the payload whichever form it took.
/// There is deliberately **no** `as_bytes()` spanning both: a shared
/// region is memory another process can still write — always for a
/// mutable blob, and even for an immutable one, whose seal stops new
/// writable mappings but not the writer's existing one — so a `&[u8]`
/// over it would be a promise this module cannot keep. Reach for
/// [`mapped`](Self::mapped) to work in place with that in view.
#[derive(Debug)]
pub enum Blob {
    /// Carried in the parcel itself (`BLOB_INPLACE`).
    Inline(Vec<u8>),
    /// Carried as a shared-memory region.
    Shared {
        /// The mapping, `len` bytes at offset 0.
        heap: Arc<MappedHeap>,
        /// Payload length. For a blob [`Parcel::read_blob`] produced it
        /// equals `heap.size()`, which records the length that was asked
        /// for; the page rounding happens inside the kernel mapping and
        /// is not visible here.
        len: usize,
        /// `true` for `BLOB_ASHMEM_MUTABLE` — the writer offered a
        /// region it expects to see changes in.
        mutable: bool,
    },
}

impl Blob {
    /// Payload length in bytes.
    pub fn len(&self) -> usize {
        match self {
            Blob::Inline(bytes) => bytes.len(),
            Blob::Shared { len, .. } => *len,
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
        matches!(self, Blob::Shared { mutable: true, .. })
    }

    /// The payload, copied out.
    ///
    /// For a shared blob this reads the region *now*; a mutable one the
    /// peer is still writing has no single value, and this returns
    /// whatever is there at the moment of the call.
    pub fn to_vec(&self) -> Result<Vec<u8>> {
        match self {
            Blob::Inline(bytes) => Ok(bytes.clone()),
            Blob::Shared { heap, len, .. } => {
                // Only a hand-assembled `Blob::Shared` can get here with
                // `len` past the mapping; make it a `BadValue` rather than
                // an OOM abort on the allocation below.
                if *len > heap.size() {
                    return Err(StatusCode::BadValue);
                }
                let mut out = vec![0u8; *len];
                heap.read_at(0, &mut out)?;
                Ok(out)
            }
        }
    }

    /// The bytes without copying — `Some` only for an inline blob, where
    /// they are this process's own. See the type docs for why a shared
    /// region does not answer here.
    pub fn inline(&self) -> Option<&[u8]> {
        match self {
            Blob::Inline(bytes) => Some(bytes),
            Blob::Shared { .. } => None,
        }
    }

    /// The mapping a shared blob arrived in, for reading it in place or
    /// (when [`is_mutable`](Self::is_mutable)) writing back to the
    /// sender. `None` for an inline blob.
    pub fn mapped(&self) -> Option<&Arc<MappedHeap>> {
        match self {
            Blob::Inline(_) => None,
            Blob::Shared { heap, .. } => Some(heap),
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
    /// keeps no handle on the region: the mapping is dropped and the
    /// owner fd closed before it returns, only a dup travels in the
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
        // AOSP rejects a length that does not fit an int32 before
        // anything else; the wire carries it as one.
        let len = i32::try_from(data.len()).map_err(|_| StatusCode::BadValue)?;

        let shared = data.len() > BLOB_INPLACE_LIMIT
            && self.allow_fds()
            && crate::shared_memory::is_supported();
        // Build and fill the region before writing anything, so a
        // failure here leaves the parcel as it was.
        let heap = if shared {
            let flags = if mutable_copy { 0 } else { FLAG_READ_ONLY };
            let heap = MemoryHeapBase::new_named(data.len(), flags, BLOB_REGION_NAME)?;
            // A pre-5.1 kernel creates the region without the write seal;
            // the immutable tag must not go out over a writable region.
            let write_sealed = heap
                .seals()
                .is_some_and(|s| s & (SEAL_WRITE | SEAL_FUTURE_WRITE) != 0);
            (mutable_copy || write_sealed).then_some(heap)
        } else {
            None
        };
        let Some(heap) = heap else {
            self.write::<i32>(&len)?;
            self.write::<i32>(&BLOB_INPLACE)?;
            // `writeInplace`: the bytes with no length of their own,
            // padded to the parcel's 4-byte grain.
            return self.write_aligned_data(data);
        };
        heap.write_at(0, data)?;
        let fd = heap.to_parcel_fd()?;

        self.write::<i32>(&len)?;
        self.write::<i32>(&if mutable_copy {
            BLOB_ASHMEM_MUTABLE
        } else {
            BLOB_ASHMEM_IMMUTABLE
        })?;
        // The bare fd object AOSP's `writeFileDescriptor` writes — no
        // AIDL not-null marker, which `ParcelFileDescriptor` would add.
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
    /// The shared form's fd must be one that cannot shrink under the
    /// mapping — a memfd sealed `F_SEAL_SHRINK` or a legacy
    /// `/dev/ashmem` fd. Anything else is [`StatusCode::BadValue`]: a
    /// regular file the sender truncates afterwards would `SIGBUS` this
    /// process, which Rust cannot catch.
    ///
    /// That gate is **stricter than AOSP's**. `readBlob` gates on
    /// `ashmem_valid(fd)`, which accepts a memfd on the `/memfd:`
    /// `/proc/self/fd/N` link alone (libcutils `ashmem-dev.cpp`) and
    /// never asks for `F_GET_SEALS`, so an unsealed `memfd_create` fd
    /// passes there and is refused here. The difference reaches only a
    /// hand-rolled sender: every AOSP send path — libcutils
    /// `__memfd_create_region`, AOSP's `MemoryHeapBase` and this
    /// module's — adds `F_SEAL_GROW | F_SEAL_SHRINK` at creation.
    pub fn read_blob(&mut self) -> Result<Blob> {
        let len = self.read::<i32>()?;
        if len < 0 {
            return Err(StatusCode::UnexpectedNull);
        }
        let len = len as usize;
        let tag = self.read::<i32>()?;

        if tag == BLOB_INPLACE {
            return Ok(Blob::Inline(self.read_aligned_data(len)?.to_vec()));
        }
        if tag != BLOB_ASHMEM_IMMUTABLE && tag != BLOB_ASHMEM_MUTABLE {
            log::error!("read_blob: unknown blob tag {tag}");
            return Err(StatusCode::BadValue);
        }
        let mutable = tag == BLOB_ASHMEM_MUTABLE;
        let fd = file_descriptor::read_raw_fd(self)?;
        // Stricter than AOSP's `ashmem_valid(fd)`, which does not read
        // seals: a regular file the peer can `ftruncate` would `SIGBUS`
        // this process on first touch.
        let shrink_sealed =
            crate::shared_memory::heap::fd_seals(&fd).is_some_and(|s| s & SEAL_SHRINK != 0);
        if !shrink_sealed
            && !crate::shared_memory::shared::is_ashmem_fd(std::os::fd::AsFd::as_fd(&fd))
        {
            log::error!("read_blob: the blob fd is neither a sealed memfd nor ashmem");
            return Err(StatusCode::BadValue);
        }
        // A zero-length shared blob cannot be mapped (and no writer
        // produces one: that side goes inline).
        if len == 0 {
            log::error!("read_blob: a shared blob of zero length has no region to map");
            return Err(StatusCode::BadValue);
        }
        // `from_fd` makes AOSP's own check — the region must be at least
        // as long as the payload — and refuses a zero-length fd that is
        // not ashmem, which would `SIGBUS` this process on first touch.
        let flags = if mutable { 0 } else { FLAG_READ_ONLY };
        let heap = MappedHeap::from_fd(fd, len, 0, flags)?;
        Ok(Blob::Shared {
            heap: Arc::new(heap),
            len,
            mutable,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Whether `write_blob` on a fresh kernel parcel can take the shared
    /// form here, by its own conditions other than the size: a backing
    /// store, and for an immutable blob a kernel that write-seals.
    fn shared_form_available(mutable_copy: bool) -> bool {
        if !crate::shared_memory::is_supported() {
            return false;
        }
        mutable_copy
            || MemoryHeapBase::new_named(4096, FLAG_READ_ONLY, BLOB_REGION_NAME)
                .ok()
                .and_then(|h| h.seals())
                .is_some_and(|s| s & (SEAL_WRITE | SEAL_FUTURE_WRITE) != 0)
    }

    /// Plan 10-2 AC-2.1: the boundary is the whole decision, so it is
    /// the thing to pin — one byte either side of it.
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
            // A fresh parcel is kernel-marshalled, so fds are allowed
            // and only the size decides.
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

    /// The tag says which form the reader gets, and a mutable region is
    /// the one the reader may write.
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
        let heap = blob.mapped().expect("a large blob is shared").clone();
        heap.write_at(0, b"edit")
            .expect("a mutable region takes a write");
        // The edit is visible through the same mapping the blob reads.
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
        // Plan 10-2 AC-2.3: the region is mapped read-only, so the
        // reader cannot write it even by mistake.
        let heap = blob.mapped().expect("a large blob is shared");
        assert!(
            heap.write_at(0, b"edit").is_err(),
            "an immutable blob must not accept a write"
        );
        // That flag is rsbinder's own bookkeeping; the seal is what stops
        // a reader that maps the received fd itself.
        assert_ne!(
            heap.seals().expect("seals") & (SEAL_WRITE | SEAL_FUTURE_WRITE),
            0,
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
    }

    /// Plan 10-2 AC-2.5: a parcel that cannot carry an fd writes the
    /// payload inline whatever its size — AOSP's `!mAllowFds` branch —
    /// and the data-only parcel behind `to_bytes` is exactly such a
    /// parcel, so a blob survives a round trip through plain bytes.
    #[cfg(feature = "rpc")]
    #[test]
    fn a_parcel_that_cannot_carry_an_fd_writes_the_payload_inline() {
        let data = pattern(1024 * 1024);
        let mut parcel = Parcel::new_data_only();
        assert!(!parcel.allow_fds());

        parcel.write_blob(&data, true).expect("write_blob");
        let bytes = parcel.into_bytes().expect("into_bytes");
        // 1 MB inline, plus the length and the tag. Nothing was handed
        // to a file descriptor, so the bytes carry the whole payload.
        assert!(bytes.len() >= data.len() + 8);

        let mut parcel = Parcel::from_slice(&bytes);
        let blob = parcel.read_blob().expect("read_blob");
        assert_eq!(blob.inline().map(<[u8]>::len), Some(data.len()));
        assert_eq!(blob.to_vec().expect("to_vec"), data);
        // `mutable_copy` asked for a shared region and could not have
        // one; an inline blob is never mutable, and says so.
        assert!(!blob.is_mutable());
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
