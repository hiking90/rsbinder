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

use crate::{
    Deserialize, DeserializeArray, DeserializeOption, Serialize, SerializeArray,
    SerializeOption, Parcel,
    binder_object::flat_binder_object,
};
use crate::error::{Result, StatusCode};

use std::fs::File;
use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd, FromRawFd};

/// Rust version of the Java class android.os.ParcelFileDescriptor
#[derive(Debug)]
pub struct ParcelFileDescriptor(File);

impl ParcelFileDescriptor {
    /// Create a new `ParcelFileDescriptor`
    pub fn new(file: File) -> Self {
        Self(file)
    }
}

impl AsRef<File> for ParcelFileDescriptor {
    fn as_ref(&self) -> &File {
        &self.0
    }
}

impl From<ParcelFileDescriptor> for File {
    fn from(file: ParcelFileDescriptor) -> File {
        file.0
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
        let fd = self.0.as_raw_fd();

        // Not null
        parcel.write::<i32>(&1)?;
        let dup_fd = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(0))?;

        let result = || -> Result<()> {
            parcel.write::<i32>(&0)?;
            let obj = flat_binder_object::new_with_fd(dup_fd, true);
            parcel.write_object(&obj, true)?;
            Ok(())
        }();

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                // Close the duplicated fd
                nix::unistd::close(dup_fd)?;
                Err(e)
            }
        }
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

        let fd = nix::fcntl::fcntl(obj.handle() as _, nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(0))?;

        let file = unsafe {
            // Safety: At this point, we know that the file descriptor was
            // not -1, so must be a valid, owned file descriptor which we
            // can safely turn into a `File`.
            File::from_raw_fd(fd)
        };

        Ok(Some(ParcelFileDescriptor::new(file)))
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
