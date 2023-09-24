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
    SerializeOption, Parcel
};
use crate::error::{Result, StatusCode};

use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};

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
        todo!()
        // let status = unsafe {
        //     // Safety: `Parcel` always contains a valid pointer to an
        //     // `AParcel`. Likewise, `ParcelFileDescriptor` always contains a
        //     // valid file, so we can borrow a valid file
        //     // descriptor. `AParcel_writeParcelFileDescriptor` does NOT take
        //     // ownership of the fd, so we need not duplicate it first.
        //     sys::AParcel_writeParcelFileDescriptor(parcel.as_native_mut(), fd)
        // };
        // status_result(status)
    }
}

impl SerializeArray for ParcelFileDescriptor {}

impl SerializeOption for ParcelFileDescriptor {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        if let Some(f) = this {
            f.serialize(parcel)
        } else {
            todo!()
            // let status = unsafe {
            //     // Safety: `Parcel` always contains a valid pointer to an
            //     // `AParcel`. `AParcel_writeParcelFileDescriptor` accepts the
            //     // value `-1` as the file descriptor to signify serializing a
            //     // null file descriptor.
            //     sys::AParcel_writeParcelFileDescriptor(parcel.as_native_mut(), -1i32)
            // };
            // status_result(status)
        }
    }
}

impl DeserializeOption for ParcelFileDescriptor {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        let mut fd = -1i32;
        todo!();
        // unsafe {
        //     // Safety: `Parcel` always contains a valid pointer to an
        //     // `AParcel`. We pass a valid mutable pointer to an i32, which
        //     // `AParcel_readParcelFileDescriptor` assigns the valid file
        //     // descriptor into, or `-1` if deserializing a null file
        //     // descriptor. The read function passes ownership of the file
        //     // descriptor to its caller if it was non-null, so we must take
        //     // ownership of the file and ensure that it is eventually closed.
        //     status_result(sys::AParcel_readParcelFileDescriptor(
        //         parcel.as_native(),
        //         &mut fd,
        //     ))?;
        // }
        if fd < 0 {
            Ok(None)
        } else {
            let file = unsafe {
                // Safety: At this point, we know that the file descriptor was
                // not -1, so must be a valid, owned file descriptor which we
                // can safely turn into a `File`.
                File::from_raw_fd(fd)
            };
            Ok(Some(ParcelFileDescriptor::new(file)))
        }
    }
}

impl Deserialize for ParcelFileDescriptor {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Deserialize::deserialize(parcel)
            .transpose()
            .unwrap_or(Err(StatusCode::UnexpectedNull.into()))
    }
}

impl DeserializeArray for ParcelFileDescriptor {}
