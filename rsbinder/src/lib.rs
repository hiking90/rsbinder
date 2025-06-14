// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A library for Binder communication developed purely in Rust.
//!
//! # License
//!
//! Licensed under Apache License, Version 2.0.
//!
//! # References
//!
//! * [AIDL](https://source.android.com/docs/core/architecture/aidl)
//! * [Binder](https://source.android.com/docs/core/architecture/hidl/binder-ipc)
//!

mod binder;
#[cfg(feature = "async")]
pub mod binder_async;
mod binder_object;
pub mod binderfs;
pub mod error;
pub mod file_descriptor;
mod macros;
pub mod native;
pub mod parcel;
pub mod parcelable;
pub mod parcelable_holder;
mod process_state;
pub mod proxy;
mod ref_counter;
pub mod status;
mod sys;
pub mod thread_state;

pub mod hub;
#[cfg(feature = "async")]
mod rt;

pub use binder::*;
#[cfg(feature = "async")]
pub use binder_async::{BinderAsyncPool, BinderAsyncRuntime, BoxFuture};
pub use error::{Result, StatusCode};
pub use file_descriptor::ParcelFileDescriptor;
pub use native::*;
pub use parcel::Parcel;
pub use parcelable::*;
pub use parcelable_holder::ParcelableHolder;
pub use process_state::ProcessState;
pub use proxy::*;
#[cfg(feature = "tokio")]
pub use rt::*;
pub use status::{ExceptionCode, Status};

pub const DEFAULT_BINDER_CONTROL_PATH: &str = "/dev/binderfs/binder-control";
pub const DEFAULT_BINDER_PATH: &str = "/dev/binderfs/binder";
pub const DEFAULT_BINDERFS_PATH: &str = "/dev/binderfs";

#[cfg(target_os = "android")]
static ANDROID_SDK_VERSION: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// Get the Android version.
#[cfg(target_os = "android")]
pub fn get_android_sdk_version() -> u32 {
    *ANDROID_SDK_VERSION.get_or_init(|| rsproperties::get_or("ro.build.version.sdk", 0))
}

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
