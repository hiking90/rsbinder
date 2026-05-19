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
                    // AOSP `Parcel::writeFileDescriptor`: `dataPos =
                    // mDataPos` captured **before** the type/present
                    // int32, then `mObjectPositions.insert(upper_bound,
                    // dataPos)`. rsbinder's RPC FD body is still its
                    // internal `[present|idx]` shape (the AOSP-faithful
                    // `[TYPE_NATIVE_FILE_DESCRIPTOR|fdCount]` body =
                    // future §7 FD-object-table-over-libbinder); Phase B
                    // adds only the *position bookkeeping* so the
                    // sorted binder+FD table is exercised. Recorded
                    // only on the android-13+ v1+ profile (R34 ⇒
                    // `false` ⇒ 2-7 wire byte-unchanged).
                    let obj_pos = parcel.data_position();
                    parcel.write::<i32>(&1)?; // present
                    parcel.write::<i32>(&idx)?; // ancillary fd-table index
                    if parcel.rpc_record_fd_positions() {
                        parcel.rpc_record_object_position(obj_pos);
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
        // RPC-mode FD read (subplan 2-7). `None`: an FD in an incoming
        // RPC parcel is impossible (the sender was rejected) → BadType.
        // `Unix`: body is `i32 present, i32 ancillary-index`; the real
        // fd arrived out-of-band via `SCM_RIGHTS`.
        #[cfg(feature = "rpc")]
        if parcel.is_for_rpc() {
            use crate::rpc::FileDescriptorTransportMode as M;
            let present = parcel.read::<i32>()?;
            if present == 0 {
                return Ok(None);
            }
            return match parcel.rpc_fd_mode() {
                M::None => Err(StatusCode::BadType),
                M::Unix => {
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
}
