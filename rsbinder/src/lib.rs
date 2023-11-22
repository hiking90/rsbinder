// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

#[macro_use]
extern crate lazy_static;

mod sys;
mod process_state;
pub mod thread_state;
mod error;
mod macros;
pub mod native;
mod binder;
pub mod parcel;
pub mod binderfs;
pub mod parcelable;
pub mod proxy;
pub mod file_descriptor;
pub mod parcelable_holder;

pub use process_state::ProcessState;
// pub use thread_state::Se;
pub use parcel::Parcel;
pub use error::{StatusCode, Error, Result, ExceptionCode, Status};
pub use binder::*;
pub use proxy::*;
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
        let process = ProcessState::as_self();
        process.init("/dev/binderfs/binder", 0);
    }

    #[test]
    fn thread_state() {
        process_state();
    }
}
