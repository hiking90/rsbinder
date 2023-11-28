// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

extern crate lazy_static;

mod sys;
mod process_state;
pub mod thread_state;
mod error;
mod status;
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

pub use process_state::ProcessState;
pub use parcel::Parcel;
pub use error::{StatusCode, Result};
pub use status::*;
pub use binder::*;
pub use proxy::*;
pub use native::*;
pub use parcelable::*;
pub use file_descriptor::ParcelFileDescriptor;
pub use parcelable_holder::ParcelableHolder;
// pub use ref_base::*;

pub const DEFAULT_BINDER_CONTROL_PATH: &str = "/dev/binderfs/binder-control";
pub const DEFAULT_BINDER_PATH: &str = "/dev/binderfs/binder";


#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn process_state() {
        // let process = ProcessState::as_self();
        ProcessState::init("/dev/binderfs/binder", 0);
    }
}
