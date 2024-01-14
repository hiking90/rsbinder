// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

mod sys;
mod process_state;
pub mod thread_state;
pub mod status;
mod macros;
pub mod native;
mod binder;
mod binder_object;
pub mod parcel;
pub mod binderfs;
pub mod parcelable;
pub mod proxy;
pub mod file_descriptor;
pub mod parcelable_holder;
pub mod error;

pub use process_state::ProcessState;
pub use parcel::Parcel;
pub use status::{ExceptionCode, Status};
pub use error::{Result, StatusCode};
pub use binder::*;
pub use proxy::*;
pub use native::*;
pub use parcelable::*;
pub use file_descriptor::ParcelFileDescriptor;
pub use parcelable_holder::{ParcelableHolder, ParcelableMetadata};

pub const DEFAULT_BINDER_CONTROL_PATH: &str = "/dev/binderfs/binder-control";
pub const DEFAULT_BINDER_PATH: &str = "/dev/binderfs/binder";
pub const DEFAULT_BINDERFS_PATH: &str = "/dev/binderfs";

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use crate::*;
    #[test]
    #[cfg(target_os = "linux")]
    fn process_state() {
        ProcessState::init("/dev/binderfs/binder", 0);
    }
}
