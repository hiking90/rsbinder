// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A pure Rust implementation of Android Binder IPC mechanism.
//!
//! This library provides a complete implementation of the Android Binder protocol
//! for inter-process communication (IPC) on Linux and Android systems. It enables
//! services to communicate across process boundaries with type safety and efficiency.
//!
//! # Core Components
//!
//! - **Binder**: Core binder object and transaction handling
//! - **Parcel**: Serialization/deserialization for IPC data
//! - **Proxy**: Client-side interface for remote services
//! - **Native**: Server-side service implementation utilities
//! - **ProcessState**: Process-level binder state management
//! - **ServiceManager**: Service discovery and registration
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

// Core binder functionality
mod binder;
/// Async binder runtime support
#[cfg(feature = "async")]
pub mod binder_async;
mod binder_object;
/// BinderFS filesystem utilities
pub mod binderfs;
/// Error types and result handling
pub mod error;
/// File descriptor wrapper for IPC
pub mod file_descriptor;
mod macros;
/// Native service implementation helpers
pub mod native;
/// Data serialization for IPC
pub mod parcel;
/// Parcelable trait for serializable types
pub mod parcelable;
/// Holder for parcelable objects
pub mod parcelable_holder;
mod process_state;
/// Client proxy for remote services
pub mod proxy;
mod ref_counter;
/// Status and exception handling
pub mod status;
mod sys;
/// Thread-local binder state
pub mod thread_state;

/// Service hub and manager implementations
pub mod hub;
/// Async runtime implementations
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

/// Default path to the binder control device
pub const DEFAULT_BINDER_CONTROL_PATH: &str = "/dev/binderfs/binder-control";
/// Default path to the binder device
pub const DEFAULT_BINDER_PATH: &str = "/dev/binderfs/binder";
/// Default path to the binderfs mount point
pub const DEFAULT_BINDERFS_PATH: &str = "/dev/binderfs";

#[cfg(target_os = "android")]
static ANDROID_SDK_VERSION: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// Get the Android SDK version from system properties.
///
/// Returns the Android SDK version number, or 0 if not available.
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
