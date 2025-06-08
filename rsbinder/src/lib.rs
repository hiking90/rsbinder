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
static ANDROID_SDK_VERSION: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// Get the Android version.
#[cfg(target_os = "android")]
pub fn get_android_sdk_version() -> u32 {
    *ANDROID_SDK_VERSION.get_or_init(|| {
        match rsproperties::get_with_result("ro.build.version.sdk") {
            Ok(version) => {
                version.parse::<u32>().unwrap_or(0)
            }
            Err(_) => {
                log::warn!("Failed to get Android SDK version, defaulting to 0");
                0
            }
        }
    })
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
