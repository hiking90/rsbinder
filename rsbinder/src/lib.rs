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
//! # Basic Usage
//!
//! This library works with AIDL (Android Interface Definition Language) files to generate
//! type-safe Rust bindings for IPC services.
//!
//! ## Setting up an AIDL-based Service
//!
//! First, create an AIDL interface file (`aidl/hello/IHello.aidl`):
//!
//! ```aidl
//! package hello;
//!
//! interface IHello {
//!     String echo(in String message);
//! }
//! ```
//!
//! Add a `build.rs` file to generate Rust bindings:
//!
//! ```rust,ignore
//! # use std::path::PathBuf;
//! rsbinder_aidl::Builder::new()
//!     .source(PathBuf::from("aidl/hello/IHello.aidl"))
//!     .output(PathBuf::from("hello.rs"))
//!     .generate()
//!     .unwrap();
//! ```
//!
//! In your `Cargo.toml`, add the build dependency:
//!
//! ```toml
//! [build-dependencies]
//! rsbinder-aidl = "*"  # Check crates.io for the latest version
//! ```
//!
//! ## Implementing the Service
//!
//! ```rust,ignore
//! use rsbinder::*;
//!
//! // Include the generated code
//! include!(concat!(env!("OUT_DIR"), "/hello.rs"));
//! pub use crate::hello::IHello::*;
//!
//! // Implement the service
//! struct HelloService;
//!
//! impl Interface for HelloService {}
//!
//! impl IHello for HelloService {
//!     fn echo(&self, message: &str) -> rsbinder::status::Result<String> {
//!         Ok(format!("Echo: {}", message))
//!     }
//! }
//!
//! # fn main() -> Result<()> {
//! // Initialize the process state
//! ProcessState::init_default()?;
//!
//! // Start the thread pool
//! ProcessState::start_thread_pool();
//!
//! // Register your service
//! let service = BnHello::new_binder(HelloService);
//! hub::add_service("hello_service", service.as_binder())?;
//!
//! println!("Hello service started");
//!
//! // Join the thread pool to handle requests
//! ProcessState::join_thread_pool();
//! # Ok(())
//! # }
//! ```
//!
//! ## Creating a Client
//!
//! ```rust,ignore
//! use rsbinder::*;
//!
//! // Include the same generated code
//! include!(concat!(env!("OUT_DIR"), "/hello.rs"));
//! pub use crate::hello::IHello::*;
//!
//! # fn main() -> Result<()> {
//! // Initialize the process state
//! ProcessState::init_default()?;
//!
//! // Get service from service manager
//! let service = hub::get_service("hello_service")?;
//! let hello_service = BpHello::new(service)?;
//!
//! // Call remote method
//! let result = hello_service.echo("Hello, World!")?;
//! println!("Service response: {}", result);
//! # Ok(())
//! # }
//! ```
//!
//! ## Opting into kernel-level features
//!
//! Native binders can opt into per-binder kernel features (for example,
//! receiving the caller's SELinux security context) via [`BinderFeatures`]
//! and the `*_with_features` constructor variants. See [`BinderFeatures`]
//! for the available opt-ins.
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

/// RPC transport (binder-over-socket) — a separate stack from the
/// kernel binder path. Present only with the `rpc` feature. See
/// `plan/2-rpc-transport.md`.
#[cfg(feature = "rpc")]
pub mod rpc;

/// Service hub and manager implementations
pub mod hub;
/// Async runtime implementations
#[cfg(feature = "async")]
mod rt;

// Explicit re-exports: glob re-exports would silently leak every
// newly-added `pub` item in these modules, defeating semver review.
// Items kept out of the crate root must be reached via
// `rsbinder::<module>::<item>`.

// From `binder` — core binder identity, transaction codes, traits.
pub use binder::{
    DeathRecipient, FromIBinder, IBinder, Interface, Remotable, RemoteProxy, SIBinder, Stability,
    Strong, ToAsyncInterface, ToSyncInterface, Transactable, TransactionCode, TransactionFlags,
    WIBinder, Weak, DEBUG_PID_TRANSACTION, DUMP_TRANSACTION, EXTENSION_TRANSACTION,
    FIRST_CALL_TRANSACTION, FLAG_CLEAR_BUF, FLAG_ONEWAY, FLAG_PRIVATE_LOCAL, FLAG_PRIVATE_VENDOR,
    INTERFACE_HEADER, INTERFACE_TRANSACTION, LAST_CALL_TRANSACTION, LIKE_TRANSACTION,
    PING_TRANSACTION, SET_RPC_CLIENT_TRANSACTION, SHELL_COMMAND_TRANSACTION,
    START_RECORDING_TRANSACTION, STOP_RECORDING_TRANSACTION, SYSPROPS_TRANSACTION,
    TWEET_TRANSACTION,
};
// `declare_binder_interface!` expands to `$crate::__rpc_stamp_descriptor(...)`
// in consumer crates, so this helper must stay reachable at the crate root.
#[doc(hidden)]
pub use binder::__rpc_stamp_descriptor;

#[cfg(feature = "async")]
pub use binder_async::{BinderAsyncPool, BinderAsyncRuntime, BoxFuture};
pub use error::{Result, StatusCode};
pub use file_descriptor::ParcelFileDescriptor;

// From `native` — server-side binder construction.
pub use native::{is_handling_transaction, Binder, BinderFeatures};

pub use parcel::Parcel;

// From `parcelable` — (de)serialization trait stack.
pub use parcelable::{
    Deserialize, DeserializeArray, DeserializeOption, Parcelable, ParcelableMetadata, Serialize,
    SerializeArray, SerializeOption, NON_NULL_PARCELABLE_FLAG, NULL_PARCELABLE_FLAG,
};

pub use parcelable_holder::ParcelableHolder;
pub use process_state::ProcessState;

// From `proxy` — client-side handle types.
pub use proxy::{Proxy, ProxyHandle};

#[cfg(feature = "tokio")]
pub use rt::*;
pub use status::{ExceptionCode, Status};

/// Default path to the binder control device
pub const DEFAULT_BINDER_CONTROL_PATH: &str = "/dev/binderfs/binder-control";
/// Default path to the binder device (binderfs, used on Android 11+).
pub const DEFAULT_BINDER_PATH: &str = "/dev/binderfs/binder";
/// Default path to the binderfs mount point
pub const DEFAULT_BINDERFS_PATH: &str = "/dev/binderfs";
/// Legacy binder device path used on Android 10 and earlier.
pub const LEGACY_BINDER_PATH: &str = "/dev/binder";

/// Returns `true` when the runtime Android SDK version is at least `version`.
/// On non-Android platforms this always returns `true`.
#[cfg(target_os = "android")]
#[inline]
pub fn sdk_at_least(version: u32) -> bool {
    get_android_sdk_version() >= version
}

#[cfg(not(target_os = "android"))]
#[inline]
pub fn sdk_at_least(_version: u32) -> bool {
    true
}

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
    #[serial_test::serial(binder)]
    fn process_state() {
        ProcessState::init("/dev/binderfs/binder", 0).expect("init");
    }
}
