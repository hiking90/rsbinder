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
