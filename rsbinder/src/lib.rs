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
//! - **ServiceManager**: Service discovery and registration (the [`hub`] module)
//! - **RPC transport**: binder-over-socket (`rpc` module, behind the `rpc`
//!   feature) — a separate stack from the kernel binder path
//! - **Entry API**: [`serve`] / [`connect`] — publish and look up services
//!   with one URI-selected transport (kernel binder or RPC)
//!
//! # Feature flags
//!
//! - `tokio` *(default)* — full async/await support on the Tokio runtime
//!   (implies `async`).
//! - `async` — generic async-trait support, without committing to a
//!   specific runtime.
//! - `rpc` — master switch for the RPC transport (binder-over-socket), a
//!   separate stack from the kernel binder path. Off by default; default
//!   and `--no-default-features` builds carry zero RPC/networking cost.
//! - `rpc-tcp-debug` — plaintext TCP backend (implies `rpc`). Debug and
//!   interop bring-up **only**, never production — use `rpc-tls` for real
//!   networks.
//! - `rpc-vsock` — vsock backend for host↔VM channels (implies `rpc`;
//!   Linux and Android targets only).
//! - `rpc-tls` — TLS backend over rustls (implies `rpc`). rsbinder never
//!   invents crypto; the caller supplies the `rustls` configuration.
//! - `android_10` … `android_16`, plus the `android_*_plus` ranges (e.g.
//!   `android_11_plus`) — select which Android service-manager protocol
//!   versions to support. Android 10 uses the legacy C service-manager
//!   protocol. Android 15 and 17 need no dedicated flag: their
//!   service-manager wire format is identical to Android 14 and
//!   Android 16 respectively, so they are served by `android_14` /
//!   `android_16`.
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
//! rsbinder::include_aidl!("hello", crate::hello::IHello::*);
//!
//! // Implement the service
//! struct HelloService;
//!
//! impl Interface for HelloService {}
//!
//! impl IHello for HelloService {
//!     fn echo(&self, message: &str) -> rsbinder::BinderResult<String> {
//!         Ok(format!("Echo: {}", message))
//!     }
//! }
//!
//! # fn main() -> Result<()> {
//! // Kernel binder: init the process state, register with the service
//! // manager, start the thread pool and join it. The same three calls
//! // with `serve("unix:///tmp/hello.sock")` serve over RPC instead.
//! rsbinder::serve("binder://")?
//!     .add("hello_service", BnHello::new_binder(HelloService))?
//!     .run()?;
//! # Ok(())
//! # }
//! ```
//!
//! (`ProcessState::init_default()` + `start_thread_pool()` +
//! `hub::add_service()` + `join_thread_pool()` remain available as the
//! low-level form.)
//!
//! ## Creating a Client
//!
//! ```rust,ignore
//! use rsbinder::*;
//!
//! // Include the same generated code
//! rsbinder::include_aidl!("hello", crate::hello::IHello::*);
//!
//! # fn main() -> Result<()> {
//! // Init the process state, wait for the service to be registered, and
//! // cast it to the interface. Over RPC: "unix:///tmp/hello.sock#hello_service".
//! let hello_service: rsbinder::Strong<dyn IHello> =
//!     rsbinder::connect("binder://hello_service")?;
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

// android-only: clippy false-positives here on thread_locals that already use,
// or cannot use, a `const { .. }` initializer.
#![cfg_attr(target_os = "android", allow(clippy::missing_const_for_thread_local))]

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
/// `LazyServiceRegistrar` skeleton (state machine +
/// `IClientCallback::onClients` dispatch). AOSP
/// `frameworks/native/libs/binder/LazyServiceRegistrar.cpp`. Hub
/// integration (actual `registerClientCallback` / `tryUnregisterService`
/// IPC) is owed by the caller until the hub plumbing lands — see the
/// module docs.
pub mod lazy_service;
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
/// Process-global proxy count + watermark callbacks
/// with opt-in per-uid tracking. AOSP `BpBinder` proxy-count surface
/// (`getBinderProxyCount` / `setBinderProxyCountWatermarks` /
/// `setBinderProxyCountEventCallback` / `enableCountByUid`).
pub mod proxy_count;
mod ref_counter;
/// Shared-memory IPC: AOSP `IMemoryHeap` / `IMemory` traits, a
/// `memfd_create` (Linux/Android) / `shm_open` (macOS) backed
/// `MemoryHeapBase`, and wire-faithful `android.utils.IMemory*`
/// stubs so a heap fd can travel over the kernel binder or
/// Unix-socket RPC. See the module docs.
pub mod shared_memory;
/// Status and exception handling
pub mod status;
mod sys;
/// Thread-local binder state
pub mod thread_state;

/// RPC transport (binder-over-socket) — a separate stack from the
/// kernel binder path. Present only with the `rpc` feature.
#[cfg(feature = "rpc")]
pub mod rpc;

/// Service hub and manager implementations
pub mod hub;

/// Client stub for Android's `PermissionManagerService`
/// (`android.os.IPermissionController`). See module doc for the
/// AOSP-faithful surface and fail-closed `check_permission` helper.
pub mod permission_controller;

/// Async runtime implementations
#[cfg(feature = "async")]
mod rt;
// Unified entry API (Plan 2-17): `serve` / `connect` + URI over every
// transport. Kept as a plain (non-doc) comment: an outer doc here would
// merge with the module's inner `//!` docs and re-resolve their intra-doc
// links at the crate root, breaking them.
pub mod entry;

#[cfg(feature = "tokio")]
pub use entry::connect_async;
pub use entry::{
    connect, connect_binder, serve, Client, ClientOptions, Endpoint, ServeOptions, Server,
    ServerGuard,
};
// The `.aidl`-free path (plan 2-19), behind the `macros` feature: declare an
// interface as a Rust trait and its data types as ordinary structs and enums.
// See the `rsbinder_macros` crate docs for the signature rules and for what
// still needs `.aidl`. `Parcelable` is re-exported as both a trait and a
// derive; they live in different namespaces, so the one name serves both.
#[cfg(feature = "macros")]
pub use rsbinder_macros::{interface, BinderEnum, Parcelable};

// Explicit re-exports: glob re-exports would silently leak every
// newly-added `pub` item in these modules, defeating semver review.
// Items kept out of the crate root must be reached via
// `rsbinder::<module>::<item>`.

// From `binder` — core binder identity, transaction codes, traits.
pub use binder::{
    DeathRecipient, FromIBinder, IBinder, Interface, Remotable, RemoteProxy, SIBinder, Stability,
    Strong, ToAsyncInterface, ToSyncInterface, Transactable, TransactionCode, TransactionFlags,
    WIBinder, Weak, DEBUG_PID_TRANSACTION, DUMP_TRANSACTION, EXTENSION_TRANSACTION,
    FIRST_CALL_TRANSACTION, FLAG_CLEAR_BUF, FLAG_COLLECT_NOTED_APP_OPS, FLAG_ONEWAY,
    FLAG_PRIVATE_LOCAL, FLAG_PRIVATE_VENDOR, FLAG_UPDATE_TXN, INTERFACE_HEADER,
    INTERFACE_TRANSACTION, LAST_CALL_TRANSACTION, LIKE_TRANSACTION, PING_TRANSACTION,
    SET_RPC_CLIENT_TRANSACTION, SHELL_COMMAND_TRANSACTION, START_RECORDING_TRANSACTION,
    STOP_RECORDING_TRANSACTION, SYSPROPS_TRANSACTION, TWEET_TRANSACTION,
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

// From `thread_state` — calling identity (AOSP
// `IPCThreadState::getCalling*`). Kernel-path only; the RPC stack has
// its own `PeerIdentity` model under `rpc::PeerIdentity`.
pub use thread_state::{
    calling_caller, clear_calling_identity, get_calling_pid, get_calling_sid, get_calling_uid,
    get_current_scheduler_policy, get_extended_error, get_strict_mode_policy,
    has_explicit_identity, restore_calling_identity, set_strict_mode_policy, Caller,
    CallingContext, ExtendedError,
};

pub use parcel::Parcel;

// From `parcelable` — (de)serialization trait stack.
pub use parcelable::{
    Deserialize, DeserializeArray, DeserializeOption, Parcelable, ParcelableMetadata, Serialize,
    SerializeArray, SerializeOption, NON_NULL_PARCELABLE_FLAG, NULL_PARCELABLE_FLAG,
};

pub use parcelable_holder::ParcelableHolder;
pub use process_state::{CallRestriction, ProcessState};

// From `proxy` — client-side handle types.
pub use proxy::{Proxy, ProxyHandle};

// Explicit (not glob) so a newly-added `pub` item in `rt` can't silently leak
// to the crate root without semver review — the policy stated above.
#[cfg(feature = "tokio")]
pub use rt::{get_interface, Tokio, TokioRuntime};
pub use status::{BinderResult, ExceptionCode, Status};

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
