// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

/*
 * Copyright (C) 2020 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! File descriptor wrapper for binder IPC.
//!
//! This module provides `ParcelFileDescriptor`, a wrapper around file descriptors
//! that can be safely transmitted through binder IPC while maintaining proper
//! ownership semantics and automatic cleanup.

use crate::error::{Result, StatusCode};
use crate::{
    binder_object::flat_binder_object, Deserialize, DeserializeArray, DeserializeOption, Parcel,
    Serialize, SerializeArray, SerializeOption,
};

use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd, RawFd};

/// File descriptor wrapper for binder IPC.
///
/// `ParcelFileDescriptor` is a Rust equivalent of the Java `android.os.ParcelFileDescriptor`,
/// providing safe transmission of file descriptors through binder IPC while ensuring
/// proper ownership and automatic cleanup.
#[derive(Debug)]
pub struct ParcelFileDescriptor(OwnedFd);

impl ParcelFileDescriptor {
    /// Create a new `ParcelFileDescriptor` from any type that can be converted to `OwnedFd`.
    ///
    /// For the common concrete cases prefer the `From` impls
    /// (`ParcelFileDescriptor::from(owned_fd)` / `from(file)`); this generic
    /// constructor covers any other `Into<OwnedFd>` source.
    pub fn new<F: Into<OwnedFd>>(fd: F) -> Self {
        Self(fd.into())
    }

    /// Duplicate the underlying file descriptor into a new owned
    /// `ParcelFileDescriptor`. The new fd is `O_CLOEXEC` (`fcntl(F_DUPFD_CLOEXEC)`,
    /// which always sets it regardless of the source's flag), matching AOSP
    /// `ParcelFileDescriptor::dup`. Useful when you must both keep an fd and
    /// hand a copy to a parcel.
    pub fn try_clone(&self) -> Result<Self> {
        let dup = rustix::io::fcntl_dupfd_cloexec(&self.0, 0)?;
        Ok(Self(dup))
    }
}

impl AsRef<OwnedFd> for ParcelFileDescriptor {
    fn as_ref(&self) -> &OwnedFd {
        &self.0
    }
}

impl AsFd for ParcelFileDescriptor {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl From<ParcelFileDescriptor> for OwnedFd {
    fn from(fd: ParcelFileDescriptor) -> OwnedFd {
        fd.0
    }
}

impl From<OwnedFd> for ParcelFileDescriptor {
    fn from(fd: OwnedFd) -> Self {
        Self(fd)
    }
}

impl From<std::fs::File> for ParcelFileDescriptor {
    fn from(file: std::fs::File) -> Self {
        Self(file.into())
    }
}

impl AsRawFd for ParcelFileDescriptor {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for ParcelFileDescriptor {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl PartialEq for ParcelFileDescriptor {
    // Since ParcelFileDescriptors own the FD, if this function ever returns true (and it is used to
    // compare two different objects), then it would imply that an FD is double-owned.
    fn eq(&self, other: &Self) -> bool {
        self.as_raw_fd() == other.as_raw_fd()
    }
}

impl Eq for ParcelFileDescriptor {}

/// Which RPC fd body a parcel carries. The single place the
/// `FileDescriptorTransportMode` policy is decided for fd writes/reads.
#[cfg(feature = "rpc")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RpcFdProfile {
    /// R34 / v0: rsbinder-only bare ancillary index (AOSP
    /// category-forbids fd-over-RPC there).
    V0,
    /// android-13+ v1+: AOSP `TYPE_NATIVE_FILE_DESCRIPTOR` + index with
    /// the object position recorded (plan/2-11).
    V1Plus,
}

/// `None` ⇒ not an RPC parcel. `Err(BadType)` ⇒ RPC parcel whose
/// negotiated fd mode forbids fds (the default, android-12/13 fidelity).
#[cfg(feature = "rpc")]
fn rpc_fd_profile(parcel: &Parcel) -> Result<Option<RpcFdProfile>> {
    use crate::rpc::FileDescriptorTransportMode as M;
    if !parcel.is_for_rpc() {
        return Ok(None);
    }
    match parcel.rpc_fd_mode() {
        M::None => Err(StatusCode::BadType),
        M::Unix if parcel.rpc_record_fd_positions() => Ok(Some(RpcFdProfile::V1Plus)),
        M::Unix => Ok(Some(RpcFdProfile::V0)),
    }
}

/// AOSP `Parcel::writeFileDescriptor` equivalent: the **bare** fd object
/// with no not-null / comm markers — `BINDER_TYPE_FD` on the kernel
/// path, the fd-table entry (`TYPE_NATIVE_FILE_DESCRIPTOR` + index, with
/// the object position recorded) on an RPC `Unix` fd-mode session.
/// The fd is dup'd (`F_DUPFD_CLOEXEC`); the caller keeps its own.
///
/// [`ParcelFileDescriptor`] layers the AIDL markers on top of this;
/// handwritten AOSP interfaces such as `android.utils.IMemoryHeap`
/// use this raw form directly.
pub(crate) fn write_raw_fd(parcel: &mut Parcel, fd: BorrowedFd<'_>) -> Result<()> {
    write_raw_owned_fd(parcel, dup_for_parcel(parcel, fd)?)
}

/// The `F_DUPFD_CLOEXEC` half of [`write_raw_fd`] and the fd-mode gate,
/// separated so callers that prefix markers can dup *before* writing
/// anything — a dup failure (`EMFILE`) or a rejected fd mode then
/// leaves the parcel untouched.
fn dup_for_parcel(parcel: &Parcel, fd: BorrowedFd<'_>) -> Result<OwnedFd> {
    #[cfg(feature = "rpc")]
    rpc_fd_profile(parcel)?;
    #[cfg(not(feature = "rpc"))]
    let _ = parcel;
    Ok(rustix::io::fcntl_dupfd_cloexec(fd, 0)?)
}

/// Body half of [`write_raw_fd`]: `dup` was produced by
/// [`dup_for_parcel`] for this same parcel (so the fd mode is already
/// accepted); ownership moves into the parcel's object table or RPC
/// ancillary fd table only after the body bytes are written.
fn write_raw_owned_fd(parcel: &mut Parcel, dup: OwnedFd) -> Result<()> {
    #[cfg(feature = "rpc")]
    if let Some(profile) = rpc_fd_profile(parcel)? {
        // Index the fd will get; push only once the body is in place so
        // a failed write cannot leave a ghost entry in the table.
        let idx = parcel.rpc_out_fds().len() as i32;
        match profile {
            RpcFdProfile::V1Plus => {
                // AOSP `writeFileDescriptor` RPC branch: the recorded object
                // position is the TYPE int32 offset (plan/2-11).
                let obj_pos = parcel.data_position();
                parcel.write::<i32>(&crate::rpc::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR)?;
                parcel.write::<i32>(&idx)?;
                parcel.rpc_record_object_position(obj_pos);
            }
            RpcFdProfile::V0 => parcel.write::<i32>(&idx)?,
        }
        let pushed = parcel.rpc_push_out_fd(dup);
        debug_assert_eq!(pushed, idx);
        return Ok(());
    }

    let obj = flat_binder_object::new_with_fd(dup.as_raw_fd(), true);
    parcel.write_object(&obj, true)?;
    // The dup has been sent, so the file descriptor is now owned by the Parcel.
    // So, we need to forget the OwnedFd to avoid double-closing the file descriptor.
    let _ = dup.into_raw_fd();
    Ok(())
}

/// AOSP `Parcel::readFileDescriptor` equivalent of [`write_raw_fd`]:
/// reads the bare fd object and returns an owned fd — a dup of the
/// parcel's object on the kernel path; on RPC the ancillary-table entry
/// itself, **consumed** (a second read of the same position is
/// `BadValue`).
pub(crate) fn read_raw_fd(parcel: &mut Parcel) -> Result<OwnedFd> {
    #[cfg(feature = "rpc")]
    if let Some(profile) = rpc_fd_profile(parcel)? {
        if profile == RpcFdProfile::V1Plus {
            // AOSP readFileDescriptor: object-position miss ⇒ BAD_TYPE
            // for v1 and v2 alike (plan/2-11).
            let pos = parcel.data_position();
            if !parcel.rpc_object_position_present(pos) {
                return Err(StatusCode::BadType);
            }
            let ty = parcel.read::<i32>()?;
            if ty != crate::rpc::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR {
                return Err(StatusCode::BadType);
            }
        }
        let idx = parcel.read::<i32>()?;
        if idx < 0 {
            return Err(StatusCode::BadValue);
        }
        return parcel
            .rpc_take_in_fd(idx as usize)
            .ok_or(StatusCode::BadValue);
    }

    let obj = parcel.read_object(true)?;
    // `read_object` checks offset-table membership, not the type: a
    // BINDER_TYPE_HANDLE placed here would otherwise be reinterpreted as
    // an fd (AOSP readFileDescriptor also returns BAD_TYPE).
    if obj.header_type() != crate::sys::BINDER_TYPE_FD {
        return Err(StatusCode::BadType);
    }
    Ok(rustix::io::fcntl_dupfd_cloexec(obj.borrowed_fd(), 0)?)
}

impl Serialize for ParcelFileDescriptor {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        // AIDL `ParcelFileDescriptor` body = not-null marker + hasComm +
        // the raw fd object. Over RPC the v1+ (AOSP-faithful) shape keeps
        // both markers (`AParcel_writeParcelFileDescriptor` = writeInt32(1)
        // → writeInt32(0) hasComm → writeFileDescriptor); the R34/v0
        // rsbinder-only shape is `[present|idx]`.
        let dup = dup_for_parcel(parcel, self.0.as_fd())?;
        #[cfg(feature = "rpc")]
        if let Some(profile) = rpc_fd_profile(parcel)? {
            parcel.write::<i32>(&1)?; // not-null marker / present
            if profile == RpcFdProfile::V1Plus {
                parcel.write::<i32>(&0)?; // hasComm = 0 (no comm fd)
            }
            return write_raw_owned_fd(parcel, dup);
        }

        // Not null
        parcel.write::<i32>(&1)?;
        parcel.write::<i32>(&0)?;
        write_raw_owned_fd(parcel, dup)
    }
}

impl SerializeArray for ParcelFileDescriptor {}

impl SerializeOption for ParcelFileDescriptor {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        if let Some(f) = this {
            f.serialize(parcel)
        } else {
            parcel.write::<i32>(&0)
        }
    }
}

impl DeserializeOption for ParcelFileDescriptor {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        // The leading `i32` is the not-null marker (`0` ⇒ `None`; AOSP
        // null fd = `writeInt32(0)`, 4 B — profile-independent). The
        // body mirrors `serialize`.
        let present = parcel.read::<i32>()?;
        if present == crate::NULL_PARCELABLE_FLAG {
            return Ok(None);
        }
        // AOSP `Parcel::readData(Parcelable*)`: anything but the not-null
        // flag (`1`) is `UNEXPECTED_NULL`, not "present". The default
        // `DeserializeOption` and `ParcelableHolder` already reject this;
        // the fd path must not be the one lenient decoder.
        if present != crate::NON_NULL_PARCELABLE_FLAG {
            return Err(StatusCode::UnexpectedNull);
        }

        #[cfg(feature = "rpc")]
        if let Some(profile) = rpc_fd_profile(parcel)? {
            if profile == RpcFdProfile::V1Plus {
                // rsbinder has no comm channel: a non-zero hasComm is
                // BadValue (real libbinder always writes 0 here).
                let has_comm = parcel.read::<i32>()?;
                if has_comm != 0 {
                    return Err(StatusCode::BadValue);
                }
            }
            return Ok(Some(ParcelFileDescriptor::new(read_raw_fd(parcel)?)));
        }

        // AOSP `ParcelFileDescriptor.writeToParcel` (frameworks/base
        // core/java/android/os/ParcelFileDescriptor.java): `writeInt(hasComm)`,
        // then `writeFileDescriptor(mFd)`, then `writeFileDescriptor(mCommFd)`
        // when `hasComm != 0` (a Java `createReliablePipe()` /
        // `createReliableSocketPair()` PFD). Read the main fd object first,
        // regardless of `hasComm`.
        let has_comm = parcel.read::<i32>()?;
        let fd = read_raw_fd(parcel)?;

        // Reliable-PFD comm channel (Java `createReliablePipe()` /
        // `createReliableSocketPair()`): consume the second fd object so the
        // parcel cursor stays aligned, and — as AOSP
        // `Parcel::readParcelFileDescriptor` does — tell the sender the
        // channel is detached (`DETACHED = 2`, big-endian). Without that
        // notice the sender's `checkError()` reports a clean close instead
        // of `FileDescriptorDetachedException`. The parcel still owns the
        // comm fd and closes it on `BC_FREE_BUFFER`.
        if has_comm != 0 {
            let comm = parcel.read_object(true)?;
            if comm.header_type() != crate::sys::BINDER_TYPE_FD {
                return Err(StatusCode::BadType);
            }
            const DETACHED: i32 = 2;
            let notice = DETACHED.to_be_bytes();
            loop {
                match rustix::io::write(comm.borrowed_fd(), &notice) {
                    Ok(n) if n == notice.len() => break,
                    Err(rustix::io::Errno::INTR) => continue,
                    _ => return Err(StatusCode::BadType),
                }
            }
        }

        Ok(Some(ParcelFileDescriptor::new(fd)))
    }
}

impl Deserialize for ParcelFileDescriptor {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Deserialize::deserialize(parcel)
            .transpose()
            .unwrap_or(Err(StatusCode::UnexpectedNull))
    }
}

impl DeserializeArray for ParcelFileDescriptor {}

/// Fuzz entrypoint for the `rpc_fd_ancillary` target: arbitrary body
/// bytes through the RPC `Unix`-mode FD-table decode **with no received
/// fds**. Property: no panic / UB / fd leak — an out-of-bounds or
/// dangling fd-table index is a clean `Err`, never a crash. Not part of
/// the supported API surface.
#[cfg(feature = "rpc")]
#[doc(hidden)]
pub fn __fuzz_rpc_fd_index(input: &[u8]) {
    let mut p = Parcel::from_vec(input.to_vec());
    p.set_for_rpc(true);
    p.set_rpc_fd_mode(crate::rpc::FileDescriptorTransportMode::Unix);
    // No ancillary fds installed: every index must be rejected, not
    // panic / leak.
    let _ = <ParcelFileDescriptor as DeserializeOption>::deserialize_option(&mut p);
}

/// Fuzz entrypoint for the **v1+ AOSP-shape** RPC FD decode: arbitrary
/// body bytes + a hostile object-position table through the strict
/// `[not-null|hasComm|TYPE|idx]` +
/// `binary_search` read, **no received fds**. Property: no panic / UB /
/// fd leak — a forged/unsorted/absent position, a wrong `TYPE`, a
/// non-zero `hasComm`, or a dangling index is a clean `Err`, never a
/// crash. Complements [`__fuzz_rpc_fd_index`] (which only covers the
/// R34 legacy `[present|idx]` path). Not part of the supported API.
#[cfg(feature = "rpc")]
#[doc(hidden)]
pub fn __fuzz_rpc_fd_index_v1(input: &[u8]) {
    let mut p = fuzz_v1_parcel(input);
    // No ancillary fds installed: every index must be rejected, not
    // panic / leak.
    let _ = <ParcelFileDescriptor as DeserializeOption>::deserialize_option(&mut p);
}

/// Shared by the v1+ fuzz entries: the first byte picks how many
/// leading u32s form the (attacker-controlled) object-position table;
/// the rest is the parcel body — so the fuzzer reaches both
/// `binary_search` hit and miss, unsorted tables, and positions past
/// the body.
#[cfg(feature = "rpc")]
fn fuzz_v1_parcel(input: &[u8]) -> Parcel {
    let (n_pos, rest) = match input.split_first() {
        Some((&n, rest)) => ((n % 16) as usize, rest),
        None => (0, input),
    };
    let take = (n_pos * 4).min(rest.len());
    let positions: Vec<u32> = rest[..take]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let mut p = Parcel::from_vec(rest[take..].to_vec());
    p.set_for_rpc(true);
    p.set_rpc_fd_mode(crate::rpc::FileDescriptorTransportMode::Unix);
    p.set_rpc_record_fd_positions(true); // v1+ AOSP body + strict read
    p.rpc_set_object_positions(positions);
    p
}

/// Fuzz entrypoint for the **bare** fd decode ([`read_raw_fd`], the
/// `android.utils.IMemoryHeap` wire) over RPC `Unix` fd-mode, **no
/// received fds**. First byte selects the profile: even = R34/v0 bare
/// index, odd = v1+ `[TYPE|idx]` with the next byte sizing a hostile
/// object-position table (as in [`__fuzz_rpc_fd_index_v1`]). Property:
/// no panic / UB / fd leak — every forged position, type, or index is
/// a clean `Err`. Not part of the supported API.
#[cfg(feature = "rpc")]
#[doc(hidden)]
pub fn __fuzz_rpc_raw_fd(input: &[u8]) {
    let Some((&profile, rest)) = input.split_first() else {
        return;
    };
    let mut p = if profile % 2 == 0 {
        let mut p = Parcel::from_vec(rest.to_vec());
        p.set_for_rpc(true);
        p.set_rpc_fd_mode(crate::rpc::FileDescriptorTransportMode::Unix);
        p
    } else {
        fuzz_v1_parcel(rest)
    };
    let _ = read_raw_fd(&mut p);
}

#[cfg(test)]
mod tests {
    use std::os::fd::FromRawFd;

    use super::*;

    #[test]
    fn test_parcel_file_descriptor() {
        // A fd this test actually owns — not stdout (fd 1), which std and
        // the test harness already own and which a failing assert would
        // then close during unwind.
        let f = std::fs::File::open("/dev/null").expect("/dev/null");
        let raw = f.as_raw_fd();
        let pfd = ParcelFileDescriptor::from(f);
        assert_eq!(pfd.as_raw_fd(), raw);

        let owned_fd: OwnedFd = pfd.into();
        let pfd = ParcelFileDescriptor::new(owned_fd);
        assert_eq!(pfd.into_raw_fd(), raw);

        // SAFETY: `into_raw_fd` just relinquished ownership of `raw`, so
        // nothing else in this process owns it; reclaim it here to close.
        drop(unsafe { OwnedFd::from_raw_fd(raw) });
    }

    // E9: From<File>/From<OwnedFd>, AsFd, and try_clone (dup) ergonomics.
    #[test]
    fn test_pfd_conversions_and_try_clone() {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .expect("/dev/null");
        // From<std::fs::File>
        let pfd: ParcelFileDescriptor = f.into();
        // AsFd delegates to the inner OwnedFd.
        let _borrowed: BorrowedFd<'_> = pfd.as_fd();
        assert!(pfd.as_raw_fd() >= 0);

        // try_clone dups (O_CLOEXEC) into a distinct fd.
        let cloned = pfd.try_clone().expect("try_clone");
        assert_ne!(
            pfd.as_raw_fd(),
            cloned.as_raw_fd(),
            "dup must allocate a new fd"
        );

        // From<OwnedFd> round-trips through OwnedFd.
        let owned: OwnedFd = cloned.into();
        let _pfd2: ParcelFileDescriptor = owned.into();
    }

    // ---- AOSP-faithful FD-over-RPC Parcel body ----
    //
    // Device-free byte-exact goldens + strict-read mutant detection.
    // The single-fd write side is asserted here; the full v1+↔v1+
    // socket round-trip is the hermetic
    // `rpc_fd::fd_v1plus_aosp_roundtrip_*`.

    #[cfg(feature = "rpc")]
    fn dev_null_pfd() -> ParcelFileDescriptor {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .expect("/dev/null");
        ParcelFileDescriptor::new(f)
    }

    #[cfg(feature = "rpc")]
    fn rpc_parcel(record_fd_positions: bool) -> Parcel {
        use crate::rpc::FileDescriptorTransportMode as M;
        let mut p = Parcel::new();
        p.set_for_rpc(true);
        p.set_rpc_fd_mode(M::Unix);
        p.set_rpc_record_fd_positions(record_fd_positions);
        p
    }

    /// v1+ (`record_fd_positions`) writes the AOSP-faithful
    /// `[not-null=1][hasComm=0][TYPE=2][fdIndex=0]` (16 B) and records
    /// the **TYPE int32 offset (= start+8)**, not the not-null marker.
    #[cfg(feature = "rpc")]
    #[test]
    fn rpc_fd_v1_body_golden() {
        let mut p = rpc_parcel(true);
        dev_null_pfd().serialize(&mut p).expect("serialize v1+ fd");
        let mut want = Vec::new();
        want.extend_from_slice(&1i32.to_le_bytes()); // not-null
        want.extend_from_slice(&0i32.to_le_bytes()); // hasComm = 0
        want.extend_from_slice(&2i32.to_le_bytes()); // TYPE_NATIVE_FILE_DESCRIPTOR
        want.extend_from_slice(&0i32.to_le_bytes()); // fdIndex (first out fd)
        assert_eq!(p.rpc_data_bytes(), &want[..], "v1+ AOSP fd body");
        assert_eq!(
            p.rpc_object_positions(),
            &[8u32],
            "recorded position = TYPE int32 offset (after not-null+hasComm), not +0"
        );
    }

    /// R34 / v0 (no object table) keeps rsbinder's legacy
    /// `[present=1][fdIndex=0]` (8 B) **byte-unchanged**, no position.
    #[cfg(feature = "rpc")]
    #[test]
    fn rpc_fd_r34_body_byte_unchanged() {
        let mut p = rpc_parcel(false);
        dev_null_pfd().serialize(&mut p).expect("serialize r34 fd");
        let mut want = Vec::new();
        want.extend_from_slice(&1i32.to_le_bytes()); // present
        want.extend_from_slice(&0i32.to_le_bytes()); // fdIndex
        assert_eq!(p.rpc_data_bytes(), &want[..], "R34 legacy fd body");
        assert!(
            p.rpc_object_positions().is_empty(),
            "R34/v0 has no object table"
        );
    }

    /// A null fd is `writeInt32(0)` (4 B), **no TYPE, no
    /// position**, at *both* profiles (an easy reshape mistake is to
    /// grow a position for null at v1+).
    #[cfg(feature = "rpc")]
    #[test]
    fn rpc_fd_null_body_unchanged_both_profiles() {
        for record in [false, true] {
            let mut p = rpc_parcel(record);
            <ParcelFileDescriptor as SerializeOption>::serialize_option(None, &mut p)
                .expect("serialize None fd");
            assert_eq!(
                p.rpc_data_bytes(),
                &0i32.to_le_bytes()[..],
                "null fd = writeInt32(0) (record_fd_positions={record})"
            );
            assert!(
                p.rpc_object_positions().is_empty(),
                "null fd records no position (record_fd_positions={record})"
            );
        }
    }

    #[cfg(feature = "rpc")]
    fn v1_reader(body: &[u8], positions: Vec<u32>) -> Parcel {
        use crate::rpc::FileDescriptorTransportMode as M;
        let mut p = Parcel::from_vec(body.to_vec());
        p.set_for_rpc(true);
        p.set_rpc_fd_mode(M::Unix);
        p.set_rpc_record_fd_positions(true);
        p.rpc_set_object_positions(positions);
        p.set_data_position(0);
        p
    }

    /// Each malformed/forged v1+ body must be a clean
    /// `Err`, never a panic or a mis-parse (the symmetric-illusion trap
    /// is exactly why these are explicit).
    #[cfg(feature = "rpc")]
    #[test]
    fn rpc_fd_v1_strict_read_rejects_mutants() {
        let ok_body = {
            let mut b = Vec::new();
            b.extend_from_slice(&1i32.to_le_bytes()); // not-null
            b.extend_from_slice(&0i32.to_le_bytes()); // hasComm
            b.extend_from_slice(&2i32.to_le_bytes()); // TYPE
            b.extend_from_slice(&0i32.to_le_bytes()); // idx
            b
        };
        let de = <ParcelFileDescriptor as DeserializeOption>::deserialize_option;

        // (1) legacy R34 `[present=1][idx]` fed to a v1+ reader: the
        //     second i32 is misread as hasComm; idx=7 ⇒ hasComm!=0.
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&1i32.to_le_bytes());
        legacy.extend_from_slice(&7i32.to_le_bytes());
        assert_eq!(
            de(&mut v1_reader(&legacy, vec![8])).unwrap_err(),
            StatusCode::BadValue,
            "legacy [present|idx] vs v1+ reader (hasComm!=0)"
        );

        // (2) position omitted from the object table ⇒ strict miss ⇒
        //     BadType (AOSP fd: binary_search miss ⇒ BAD_TYPE).
        assert_eq!(
            de(&mut v1_reader(&ok_body, vec![])).unwrap_err(),
            StatusCode::BadType,
            "unrecorded fd position rejected (strict v1+)"
        );

        // (3) position recorded at +0 (not-null marker) instead of the
        //     +8 TYPE offset — the obj_pos-captured-too-early mutant.
        assert_eq!(
            de(&mut v1_reader(&ok_body, vec![0])).unwrap_err(),
            StatusCode::BadType,
            "position must point at the TYPE int32 (start+8), not +0"
        );

        // (4) wrong object type (TYPE_BINDER=1 instead of 2).
        let mut wrong_ty = ok_body.clone();
        wrong_ty[8..12].copy_from_slice(&1i32.to_le_bytes());
        assert_eq!(
            de(&mut v1_reader(&wrong_ty, vec![8])).unwrap_err(),
            StatusCode::BadType,
            "TYPE != TYPE_NATIVE_FILE_DESCRIPTOR rejected"
        );

        // (5) hasComm != 0 (documented divergence: rsbinder has no comm
        //     channel — AOSP would read a second fd).
        let mut comm = ok_body.clone();
        comm[4..8].copy_from_slice(&1i32.to_le_bytes());
        assert_eq!(
            de(&mut v1_reader(&comm, vec![8])).unwrap_err(),
            StatusCode::BadValue,
            "hasComm != 0 rejected (AC-11.5)"
        );

        // Sanity: the well-formed body passes the strict/type/hasComm
        // gates and only then fails on the *absent in-fd* (BadValue
        // from `rpc_take_in_fd`) — proving the gates above are what
        // rejected (1)–(5), not an earlier accident.
        assert_eq!(
            de(&mut v1_reader(&ok_body, vec![8])).unwrap_err(),
            StatusCode::BadValue,
            "well-formed body reaches the fd lookup (no in-fd installed)"
        );
    }
}
