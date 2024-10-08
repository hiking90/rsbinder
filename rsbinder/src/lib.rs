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
mod ref_counter;
#[cfg(feature = "async")]
pub mod binder_async;

pub mod hub;
#[cfg(feature = "async")]
mod rt;

pub use process_state::ProcessState;
pub use parcel::Parcel;
pub use status::{ExceptionCode, Status};
pub use error::{Result, StatusCode};
pub use binder::*;
pub use proxy::*;
pub use native::*;
pub use parcelable::*;
pub use file_descriptor::ParcelFileDescriptor;
pub use parcelable_holder::ParcelableHolder;
#[cfg(feature = "async")]
pub use binder_async::{BinderAsyncPool, BinderAsyncRuntime, BoxFuture};
#[cfg(feature = "tokio")]
pub use rt::*;

pub const DEFAULT_BINDER_CONTROL_PATH: &str = "/dev/binderfs/binder-control";
pub const DEFAULT_BINDER_PATH: &str = "/dev/binderfs/binder";
pub const DEFAULT_BINDERFS_PATH: &str = "/dev/binderfs";


#[cfg(target_os = "android")]
static ANDROID_VERSION: std::sync::OnceLock<i32> = std::sync::OnceLock::new();

/// Set the Android version for compatibility.
/// There are compatibility issues with Binder IPC depending on the Android version.
/// The current findings indicate there are compatibility issues before and after Android 12.
/// If your software needs to work on both Android 11 and 12,
/// you must set the Android version using the rsbinder::set_android_version() API.
#[cfg(target_os = "android")]
pub fn set_android_version(version: i32) {
    ANDROID_VERSION.set(version).expect("Android version is already set.");
}

/// Get the Android version.
#[cfg(target_os = "android")]
pub fn get_android_version() -> i32 {
    *ANDROID_VERSION.get().unwrap_or(&0)
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
