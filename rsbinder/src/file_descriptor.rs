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

use std::os::unix::io::{AsRawFd, IntoRawFd, OwnedFd, RawFd};

/// File descriptor wrapper for binder IPC.
///
/// `ParcelFileDescriptor` is a Rust equivalent of the Java `android.os.ParcelFileDescriptor`,
/// providing safe transmission of file descriptors through binder IPC while ensuring
/// proper ownership and automatic cleanup.
#[derive(Debug)]
pub struct ParcelFileDescriptor(OwnedFd);

impl ParcelFileDescriptor {
    /// Create a new `ParcelFileDescriptor` from any type that can be converted to `OwnedFd`.
    pub fn new<F: Into<OwnedFd>>(fd: F) -> Self {
        Self(fd.into())
    }
}

impl AsRef<OwnedFd> for ParcelFileDescriptor {
    fn as_ref(&self) -> &OwnedFd {
        &self.0
    }
}

impl From<ParcelFileDescriptor> for OwnedFd {
    fn from(fd: ParcelFileDescriptor) -> OwnedFd {
        fd.0
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

impl Serialize for ParcelFileDescriptor {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        // RPC-mode FD policy (subplan 2-2 / 2-7). `None` (default,
        // android-12 fidelity / android-13 default) is the hard
        // `BAD_TYPE` reject — **bit-identical to 2-2** (AC-7.1). `Unix`
        // (both peers opted in + UDS) stashes a dup'd fd for
        // out-of-band `SCM_RIGHTS` transfer and writes only an
        // ancillary-table index in the body (android-13+ shape).
        #[cfg(feature = "rpc")]
        if parcel.is_for_rpc() {
            use crate::rpc::FileDescriptorTransportMode as M;
            match parcel.rpc_fd_mode() {
                M::None => return Err(StatusCode::BadType),
                M::Unix => {
                    let dup = rustix::io::fcntl_dupfd_cloexec(&self.0, 0)?;
                    let idx = parcel.rpc_push_out_fd(dup);
                    if parcel.rpc_record_fd_positions() {
                        // android-13+ **v1+**: the AOSP-faithful
                        // FD-over-RPC Parcel body (subplan 2-11 Phase A).
                        // AOSP `AParcel_writeParcelFileDescriptor(fd>=0)`
                        // = `writeInt32(1)` not-null →
                        // `writeDupParcelFileDescriptor` →
                        // `writeParcelFileDescriptor` = `writeInt32(0)`
                        // hasComm → `writeFileDescriptor` RPC branch:
                        // `dataPos = mDataPos` captured **here** (after
                        // not-null + hasComm), `writeInt32(TYPE_NATIVE_
                        // FILE_DESCRIPTOR=2)`, `writeInt32(mFds.size())`,
                        // `mObjectPositions.insert(upper_bound,dataPos)`.
                        // Pinned byte-exact android-14.0.0_r75…
                        // android-16.0.0_r4 (v1≡v2 for the body; v2 only
                        // merges binder positions into the same sorted
                        // table — subplan 2-8 Phase B). The recorded
                        // position is the **TYPE int32 offset**, not the
                        // not-null marker.
                        parcel.write::<i32>(&1)?; // not-null marker
                        parcel.write::<i32>(&0)?; // hasComm = 0 (no comm fd)
                        let obj_pos = parcel.data_position();
                        parcel.write::<i32>(
                            &crate::rpc::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR,
                        )?;
                        parcel.write::<i32>(&idx)?; // fd-table index (== mFds.size())
                        parcel.rpc_record_object_position(obj_pos);
                    } else {
                        // R34 / v0 (non-versioned): rsbinder's internal
                        // `[present|idx]` shape (subplan 2-7). AOSP
                        // **category-forbids** fd-over-RPC at android-12
                        // r34 and android-13 (v0), so this is a
                        // rsbinder-only symmetric extension — kept
                        // **byte-unchanged** (AC-11.1 / `rpc_fd` 3/0).
                        // No object table on R34/v0, so no position.
                        parcel.write::<i32>(&1)?; // present
                        parcel.write::<i32>(&idx)?; // ancillary fd-table index
                    }
                    return Ok(());
                }
            }
        }

        // Not null
        parcel.write::<i32>(&1)?;
        let dup_fd = rustix::io::fcntl_dupfd_cloexec(&self.0, 0)?;

        parcel.write::<i32>(&0)?;
        let obj = flat_binder_object::new_with_fd(dup_fd.as_raw_fd(), true);
        parcel.write_object(&obj, true)?;

        // The dup_fd has been sent, so the file descriptor is now owned by the Parcel.
        // So, we need to forget the OwnedFd to avoid double-closing the file descriptor.
        let _ = dup_fd.into_raw_fd();

        Ok(())
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
        // RPC-mode FD read (subplan 2-7 / 2-11). The leading `i32` is
        // the not-null marker (`0` ⇒ `None`; AOSP null fd =
        // `writeInt32(0)`, 4 B — profile-independent, unchanged).
        // `None` fd-mode: an fd in an incoming RPC parcel is impossible
        // (the sender was rejected) → BadType. `Unix`: body shape is
        // profile-keyed (must mirror `serialize`):
        //  * v1+ (AOSP-faithful, subplan 2-11): `[not-null|hasComm|
        //    TYPE_NATIVE_FILE_DESCRIPTOR|fdIndex]` + the **strict
        //    object-position read** (Phase B);
        //  * R34/v0: rsbinder's legacy `[present|idx]` (byte-unchanged).
        // The real fd arrived out-of-band via `SCM_RIGHTS`.
        #[cfg(feature = "rpc")]
        if parcel.is_for_rpc() {
            use crate::rpc::FileDescriptorTransportMode as M;
            let present = parcel.read::<i32>()?;
            if present == 0 {
                return Ok(None);
            }
            return match parcel.rpc_fd_mode() {
                M::None => Err(StatusCode::BadType),
                M::Unix if parcel.rpc_record_fd_positions() => {
                    // v1+ AOSP body. `present` was the not-null marker;
                    // next is `hasComm`. rsbinder has no comm channel,
                    // so `hasComm != 0` is `BadValue` — a documented
                    // divergence (AC-11.5): AOSP `readParcelFile
                    // Descriptor` would read a second (comm) fd. Real
                    // libbinder via `AParcel_writeParcelFileDescriptor`
                    // always writes `hasComm == 0`.
                    let has_comm = parcel.read::<i32>()?;
                    if has_comm != 0 {
                        return Err(StatusCode::BadValue);
                    }
                    // Phase B — v1+ strict receive (AOSP
                    // `Parcel::readFileDescriptor`: a `binary_search`
                    // miss in `mObjectPositions` ⇒ **`BAD_TYPE`**). This
                    // is v1 **and** v2 for fd (verified
                    // android-15.0.0_r36) — distinct from the binder
                    // analog (v2-only, `BAD_VALUE`). `pos` is the TYPE
                    // int32 offset (after not-null + hasComm) == the
                    // write-side recorded position.
                    let pos = parcel.data_position();
                    if !parcel.rpc_object_position_present(pos) {
                        return Err(StatusCode::BadType);
                    }
                    let ty = parcel.read::<i32>()?;
                    if ty != crate::rpc::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR {
                        return Err(StatusCode::BadType);
                    }
                    let idx = parcel.read::<i32>()?;
                    if idx < 0 {
                        return Err(StatusCode::BadValue);
                    }
                    let fd = parcel
                        .rpc_take_in_fd(idx as usize)
                        .ok_or(StatusCode::BadValue)?;
                    Ok(Some(ParcelFileDescriptor::new(fd)))
                }
                M::Unix => {
                    // R34 / v0 legacy `[present|idx]` (byte-unchanged).
                    let idx = parcel.read::<i32>()?;
                    if idx < 0 {
                        return Err(StatusCode::BadValue);
                    }
                    let fd = parcel
                        .rpc_take_in_fd(idx as usize)
                        .ok_or(StatusCode::BadValue)?;
                    Ok(Some(ParcelFileDescriptor::new(fd)))
                }
            };
        }

        let present = parcel.read::<i32>()?;
        if present == 0 {
            return Ok(None);
        }

        let has_comm = parcel.read::<i32>()?;
        if has_comm != 0 {
            return Err(StatusCode::BadValue);
        }

        let obj = parcel.read_object(true)?;

        let fd = rustix::io::fcntl_dupfd_cloexec(obj.borrowed_fd(), 0)?;

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

/// Fuzz entrypoint for the `rpc_fd_ancillary` target (subplan 2-7
/// §6.3 / V4 / T7.6): arbitrary body bytes through the RPC `Unix`-mode
/// FD-table decode **with no received fds**. Property: no panic / UB /
/// fd leak — an out-of-bounds or dangling fd-table index is a clean
/// `Err`, never a crash. Not part of the supported API surface.
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

/// Fuzz entrypoint for the **v1+ AOSP-shape** RPC FD decode (subplan
/// 2-11 Phase B / V4): arbitrary body bytes + a hostile object-position
/// table through the strict `[not-null|hasComm|TYPE|idx]` +
/// `binary_search` read, **no received fds**. Property: no panic / UB /
/// fd leak — a forged/unsorted/absent position, a wrong `TYPE`, a
/// non-zero `hasComm`, or a dangling index is a clean `Err`, never a
/// crash. Complements [`__fuzz_rpc_fd_index`] (which only covers the
/// R34 legacy `[present|idx]` path). Not part of the supported API.
#[cfg(feature = "rpc")]
#[doc(hidden)]
pub fn __fuzz_rpc_fd_index_v1(input: &[u8]) {
    // First byte picks how many leading u32s are treated as the
    // (attacker-controlled) object-position table; the rest is the
    // parcel body — so the fuzzer reaches both `binary_search` hit and
    // miss, unsorted tables, and positions past the body.
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
    // No ancillary fds installed: every index must be rejected, not
    // panic / leak.
    let _ = <ParcelFileDescriptor as DeserializeOption>::deserialize_option(&mut p);
}

#[cfg(test)]
mod tests {
    use std::os::fd::FromRawFd;

    use super::*;

    #[test]
    fn test_parcel_file_descriptor() {
        let fd = unsafe { OwnedFd::from_raw_fd(1) };
        let pfd = ParcelFileDescriptor::new(fd);

        assert_eq!(pfd.as_raw_fd(), 1);

        let owned_fd: OwnedFd = pfd.into();

        let pfd = ParcelFileDescriptor::new(owned_fd);

        assert_eq!(pfd.into_raw_fd(), 1);
    }

    // ---- subplan 2-11: AOSP-faithful FD-over-RPC Parcel body ----
    //
    // Device-free byte-exact goldens (AC-11.1) + Phase B strict-read
    // mutant detection (V3). The single-fd write side is asserted here;
    // the full v1+↔v1+ socket round-trip (A0 + A + B) is the hermetic
    // `rpc_fd::fd_v1plus_aosp_roundtrip_*` (AC-11.0/11.4).

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

    /// AC-11.1: v1+ (`record_fd_positions`) writes the AOSP-faithful
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

    /// AC-11.1 / AC-11.4: R34 / v0 (no object table) keeps rsbinder's
    /// legacy `[present=1][fdIndex=0]` (8 B) **byte-unchanged**, no
    /// position — the anchor that `rpc_fd` 3/0 stays green.
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

    /// AC-11.1: a null fd is `writeInt32(0)` (4 B), **no TYPE, no
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

    /// V3 mutants — each malformed/forged v1+ body must be a clean
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

        // (2) position omitted from the object table ⇒ Phase B strict
        //     miss ⇒ BadType (AOSP fd: binary_search miss ⇒ BAD_TYPE).
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

        // (5) hasComm != 0 (AC-11.5 documented divergence: rsbinder has
        //     no comm channel — AOSP would read a second fd).
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
