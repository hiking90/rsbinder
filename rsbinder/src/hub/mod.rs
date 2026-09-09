// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! # Service Hub and Manager Implementations
//!
//! This module provides a unified interface to interact with Android's Service Manager
//! across different Android API versions. It abstracts version differences and provides
//! both common functionality and version-specific access when needed.
//!
//! ## Version Compatibility
//!
//! Android's Service Manager interface has evolved across different Android versions.
//! This hub module abstracts these differences and provides a consistent API
//! for the most common operations needed by applications.
//!
//! The hub exposes common functionality available across all supported Android versions.
//! For version-specific features, use the specific version modules directly
//! (e.g., `android_16`, `android_14`, etc.).
//!
//! Version and protocol are not the same thing here. Android 17 reuses
//! Android 16's interface, and Android 15 has two of its own —
//! `android-15.0.0_r6` inserted a method in the middle and shifted every
//! transaction code after it while keeping SDK 35. `hub::default` measures
//! which one an Android 15 device speaks; see the `android_15` module for
//! the split.
//!
//! ## Usage
//!
//! ### Common API (Version-Agnostic)
//!
//! ```rust,no_run
//! use rsbinder::hub;
//!
//! // Client startup: block until the service is registered, then use it.
//! let service = hub::wait_for_service("example_service");
//!
//! // Non-blocking probe: returns immediately, `None` if not registered yet.
//! let maybe = hub::check_service("example_service");
//!
//! // List all registered services.
//! let services = hub::list_services(hub::DUMP_FLAG_PRIORITY_ALL);
//! ```
//!
//! The typed variants (`*_interface`) cast straight to a `Strong<dyn IFoo>`:
//!
//! ```ignore
//! let foo: Strong<dyn IFoo> = hub::wait_for_interface("example_service")?;
//! ```
//!
//! ### Choosing a lookup function
//!
//! The lookup family differs only in *how it waits* and *how it encodes
//! "not registered"*. Pick by the column you need:
//!
//! | Function | Returns | Not registered | SM unreachable | Cast mismatch |
//! |---|---|---|---|---|
//! | [`check_service`](crate::hub::check_service) | `Option<SIBinder>` | `None` | `None` | — |
//! | [`check_interface`](crate::hub::check_interface) | `Result<Strong<T>>` | `Err(NameNotFound)` | `Err(NameNotFound)` | `Err(BadType)` |
//! | [`try_get_service`](crate::hub::try_get_service) | `Result<Option<SIBinder>>` | `Ok(None)` | `Err(..)` | — |
//! | [`try_get_interface`](crate::hub::try_get_interface) | `Result<Option<Strong<T>>>` | `Ok(None)` | `Err(..)` | `Err(BadType)` |
//! | [`wait_for_service`](crate::hub::wait_for_service) | `Option<SIBinder>` | *blocks*; `None` on give-up | `None` | — |
//! | [`wait_for_interface`](crate::hub::wait_for_interface) | `Result<Strong<T>>` | *blocks*; `Err(NameNotFound)` on give-up | `Err(NameNotFound)` | `Err(BadType)` |
//!
//! Use `wait_*` for a dependency expected to appear (client startup),
//! `check_*` for an optional service probed once, and `try_*` when you must
//! tell "not registered" (`Ok(None)`) apart from "service manager
//! unreachable" (`Err`). One caveat on `check_*`: on Android 15 r6+
//! (`android_15`) it is carried by `getService`, which makes the service
//! manager try to start an unregistered lazy service — see the docs on
//! `hub::android_15::check_service` (an Android-only module, so not
//! linkable from a host build). On the Android 10 legacy C service manager, which
//! cannot distinguish not-found from a transport failure, the `try_*`
//! functions map any failure to `Ok(None)`.
//!
//! ### Version-Specific API
//!
//! If you need to use version-specific features:
//!
//! ```rust,no_run
//! use rsbinder::hub;
//!
//! // For Android 16 specific functionality
//! #[cfg(all(target_os = "android", feature = "android_16"))]
//! {
//!     if let hub::ServiceManager::Android16(sm) = &*hub::default().unwrap() {
//!         let _svc = hub::android_16::get_service(sm, "example_service");
//!         // Use Android 16 specific methods on `sm` here
//!     }
//! }
//! ```

use std::sync::{Arc, OnceLock};

/// The common body of every per-version `servicemanager_N` module
/// (Android 11 through 15). Each call expands to the same
/// `BpServiceManager` re-exports + dispatch wrappers; version-specific
/// additions (e.g. `get_service_debug_info` since 12) go in the
/// `$($extra:tt)*` repetition. Caller emits `include!(...)` for the
/// generated AIDL bindings *before* invoking this macro so that the
/// `android::os::*` paths below resolve in the caller's scope.
///
/// The plain form also emits `check_service` over the `checkService` wire
/// call. `@custom_check_service` omits it, for a version where that method
/// does not return an `@nullable IBinder` — Android 15's returns a `Service`
/// union, so `servicemanager_15` supplies its own (see the module docs there
/// for why it does not parse the union).
#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
        feature = "android_15",
    )
))]
macro_rules! impl_sm_module_body {
    (@custom_check_service $($extra:tt)*) => {
        use crate::*;
        pub use android::os::IServiceManager::{
            BnServiceManager, BpServiceManager, IServiceManager,
            DUMP_FLAG_PRIORITY_ALL, DUMP_FLAG_PRIORITY_CRITICAL,
            DUMP_FLAG_PRIORITY_DEFAULT, DUMP_FLAG_PRIORITY_HIGH,
            DUMP_FLAG_PRIORITY_NORMAL, DUMP_FLAG_PROTO,
        };
        pub use android::os::IServiceCallback::{BnServiceCallback, IServiceCallback};
        pub use android::os::IClientCallback::{BnClientCallback, IClientCallback};

        /// Retrieve an existing service via a single `getService` wire call
        /// (one attempt; not blocking). Use `wait_for_service` to block until
        /// the service appears, or `check_service` for an explicit
        /// non-blocking lookup.
        pub fn get_service(sm: &BpServiceManager, name: &str) -> Option<SIBinder> {
            match sm.getService(name) {
                Ok(result) => result,
                Err(err) => {
                    log::error!("Failed to get service {}: {:?}", name, err);
                    None
                }
            }
        }

        /// Like `get_service` but preserves a transport error instead of
        /// collapsing it to `None`, so a waiter can tell "not yet registered"
        /// (`Ok(None)`) from "service manager unreachable" (`Err`) — the
        /// distinction AOSP `realGetService` carries in its `Status`.
        pub fn try_get_service(sm: &BpServiceManager, name: &str) -> Result<Option<SIBinder>> {
            sm.getService(name).map_err(|e| e.into())
        }

        /// Return a list of all currently running services.
        pub fn list_services(sm: &BpServiceManager, dump_priority: i32) -> Vec<String> {
            match sm.listServices(dump_priority) {
                Ok(result) => result,
                Err(err) => {
                    log::error!("Failed to list services: {}", err);
                    Vec::new()
                }
            }
        }

        pub fn add_service(
            sm: &BpServiceManager,
            identifier: &str,
            binder: SIBinder,
        ) -> std::result::Result<(), Status> {
            sm.addService(identifier, &binder, false, DUMP_FLAG_PRIORITY_DEFAULT)
        }

        /// Request a callback when a service is registered.
        pub fn register_for_notifications(
            sm: &BpServiceManager,
            name: &str,
            callback: &crate::Strong<dyn IServiceCallback>,
        ) -> Result<()> {
            sm.registerForNotifications(name, callback).map_err(|e| e.into())
        }

        /// Unregisters all requests for notifications for a specific callback.
        pub fn unregister_for_notifications(
            sm: &BpServiceManager,
            name: &str,
            callback: &crate::Strong<dyn IServiceCallback>,
        ) -> Result<()> {
            sm.unregisterForNotifications(name, callback).map_err(|e| e.into())
        }

        /// Register a callback for client (proxy) presence transitions on a
        /// lazy service. AOSP `IServiceManager::registerClientCallback`.
        pub fn register_client_callback(
            sm: &BpServiceManager,
            name: &str,
            service: &SIBinder,
            callback: &crate::Strong<dyn IClientCallback>,
        ) -> Result<()> {
            sm.registerClientCallback(name, service, callback).map_err(|e| e.into())
        }

        /// Attempt to unregister a service previously registered with
        /// `add_service`. AOSP `IServiceManager::tryUnregisterService`.
        pub fn try_unregister_service(
            sm: &BpServiceManager,
            name: &str,
            service: &SIBinder,
        ) -> Result<()> {
            sm.tryUnregisterService(name, service).map_err(|e| e.into())
        }

        /// Returns whether a given interface is declared on the device,
        /// even if it is not started yet. For instance, this could be a
        /// service declared in the VINTF manifest.
        pub fn is_declared(sm: &BpServiceManager, name: &str) -> bool {
            match sm.isDeclared(name) {
                Ok(result) => result,
                Err(err) => {
                    log::error!("Failed to is_declared({}): {}", name, err);
                    false
                }
            }
        }

        pub fn get_interface<T: FromIBinder + ?Sized>(
            sm: &BpServiceManager,
            name: &str,
        ) -> Result<Strong<T>> {
            match sm.getService(name) {
                Ok(Some(service)) => FromIBinder::try_from(service),
                Ok(None) => {
                    log::error!("Service {} not found", name);
                    Err(StatusCode::NameNotFound)
                }
                Err(err) => {
                    log::error!("Failed to get interface {}: {}", name, err);
                    Err(StatusCode::NameNotFound)
                }
            }
        }

        $($extra)*
    };
    ($($extra:tt)*) => {
        $crate::hub::impl_sm_module_body! { @custom_check_service
            /// Retrieve an existing service called @a name from the service
            /// manager. Non-blocking. Returns null if the service does not
            /// exist.
            pub fn check_service(sm: &BpServiceManager, name: &str) -> Option<SIBinder> {
                match sm.checkService(name) {
                    Ok(result) => result,
                    Err(err) => {
                        log::error!("Failed to check service {}: {}", name, err);
                        None
                    }
                }
            }

            $($extra)*
        }
    };
}
#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
        feature = "android_15",
    )
))]
pub(crate) use impl_sm_module_body;

#[cfg(all(target_os = "android", feature = "android_10"))]
mod servicemanager_10;
#[cfg(all(target_os = "android", feature = "android_10"))]
pub mod android_10 {
    pub use super::servicemanager_10::*;
}

#[cfg(all(target_os = "android", feature = "android_11"))]
mod servicemanager_11;
#[cfg(all(target_os = "android", feature = "android_11"))]
pub mod android_11 {
    pub use super::servicemanager_11::*;
}

#[cfg(all(target_os = "android", feature = "android_12"))]
mod servicemanager_12;
#[cfg(all(target_os = "android", feature = "android_12"))]
pub mod android_12 {
    pub use super::servicemanager_12::*;
}

#[cfg(all(target_os = "android", feature = "android_13"))]
mod servicemanager_13;
#[cfg(all(target_os = "android", feature = "android_13"))]
pub mod android_13 {
    pub use super::servicemanager_13::*;
}

#[cfg(all(target_os = "android", feature = "android_14"))]
mod servicemanager_14;
#[cfg(all(target_os = "android", feature = "android_14"))]
pub mod android_14 {
    pub use super::servicemanager_14::*;
}

#[cfg(test)]
mod numbering_pins;
#[cfg(all(target_os = "android", feature = "android_15"))]
mod servicemanager_15;
/// The Android 15 service-manager protocol **from `android-15.0.0_r6` on**.
///
/// # Two protocols, one SDK version
///
/// `android-15.0.0_r6` inserted `getService2` at index 1 of
/// `IServiceManager.aidl` and shifted every transaction code after it by one:
/// `checkService` 1 → 2, `addService` 2 → 3, `registerClientCallback`
/// 11 → 12, `tryUnregisterService` 12 → 13, `getServiceDebugInfo` 13 → 14.
/// The SDK version stayed 35, and the interface is not a frozen
/// `aidl_interface`, so nothing recorded the change. AOSP is unaffected —
/// `servicemanager` and `libbinder` ship in one image and move together —
/// but an out-of-tree client pins the numbers and has to be told which build
/// it is talking to.
///
/// This module is the second numbering; `android_14` is the first, which
/// serves `android-15.0.0_r1` through `r5`. **The feature name says 15
/// because that is the platform version, not because it covers all of it**;
/// enable `android_14` and `android_15` together to reach every Android 15
/// device. Neither can be chosen from the SDK version, so
/// [`default`] measures it: one argument-free transaction to
/// code 14, which is `getServiceDebugInfo()` here and past the end of the
/// interface there.
///
/// The numbering has not moved again through `android-15.0.0_r36`; only the
/// `Service` union's payload has (see below).
///
/// With only one of the two features compiled in, [`default`] refuses the
/// other numbering rather than addressing it: the wrong module's codes land
/// on real methods (`addService` on `checkService`, and so on — measured
/// against a real QPR2 `servicemanager`), so its calls fail one at a time as
/// `BAD_PARCELABLE` errors that say nothing about the cause. The refusal
/// names the missing feature instead. Only kernel-binder use through `hub`
/// is affected — the RPC transport does not go through the service manager.
///
/// # Why nothing here parses the `Service` union
///
/// `getService2` and `checkService` return `Service`, and that union is
/// *not* stable across the Android 15 release trains: `r6` declares
/// `{@nullable IBinder binder, @nullable IBinder accessor}`, while `r20`
/// through `r36` declare `{ServiceWithMetadata serviceWithMetadata,
/// @nullable IBinder accessor}`. The two shapes deserialize differently, and
/// nothing on the wire says which one a device sends.
///
/// So this module resolves services through `getService` (code 0) alone —
/// including `check_service`, which forwards to it. `getService` returns a
/// plain `@nullable IBinder` on every release in the range, and AOSP's own
/// AIDL comment records that it "is the same as checkService (returns
/// immediately) but exists for legacy purposes". Not parsing the union is
/// the design of this module, not an omission: it is what makes one module
/// cover `r6` through `r36`.
///
/// `getService2`/`checkService` are still generated — the transaction codes
/// depend on their being declared — and the vendored AIDL is
/// `android-15.0.0_r20` verbatim, so calling them directly will mis-parse on
/// an `r6`-era device. Use the functions in this module.
///
/// # Not supported
///
/// The union's `accessor` arm, and with it VINTF `<accessor>` resolution
/// over RPC. rsbinder's accessor bridge is Android 16 only
/// ([`android_16`]); reaching it from here would mean parsing the union.
#[cfg(all(target_os = "android", feature = "android_15"))]
pub mod android_15 {
    pub use super::servicemanager_15::*;
}

#[cfg(feature = "rpc")]
pub(crate) mod accessor_16;
/// Register-side companion to [`accessor_16`]. Defines
/// `AccessorSockAddr` + `AccessorAddrProvider`, `LocalAccessor`,
/// the `add_accessor_provider` / `create_accessor` /
/// `remove_accessor_provider` process-local registry, and the
/// `resolve_via_process_local` fallback helper. Same
/// `cfg(feature = "rpc")` gate as the consume side.
#[cfg(feature = "rpc")]
pub(crate) mod accessor_register;
mod servicemanager_16;
pub mod android_16 {
    /// Expose the deterministic error-name decoder
    /// (and its `__fuzz_*` hook) so the libFuzzer target can drive it
    /// without re-implementing the i32→symbol map.
    #[cfg(all(feature = "rpc", feature = "fuzzing"))]
    pub use super::accessor_16::__fuzz_accessor_error_decode;
    #[cfg(feature = "rpc")]
    pub use super::accessor_16::{
        accessor_error_name, resolve_accessor, BnAccessor, BpAccessor, IAccessor, IAccessorDefault,
        IAccessorDefaultRef, ERROR_CONNECTION_INFO_NOT_FOUND, ERROR_FAILED_TO_CONNECT_EACCES,
        ERROR_FAILED_TO_CONNECT_TO_SOCKET, ERROR_FAILED_TO_CREATE_SOCKET,
        ERROR_UNSUPPORTED_SOCKET_FAMILY,
    };
    // Async-trait re-export gated on the runtime `async` feature —
    // mirrors the codegen gate in `accessor_16::pub use ...`.
    #[cfg(all(feature = "rpc", feature = "async"))]
    pub use super::accessor_16::IAccessorAsyncService;
    /// Register-side public surface.
    #[cfg(feature = "rpc")]
    pub use super::accessor_register::{
        add_accessor_provider, create_accessor, remove_accessor_provider,
        resolve_via_process_local, AccessorAddrProvider, AccessorConnectError, AccessorProviderFn,
        AccessorProviderHandle, AccessorSockAddr, LocalAccessor,
    };
    pub use super::servicemanager_16::*;
}

use crate::*;

// Export Android 16 types as the default public API
pub use android_16::{
    BnClientCallback, BnServiceCallback, ConnectionInfo, IClientCallback, IServiceCallback,
    ServiceDebugInfo, DUMP_FLAG_PRIORITY_ALL, DUMP_FLAG_PRIORITY_CRITICAL,
    DUMP_FLAG_PRIORITY_DEFAULT, DUMP_FLAG_PRIORITY_HIGH, DUMP_FLAG_PRIORITY_NORMAL,
    DUMP_FLAG_PROTO,
};

/// Android SDK version constants
#[cfg(target_os = "android")]
pub mod sdk_versions {
    /// Android 17 (API level 37)
    pub const ANDROID_17: u32 = 37;
    /// Android 16 (API level 36)
    pub const ANDROID_16: u32 = 36;
    /// Android 15 (API level 35)
    pub const ANDROID_15: u32 = 35;
    /// Android 14 (API level 34)
    pub const ANDROID_14: u32 = 34;
    /// Android 13 (API level 33)
    pub const ANDROID_13: u32 = 33;
    /// Android 12L (API level 32)
    pub const ANDROID_12L: u32 = 32;
    /// Android 12 (API level 31)
    pub const ANDROID_12: u32 = 31;
    /// Android 11 (API level 30)
    pub const ANDROID_11: u32 = 30;
    /// Android 10 (API level 29)
    pub const ANDROID_10: u32 = 29;

    /// Minimum supported Android SDK version
    pub const MIN_SUPPORTED: u32 = ANDROID_10;
    /// Maximum supported Android SDK version
    pub const MAX_SUPPORTED: u32 = ANDROID_17;
}

/// ServiceManager provides a unified interface to interact with Android's Service Manager
/// across different Android versions.
///
/// This enum internally dispatches calls to the appropriate version-specific implementation
/// based on the detected Android version or the explicitly specified version.
///
/// For version-specific features not covered by the common API, cast to the specific
/// version's ServiceManager implementation or use the version-specific modules directly.
pub enum ServiceManager {
    #[cfg(all(target_os = "android", feature = "android_10"))]
    Android10(android_10::BpServiceManager),
    #[cfg(all(target_os = "android", feature = "android_11"))]
    Android11(android_11::BpServiceManager),
    #[cfg(all(target_os = "android", feature = "android_12"))]
    Android12(android_12::BpServiceManager),
    #[cfg(all(target_os = "android", feature = "android_13"))]
    Android13(android_13::BpServiceManager),
    #[cfg(all(target_os = "android", feature = "android_14"))]
    Android14(android_14::BpServiceManager),
    /// Android 15 from `android-15.0.0_r6` on; earlier Android 15 builds are
    /// `Android14` (a different feature, so not linkable from here). Which
    /// one a device speaks is measured, not derived from the SDK version —
    /// see [`default`].
    #[cfg(all(target_os = "android", feature = "android_15"))]
    Android15(android_15::BpServiceManager),
    Android16(android_16::BpServiceManager),
}

/// Which of Android 15's two service-manager numberings this device speaks;
/// see [`android_15`] for the split.
#[cfg(all(
    target_os = "android",
    any(feature = "android_14", feature = "android_15")
))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Android15Numbering {
    /// `android-15.0.0_r1`–`r5`: the Android 14 interface unchanged.
    Original,
    /// `android-15.0.0_r6`+: every code from `checkService` on moved up one.
    Shifted,
}

/// Side-effect-free on both numberings: past the end pre-r6, `getServiceDebugInfo()` on r6+ (`numbering_pins` pins it).
#[allow(dead_code)] // only issued on android; pinned everywhere
pub(crate) const ANDROID_15_PROBE_CODE: TransactionCode = 14;

/// Tell the two Android 15 service-manager numberings apart with one
/// side-effect-free transaction ([`ANDROID_15_PROBE_CODE`]). See [`android_15`].
#[cfg(all(
    target_os = "android",
    any(feature = "android_14", feature = "android_15")
))]
fn check_android_15_numbering(context: &SIBinder) -> Result<Android15Numbering> {
    // The answer never changes for the process; a refused numbering would otherwise re-probe on every `default()`.
    static PROBED: OnceLock<Android15Numbering> = OnceLock::new();
    if let Some(&numbering) = PROBED.get() {
        return Ok(numbering);
    }

    #[cfg(feature = "android_15")]
    let descriptor = <android_15::BpServiceManager as android_15::IServiceManager>::descriptor();
    #[cfg(all(feature = "android_14", not(feature = "android_15")))]
    let descriptor = <android_14::BpServiceManager as android_14::IServiceManager>::descriptor();

    let proxy = context.as_proxy().ok_or(StatusCode::BadType)?;
    let mut data = Parcel::new();
    data.write_interface_token(descriptor)?;

    let numbering =
        match proxy.submit_transact(FIRST_CALL_TRANSACTION + ANDROID_15_PROBE_CODE, &data, 0) {
            // Rejected: 14 methods, so this is a pre-r6 build.
            Err(StatusCode::UnknownTransaction) => Android15Numbering::Original,
            // Answered (an in-band exception counts): 15 methods, the r6+ build.
            Ok(_) => Android15Numbering::Shifted,
            // Neither answer: refuse rather than guess (see `android_15`'s docs).
            Err(e) => {
                log::error!("could not probe the Android 15 service-manager protocol: {e:?}");
                return Err(e);
            }
        };
    Ok(*PROBED.get_or_init(|| numbering))
}

/// Refuse an Android 15 numbering this build has no module for, naming the
/// feature that would cover it; see [`android_15`] for why refusing beats
/// addressing it with the wrong codes.
#[cfg(all(
    target_os = "android",
    any(
        all(feature = "android_14", not(feature = "android_15")),
        all(feature = "android_15", not(feature = "android_14")),
    )
))]
fn android_15_feature_missing(numbering: Android15Numbering) -> StatusCode {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    let (build, feature) = match numbering {
        Android15Numbering::Original => ("android-15.0.0_r1 through r5", "android_14"),
        Android15Numbering::Shifted => ("android-15.0.0_r6 or later", "android_15"),
    };
    LOGGED.call_once(|| {
        log::error!(
            "this Android 15 device speaks the {build} service-manager protocol, \
             which needs the `{feature}` feature; rsbinder was built without it. \
             The two numberings differ from `checkService` on and cannot be told \
             apart by SDK version, so the service manager is refused rather than \
             addressed with the wrong transaction codes."
        );
    });
    StatusCode::InvalidOperation
}

/// Returns the global ServiceManager instance appropriate for the current Android version.
///
/// The singleton is created on first call and reused afterwards. The correct
/// version-specific implementation is selected from the detected Android SDK
/// version.
///
/// Returns an error instead of panicking when the context object cannot be
/// obtained, the proxy cannot be created, or the SDK version is unsupported.
/// A failed initialization is not cached, so a later call may retry.
///
/// # Panics
///
/// Panics if the kernel-binder [`ProcessState`] has not been initialized with
/// [`ProcessState::init`] or [`ProcessState::init_default`]. Reaching the
/// service manager without kernel binder is a programming error, not a
/// runtime condition, so it fails fast at the first call rather than
/// degrading into a plausible-looking "no such service" answer.
///
/// Every convenience wrapper in this module inherits that, including the ones
/// whose signature cannot report failure — [`is_declared`], [`check_service`],
/// [`wait_for_service`], [`list_services`] and [`get_declared_instances`]. Their
/// `unwrap_or`/`ok()?` covers the *other* failures listed above, not this one.
///
/// [`ProcessState::init`]: crate::ProcessState::init
/// [`ProcessState::init_default`]: crate::ProcessState::init_default
pub fn default() -> Result<Arc<ServiceManager>> {
    static GLOBAL_SM: OnceLock<Arc<ServiceManager>> = OnceLock::new();

    if let Some(sm) = GLOBAL_SM.get() {
        return Ok(sm.clone());
    }

    let process = ProcessState::as_self();
    let context = process.context_object()?;
    #[cfg(target_os = "android")]
    let sdk_version = crate::get_android_sdk_version();

    #[cfg(target_os = "android")]
    let service_manager = {
        macro_rules! create_service_manager {
            ($variant:ident, $module:ident) => {
                ServiceManager::$variant(
                    $module::BpServiceManager::from_binder(context).ok_or(StatusCode::BadType)?,
                )
            };
        }

        match sdk_version {
            // Android 17 (SDK 37) shares Android 16's service-manager wire
            // format — the `android/os/*` AIDL is byte-identical between
            // android-16.0.0_r4 and android-17.0.0_r1 (and the kernel binder
            // UAPI is unchanged), so it is served by the `android_16` module.
            sdk_versions::ANDROID_16 | sdk_versions::ANDROID_17 => {
                create_service_manager!(Android16, android_16)
            }
            #[cfg(feature = "android_14")]
            sdk_versions::ANDROID_14 => create_service_manager!(Android14, android_14),
            // Two numberings share SDK 35 — probe, never guess (see `android_15`).
            #[cfg(any(feature = "android_14", feature = "android_15"))]
            sdk_versions::ANDROID_15 => match check_android_15_numbering(&context)? {
                Android15Numbering::Original => {
                    #[cfg(feature = "android_14")]
                    {
                        create_service_manager!(Android14, android_14)
                    }
                    #[cfg(not(feature = "android_14"))]
                    {
                        return Err(android_15_feature_missing(Android15Numbering::Original));
                    }
                }
                Android15Numbering::Shifted => {
                    #[cfg(feature = "android_15")]
                    {
                        create_service_manager!(Android15, android_15)
                    }
                    #[cfg(not(feature = "android_15"))]
                    {
                        return Err(android_15_feature_missing(Android15Numbering::Shifted));
                    }
                }
            },
            #[cfg(not(any(feature = "android_14", feature = "android_15")))]
            sdk_versions::ANDROID_15 => {
                log::error!(
                    "Android 15 needs the `android_14` (android-15.0.0_r1 through r5) and/or \
                     `android_15` (r6 or later) feature; rsbinder was built with neither"
                );
                return Err(StatusCode::InvalidOperation);
            }
            #[cfg(feature = "android_13")]
            sdk_versions::ANDROID_13 => create_service_manager!(Android13, android_13),
            #[cfg(feature = "android_12")]
            sdk_versions::ANDROID_12 | sdk_versions::ANDROID_12L => {
                create_service_manager!(Android12, android_12)
            }
            #[cfg(feature = "android_11")]
            sdk_versions::ANDROID_11 => create_service_manager!(Android11, android_11),
            #[cfg(feature = "android_10")]
            sdk_versions::ANDROID_10 => create_service_manager!(Android10, android_10),
            _ => return Err(StatusCode::InvalidOperation),
        }
    };

    #[cfg(not(target_os = "android"))]
    let service_manager = ServiceManager::Android16(
        android_16::BpServiceManager::from_binder(context).ok_or(StatusCode::BadType)?,
    );

    // Cache only on success; a failed init returned above is not stored,
    // so a later call may retry. If two threads race here, get_or_init
    // keeps the first stored instance and the extra one is dropped.
    Ok(GLOBAL_SM.get_or_init(|| Arc::new(service_manager)).clone())
}

/// Forwards an existing `IServiceCallback` to a per-version
/// service-manager shim without reconstructing a typed `Strong`.
///
/// Each `android_N::IServiceCallback` is generated from its own AIDL unit,
/// so they are distinct trait types with independently-built vtables;
/// transmuting a `Strong<dyn _>` (a `Box<dyn _>` fat pointer) across them
/// would dispatch through a foreign vtable, a layout Rust does not
/// guarantee. A `FromIBinder::try_from` round-trip is also wrong: it
/// rejects a *local* callback whose concrete native type differs from the
/// target version's (descriptor matches but the `Inner<B>` downcast
/// fails), which is the normal case for this API.
///
/// `register/unregister_for_notifications` only ever serialize the
/// callback as its underlying `SIBinder` (`Serialize for dyn _` calls
/// `as_binder()` and nothing else), so a thin wrapper that returns the
/// original `SIBinder` is wire-identical and behavior-identical for both
/// local and proxy callbacks, with no `unsafe`. `onRegistration` is
/// unreachable here: the wrapper is only serialized and sent; inbound
/// notifications are delivered by the kernel to the original binder node,
/// never to this transient local forwarder.
#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
        feature = "android_15"
    )
))]
struct ForwardServiceCallback(crate::SIBinder);

#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
        feature = "android_15"
    )
))]
impl crate::Interface for ForwardServiceCallback {
    fn as_binder(&self) -> crate::SIBinder {
        self.0.clone()
    }
}

/// Build a per-version `Strong<dyn IServiceCallback>` that wraps the
/// unified callback into [`ForwardServiceCallback`]. Used by the
/// `register_for_notifications` / `unregister_for_notifications`
/// dispatch arms on every pre-16 protocol that has them.
#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
        feature = "android_15",
    )
))]
macro_rules! wrap_callback {
    ($modu:ident, $callback:expr) => {
        crate::Strong::<dyn $modu::IServiceCallback>::new(Box::new(ForwardServiceCallback(
            $callback.as_binder(),
        )))
    };
}

/// Collect a per-version `Vec<android_N::ServiceDebugInfo>` into the
/// unified `Vec<ServiceDebugInfo>`. Used by the `get_service_debug_info`
/// dispatch arms on every pre-16 protocol that has it (16 returns the
/// unified type directly).
#[cfg(all(
    target_os = "android",
    any(
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
        feature = "android_15",
    )
))]
macro_rules! collect_debug_info {
    ($modu:ident, $sm:expr) => {{
        let result = $modu::get_service_debug_info($sm)?;
        Ok(result
            .into_iter()
            .map(|info| ServiceDebugInfo {
                name: info.name,
                debugPid: info.debugPid,
            })
            .collect())
    }};
}

/// The `Status` an error-preserving `try_*` call returns when the running
/// service manager's protocol predates the method it was asked for.
///
/// `EX_UNSUPPORTED_OPERATION` is AOSP's code for "this interface has no
/// such method"; the message names the first Android version that does,
/// which is the actionable half for whoever reads it.
#[cfg(all(
    target_os = "android",
    any(feature = "android_10", feature = "android_11", feature = "android_12")
))]
fn unsupported(method: &str, since: u32) -> Status {
    (
        ExceptionCode::UnsupportedOperation,
        format!("{method} requires the Android {since} service-manager protocol or later").as_str(),
    )
        .into()
}

/// Emits the per-version `IServiceCallback` impl for [`ForwardServiceCallback`]
/// (one per supported pre-16 version).
macro_rules! forward_service_callback_impl {
    ($modu:ident, $feat:literal) => {
        #[cfg(all(target_os = "android", feature = $feat))]
        impl $modu::IServiceCallback for ForwardServiceCallback {
            fn r#onRegistration(
                &self,
                _name: &str,
                _binder: &crate::SIBinder,
            ) -> crate::BinderResult<()> {
                // Unreachable on the serialize-only path; see the
                // ForwardServiceCallback doc. Return an error rather than
                // panic in library code if it is ever reached.
                Err(crate::StatusCode::UnknownTransaction.into())
            }
        }
    };
}

forward_service_callback_impl!(android_11, "android_11");
forward_service_callback_impl!(android_12, "android_12");
forward_service_callback_impl!(android_13, "android_13");
forward_service_callback_impl!(android_14, "android_14");
forward_service_callback_impl!(android_15, "android_15");

/// `IClientCallback` analogue of [`ForwardServiceCallback`], used by
/// `register_client_callback` on every pre-16 protocol that has it. Same
/// rationale: the
/// per-version `android_N::IClientCallback` trait types have distinct
/// vtables, but `registerClientCallback` only serializes the callback as
/// its underlying `SIBinder`, so forwarding that binder is wire- and
/// behavior-identical. `onClients` is unreachable on the serialize-only
/// path (the kernel delivers notifications to the original binder node).
#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
        feature = "android_15"
    )
))]
struct ForwardClientCallback(crate::SIBinder);

#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
        feature = "android_15"
    )
))]
impl crate::Interface for ForwardClientCallback {
    fn as_binder(&self) -> crate::SIBinder {
        self.0.clone()
    }
}

/// Build a per-version `Strong<dyn IClientCallback>` wrapping the unified
/// callback into [`ForwardClientCallback`]. Used by the
/// `register_client_callback` dispatch arms on every pre-16 protocol that
/// has it.
#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
        feature = "android_15",
    )
))]
macro_rules! wrap_client_callback {
    ($modu:ident, $callback:expr) => {
        crate::Strong::<dyn $modu::IClientCallback>::new(Box::new(ForwardClientCallback(
            $callback.as_binder(),
        )))
    };
}

/// Emits the per-version `IClientCallback` impl for [`ForwardClientCallback`]
/// (one per supported pre-16 version).
macro_rules! forward_client_callback_impl {
    ($modu:ident, $feat:literal) => {
        #[cfg(all(target_os = "android", feature = $feat))]
        impl $modu::IClientCallback for ForwardClientCallback {
            fn r#onClients(
                &self,
                _registered: &crate::SIBinder,
                _has_clients: bool,
            ) -> crate::BinderResult<()> {
                // Unreachable on the serialize-only path; see the
                // ForwardClientCallback doc. Return an error rather than
                // panic in library code if it is ever reached.
                Err(crate::StatusCode::UnknownTransaction.into())
            }
        }
    };
}

forward_client_callback_impl!(android_11, "android_11");
forward_client_callback_impl!(android_12, "android_12");
forward_client_callback_impl!(android_13, "android_13");
forward_client_callback_impl!(android_14, "android_14");
forward_client_callback_impl!(android_15, "android_15");

impl ServiceManager {
    /// Checks if a service with the given name is available.
    ///
    /// This method is version-agnostic and works across all supported Android versions.
    pub fn check_service(&self, name: &str) -> Option<SIBinder> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(sm) => android_10::check_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::check_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::check_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::check_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::check_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::check_service(sm, name),
            ServiceManager::Android16(sm) => {
                android_16::check_service(sm, name).and_then(|s| s.service)
            }
        }
    }

    /// Resolve a service by name **without blocking** and cast it to the
    /// interface `T` — the `interface_cast(check_service(name))` of AOSP.
    ///
    /// Returns [`StatusCode::NameNotFound`] immediately if the service is not
    /// registered, in contrast to
    /// [`wait_for_interface`](Self::wait_for_interface), which blocks until it
    /// appears.
    pub fn check_interface<T: FromIBinder + ?Sized>(&self, name: &str) -> Result<Strong<T>> {
        match self.check_service(name) {
            Some(binder) => FromIBinder::try_from(binder),
            None => Err(StatusCode::NameNotFound),
        }
    }

    /// Like [`is_declared`](Self::is_declared), but reports the service
    /// manager's error instead of collapsing it to `false`.
    ///
    /// `rsb_hub` answers a `find`-denied `isDeclared` with `EX_SECURITY`
    /// (AOSP's `servicemanager` does the same through SELinux), and the
    /// swallowing wrapper renders that indistinguishable from "not
    /// declared". Anything that reports *why* — a diagnostic tool, a
    /// health check — needs the status, not the bool.
    pub fn try_is_declared(&self, name: &str) -> std::result::Result<bool, Status> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => Err(unsupported("isDeclared", 11)),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::IServiceManager::isDeclared(sm, name),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::IServiceManager::isDeclared(sm, name),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::IServiceManager::isDeclared(sm, name),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::IServiceManager::isDeclared(sm, name),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::IServiceManager::isDeclared(sm, name),
            ServiceManager::Android16(sm) => android_16::IServiceManager::isDeclared(sm, name),
        }
    }

    /// Like [`get_declared_instances`](Self::get_declared_instances), but
    /// reports the service manager's error instead of collapsing it to an
    /// empty list. See [`try_is_declared`](Self::try_is_declared).
    pub fn try_get_declared_instances(
        &self,
        iface: &str,
    ) -> std::result::Result<Vec<String>, Status> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => Err(unsupported("getDeclaredInstances", 12)),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(_) => Err(unsupported("getDeclaredInstances", 12)),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => {
                android_12::IServiceManager::getDeclaredInstances(sm, iface)
            }
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                android_13::IServiceManager::getDeclaredInstances(sm, iface)
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                android_14::IServiceManager::getDeclaredInstances(sm, iface)
            }
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => {
                android_15::IServiceManager::getDeclaredInstances(sm, iface)
            }
            ServiceManager::Android16(sm) => {
                android_16::IServiceManager::getDeclaredInstances(sm, iface)
            }
        }
    }

    /// Like [`get_connection_info`](Self::get_connection_info), but
    /// reports the service manager's error instead of collapsing it to
    /// `None`. See [`try_is_declared`](Self::try_is_declared).
    ///
    /// Each version generates its own `ConnectionInfo`, so the pre-16 arms
    /// rebuild the unified one field by field — the same shape the
    /// `getServiceDebugInfo` dispatch uses.
    pub fn try_get_connection_info(
        &self,
        name: &str,
    ) -> std::result::Result<Option<ConnectionInfo>, Status> {
        /// Rebuild the unified `ConnectionInfo` from a version's own.
        #[cfg(all(
            target_os = "android",
            any(feature = "android_13", feature = "android_14", feature = "android_15")
        ))]
        macro_rules! unify {
            ($call:expr) => {
                $call.map(|info| {
                    info.map(|info| ConnectionInfo {
                        ipAddress: info.ipAddress,
                        port: info.port,
                    })
                })
            };
        }
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => Err(unsupported("getConnectionInfo", 13)),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(_) => Err(unsupported("getConnectionInfo", 13)),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(_) => Err(unsupported("getConnectionInfo", 13)),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                unify!(android_13::IServiceManager::getConnectionInfo(sm, name))
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                unify!(android_14::IServiceManager::getConnectionInfo(sm, name))
            }
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => {
                unify!(android_15::IServiceManager::getConnectionInfo(sm, name))
            }
            ServiceManager::Android16(sm) => {
                android_16::IServiceManager::getConnectionInfo(sm, name)
            }
        }
    }

    /// Like [`list_services`](Self::list_services), but reports the
    /// service manager's error instead of collapsing it to an empty list.
    /// See [`try_is_declared`](Self::try_is_declared).
    pub fn try_list_services(
        &self,
        dump_priority: i32,
    ) -> std::result::Result<Vec<String>, Status> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(sm) => {
                // The legacy protocol enumerates by index and stops at the
                // first index that does not answer, so a failure part-way
                // through is indistinguishable from the end of the list.
                // There is no status to preserve.
                Ok(android_10::list_services(sm, dump_priority))
            }
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => {
                android_11::IServiceManager::listServices(sm, dump_priority)
            }
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => {
                android_12::IServiceManager::listServices(sm, dump_priority)
            }
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                android_13::IServiceManager::listServices(sm, dump_priority)
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                android_14::IServiceManager::listServices(sm, dump_priority)
            }
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => {
                android_15::IServiceManager::listServices(sm, dump_priority)
            }
            ServiceManager::Android16(sm) => {
                android_16::IServiceManager::listServices(sm, dump_priority)
            }
        }
    }

    /// Checks if a service with the given name is declared.
    ///
    /// Note: not supported on Android 10 - always returns false.
    pub fn is_declared(&self, name: &str) -> bool {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => {
                log::error!("is_declared: not supported on Android 10");
                false
            }
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::is_declared(sm, name),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::is_declared(sm, name),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::is_declared(sm, name),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::is_declared(sm, name),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::is_declared(sm, name),
            ServiceManager::Android16(sm) => android_16::is_declared(sm, name),
        }
    }

    /// Every declared instance of `iface`.
    ///
    /// Requires the Android 12 protocol or later; earlier ones have no
    /// `getDeclaredInstances`, and report none declared.
    pub fn get_declared_instances(&self, iface: &str) -> Vec<String> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => {
                log::error!("get_declared_instances: not supported on Android 10");
                Vec::new()
            }
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(_) => {
                log::error!("get_declared_instances: not supported on Android 11");
                Vec::new()
            }
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::get_declared_instances(sm, iface),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::get_declared_instances(sm, iface),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::get_declared_instances(sm, iface),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::get_declared_instances(sm, iface),
            ServiceManager::Android16(sm) => android_16::get_declared_instances(sm, iface),
        }
    }

    /// Connection info declared for `name`, if any.
    ///
    /// Requires the Android 13 protocol or later; earlier ones have no
    /// `getConnectionInfo`, and report none.
    pub fn get_connection_info(&self, name: &str) -> Option<ConnectionInfo> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => {
                log::error!("get_connection_info: not supported on Android 10");
                None
            }
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(_) => {
                log::error!("get_connection_info: not supported on Android 11");
                None
            }
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(_) => {
                log::error!("get_connection_info: not supported on Android 12");
                None
            }
            // Each version generates its own `ConnectionInfo`; rebuild the
            // unified one field by field, as the `getServiceDebugInfo`
            // dispatch does for `ServiceDebugInfo`.
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                android_13::get_connection_info(sm, name).map(|info| ConnectionInfo {
                    ipAddress: info.ipAddress,
                    port: info.port,
                })
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                android_14::get_connection_info(sm, name).map(|info| ConnectionInfo {
                    ipAddress: info.ipAddress,
                    port: info.port,
                })
            }
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => {
                android_15::get_connection_info(sm, name).map(|info| ConnectionInfo {
                    ipAddress: info.ipAddress,
                    port: info.port,
                })
            }
            ServiceManager::Android16(sm) => android_16::get_connection_info(sm, name),
        }
    }

    /// Returns a list of all registered services with the specified dump priority.
    ///
    /// This method is version-agnostic and works across all supported Android versions.
    /// On Android 10, uses the iterative wire protocol internally.
    pub fn list_services(&self, dump_priority: i32) -> Vec<String> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(sm) => android_10::list_services(sm, dump_priority),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::list_services(sm, dump_priority),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::list_services(sm, dump_priority),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::list_services(sm, dump_priority),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::list_services(sm, dump_priority),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::list_services(sm, dump_priority),
            ServiceManager::Android16(sm) => android_16::list_services(sm, dump_priority),
        }
    }

    /// Registers a service with the service manager.
    ///
    /// This method is version-agnostic and works across all supported Android
    /// versions. `binder` accepts anything convertible into [`SIBinder`] — a
    /// typed `Strong<dyn IFoo>` goes in directly, no `.as_binder()` needed.
    pub fn add_service(
        &self,
        identifier: &str,
        binder: impl Into<SIBinder>,
    ) -> std::result::Result<(), Status> {
        let binder = binder.into();
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(sm) => android_10::add_service(sm, identifier, binder),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::add_service(sm, identifier, binder),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::add_service(sm, identifier, binder),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::add_service(sm, identifier, binder),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::add_service(sm, identifier, binder),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::add_service(sm, identifier, binder),
            ServiceManager::Android16(sm) => android_16::add_service(sm, identifier, binder),
        }
    }

    /// `add_service` + `FLAG_IS_LAZY_SERVICE` (15 r6+ and 16 only; 11–14 have no such bit, 10 is refused).
    pub(crate) fn add_lazy_service(
        &self,
        identifier: &str,
        binder: impl Into<SIBinder>,
    ) -> std::result::Result<(), Status> {
        let binder = binder.into();
        match self {
            // The legacy C protocol has neither `registerClientCallback` nor
            // `tryUnregisterService`, so a service published here could
            // never be tracked *or* taken back down. Refuse before it is.
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => Err(unsupported("lazy service registration", 11)),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::add_service(sm, identifier, binder),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::add_service(sm, identifier, binder),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::add_service(sm, identifier, binder),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::add_service(sm, identifier, binder),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::add_lazy_service(sm, identifier, binder),
            ServiceManager::Android16(sm) => android_16::add_lazy_service(sm, identifier, binder),
        }
    }

    /// Retrieves debug information about all currently registered services.
    ///
    /// Note: not supported on Android 10 or Android 11 - returns an error on those versions.
    pub fn get_service_debug_info(&self) -> Result<Vec<ServiceDebugInfo>> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => {
                log::error!(
                    "get_service_debug_info: Unsupported Android SDK version: {}",
                    crate::get_android_sdk_version()
                );
                Err(StatusCode::UnknownTransaction)
            }
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(_) => {
                log::error!(
                    "get_service_debug_info: Unsupported Android SDK version: {}",
                    crate::get_android_sdk_version()
                );
                Err(StatusCode::UnknownTransaction)
            }
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => collect_debug_info!(android_12, sm),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => collect_debug_info!(android_13, sm),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => collect_debug_info!(android_14, sm),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => collect_debug_info!(android_15, sm),
            ServiceManager::Android16(sm) => android_16::get_service_debug_info(sm),
        }
    }

    /// Like [`get_service_debug_info`](Self::get_service_debug_info), but
    /// preserves the service manager's `Status` — message included —
    /// instead of flattening it to a [`StatusCode`].
    ///
    /// A `list`-denied call arrives as `EX_SECURITY` carrying the policy's
    /// own wording; the flattening conversion turns that into an anonymous
    /// `FailedTransaction`, which is not something a diagnostic can report.
    pub fn try_get_service_debug_info(&self) -> std::result::Result<Vec<ServiceDebugInfo>, Status> {
        /// Rebuild the unified `ServiceDebugInfo` from a version's own.
        #[cfg(all(
            target_os = "android",
            any(
                feature = "android_12",
                feature = "android_13",
                feature = "android_14",
                feature = "android_15"
            )
        ))]
        macro_rules! unify {
            ($call:expr) => {
                $call.map(|v| {
                    v.into_iter()
                        .map(|info| ServiceDebugInfo {
                            name: info.name,
                            debugPid: info.debugPid,
                        })
                        .collect()
                })
            };
        }
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => Err(unsupported("getServiceDebugInfo", 12)),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(_) => Err(unsupported("getServiceDebugInfo", 12)),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => {
                unify!(android_12::IServiceManager::getServiceDebugInfo(sm))
            }
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                unify!(android_13::IServiceManager::getServiceDebugInfo(sm))
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                unify!(android_14::IServiceManager::getServiceDebugInfo(sm))
            }
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => {
                unify!(android_15::IServiceManager::getServiceDebugInfo(sm))
            }
            ServiceManager::Android16(sm) => android_16::IServiceManager::getServiceDebugInfo(sm),
        }
    }

    /// Registers for notifications when a service becomes available.
    ///
    /// Note: not supported on Android 10 - returns an error on that version.
    pub fn register_for_notifications(
        &self,
        name: &str,
        callback: &crate::Strong<dyn IServiceCallback>,
    ) -> Result<()> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => {
                log::error!("register_for_notifications: not supported on Android 10");
                Err(StatusCode::UnknownTransaction)
            }
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::register_for_notifications(
                sm,
                name,
                &wrap_callback!(android_11, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::register_for_notifications(
                sm,
                name,
                &wrap_callback!(android_12, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::register_for_notifications(
                sm,
                name,
                &wrap_callback!(android_13, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::register_for_notifications(
                sm,
                name,
                &wrap_callback!(android_14, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::register_for_notifications(
                sm,
                name,
                &wrap_callback!(android_15, callback),
            ),
            ServiceManager::Android16(sm) => {
                android_16::register_for_notifications(sm, name, callback)
            }
        }
    }

    /// Unregisters from notifications for a service.
    ///
    /// Note: not supported on Android 10 - returns an error on that version.
    pub fn unregister_for_notifications(
        &self,
        name: &str,
        callback: &crate::Strong<dyn IServiceCallback>,
    ) -> Result<()> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => {
                log::error!("unregister_for_notifications: not supported on Android 10");
                Err(StatusCode::UnknownTransaction)
            }
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::unregister_for_notifications(
                sm,
                name,
                &wrap_callback!(android_11, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::unregister_for_notifications(
                sm,
                name,
                &wrap_callback!(android_12, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::unregister_for_notifications(
                sm,
                name,
                &wrap_callback!(android_13, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::unregister_for_notifications(
                sm,
                name,
                &wrap_callback!(android_14, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::unregister_for_notifications(
                sm,
                name,
                &wrap_callback!(android_15, callback),
            ),
            ServiceManager::Android16(sm) => {
                android_16::unregister_for_notifications(sm, name, callback)
            }
        }
    }

    /// [`register_client_callback`](Self::register_client_callback) keeping the
    /// service manager's own [`Status`] (`EX_SECURITY` and so on), for
    /// [`LazyServiceRegistrar`](crate::lazy_service::LazyServiceRegistrar).
    pub(crate) fn register_client_callback_status(
        &self,
        name: &str,
        service: &SIBinder,
        callback: &crate::Strong<dyn IClientCallback>,
    ) -> std::result::Result<(), Status> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => Err(unsupported("registerClientCallback", 11)),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::IServiceManager::registerClientCallback(
                sm,
                name,
                service,
                &wrap_client_callback!(android_11, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::IServiceManager::registerClientCallback(
                sm,
                name,
                service,
                &wrap_client_callback!(android_12, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::IServiceManager::registerClientCallback(
                sm,
                name,
                service,
                &wrap_client_callback!(android_13, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::IServiceManager::registerClientCallback(
                sm,
                name,
                service,
                &wrap_client_callback!(android_14, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::IServiceManager::registerClientCallback(
                sm,
                name,
                service,
                &wrap_client_callback!(android_15, callback),
            ),
            ServiceManager::Android16(sm) => {
                android_16::IServiceManager::registerClientCallback(sm, name, service, callback)
            }
        }
    }

    /// [`try_unregister_service`](Self::try_unregister_service) keeping the
    /// service manager's own [`Status`], for
    /// [`LazyServiceRegistrar`](crate::lazy_service::LazyServiceRegistrar).
    pub(crate) fn try_unregister_service_status(
        &self,
        name: &str,
        service: &SIBinder,
    ) -> std::result::Result<(), Status> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => Err(unsupported("tryUnregisterService", 11)),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => {
                android_11::IServiceManager::tryUnregisterService(sm, name, service)
            }
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => {
                android_12::IServiceManager::tryUnregisterService(sm, name, service)
            }
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                android_13::IServiceManager::tryUnregisterService(sm, name, service)
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                android_14::IServiceManager::tryUnregisterService(sm, name, service)
            }
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => {
                android_15::IServiceManager::tryUnregisterService(sm, name, service)
            }
            ServiceManager::Android16(sm) => {
                android_16::IServiceManager::tryUnregisterService(sm, name, service)
            }
        }
    }

    /// Registers a callback that fires when the set of clients holding a
    /// reference to `service` changes — the building block for lazy
    /// (on-demand) services. AOSP `IServiceManager::registerClientCallback`.
    ///
    /// `service` is the binder previously handed to [`add_service`](Self::add_service)
    /// (e.g. `my_binder.as_binder()`); `callback` is a `BnClientCallback`.
    ///
    /// Note: not supported on Android 10 — returns an error on that version.
    pub fn register_client_callback(
        &self,
        name: &str,
        service: &SIBinder,
        callback: &crate::Strong<dyn IClientCallback>,
    ) -> Result<()> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => {
                log::error!("register_client_callback: not supported on Android 10");
                Err(StatusCode::UnknownTransaction)
            }
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::register_client_callback(
                sm,
                name,
                service,
                &wrap_client_callback!(android_11, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::register_client_callback(
                sm,
                name,
                service,
                &wrap_client_callback!(android_12, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::register_client_callback(
                sm,
                name,
                service,
                &wrap_client_callback!(android_13, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::register_client_callback(
                sm,
                name,
                service,
                &wrap_client_callback!(android_14, callback),
            ),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::register_client_callback(
                sm,
                name,
                service,
                &wrap_client_callback!(android_15, callback),
            ),
            ServiceManager::Android16(sm) => {
                android_16::register_client_callback(sm, name, service, callback)
            }
        }
    }

    /// Attempts to unregister a service previously added with
    /// [`add_service`](Self::add_service); the service manager honors it only
    /// if no clients currently hold a reference. AOSP
    /// `IServiceManager::tryUnregisterService`.
    ///
    /// Note: not supported on Android 10 — returns an error on that version.
    pub fn try_unregister_service(&self, name: &str, service: &SIBinder) -> Result<()> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(_) => {
                log::error!("try_unregister_service: not supported on Android 10");
                Err(StatusCode::UnknownTransaction)
            }
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::try_unregister_service(sm, name, service),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::try_unregister_service(sm, name, service),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::try_unregister_service(sm, name, service),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::try_unregister_service(sm, name, service),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::try_unregister_service(sm, name, service),
            ServiceManager::Android16(sm) => android_16::try_unregister_service(sm, name, service),
        }
    }

    /// Error-preserving, non-blocking lookup: `Ok(Some)` found, `Ok(None)` not
    /// registered, `Err` on a transport/SM failure — the distinction that
    /// [`check_service`](Self::check_service) collapses to `None`. Reach for
    /// this when you must tell "the service isn't there" apart from "the
    /// service manager is unreachable" (e.g. to fail fast instead of retrying).
    ///
    /// This is the `getService` wire call, so unlike `check_service` it lets
    /// the service manager start an unregistered lazy service.
    ///
    /// It is also what [`wait_for_service`](Self::wait_for_service) uses to give
    /// up on a dead service manager instead of looping forever (AOSP
    /// `realGetService`). The Android 10 legacy C SM cannot tell not-found from
    /// a transport error, so its arm reports any failure as `Ok(None)` and never
    /// `Err`.
    pub fn try_get_service(&self, name: &str) -> Result<Option<SIBinder>> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(sm) => android_10::try_get_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::try_get_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::try_get_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::try_get_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::try_get_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_15"))]
            ServiceManager::Android15(sm) => android_15::try_get_service(sm, name),
            ServiceManager::Android16(sm) => {
                Ok(android_16::try_get_service(sm, name)?.and_then(|s| s.service))
            }
        }
    }

    /// Interface-typed [`try_get_service`](Self::try_get_service): a
    /// non-blocking lookup that preserves the not-found vs. SM-unreachable
    /// distinction and casts the result to `T`.
    ///
    /// `Ok(Some(strong))` found and cast, `Ok(None)` not registered, `Err` on a
    /// transport/SM failure *or* a descriptor mismatch. A name registered under
    /// the wrong interface stays distinguishable from an unreachable service
    /// manager: the former is exactly [`StatusCode::BadType`], the latter a
    /// transport code (the lookup never yields `BadType` itself). Contrast
    /// [`check_interface`](Self::check_interface), which folds not-found into
    /// `Err(NameNotFound)`, and [`wait_for_interface`](Self::wait_for_interface),
    /// which blocks until the service appears.
    pub fn try_get_interface<T: FromIBinder + ?Sized>(
        &self,
        name: &str,
    ) -> Result<Option<Strong<T>>> {
        match self.try_get_service(name)? {
            Some(binder) => FromIBinder::try_from(binder).map(Some),
            None => Ok(None),
        }
    }

    /// Block until the service named `name` is registered, then return it —
    /// the event-driven equivalent of AOSP
    /// `IServiceManager::waitForService`
    /// (`frameworks/native/libs/binder/IServiceManager.cpp`).
    ///
    /// A single `getService` fast path is tried first. If the service is
    /// absent, an [`IServiceCallback`] is registered and this thread blocks on
    /// a condition variable until the service manager fires `onRegistration` —
    /// the SM fires it immediately when the service is already present, so the
    /// register-after-miss race is covered. Each second the wait also re-polls
    /// `getService`, mirroring AOSP's per-tick `realGetService` retry for lazy
    /// services.
    ///
    /// The wait is **unbounded** on every supported version (matching AOSP's
    /// `while(true)`): it returns when the service appears, or `None` when the
    /// service manager itself is unreachable — a transport error on the lookup,
    /// mirroring AOSP's `realGetService`-error → `nullptr`.
    ///
    /// # Thread pool: event-driven vs. polling
    ///
    /// `onRegistration` arrives as an inbound transaction, so it is delivered
    /// promptly only when a binder worker thread is reading commands — call
    /// [`crate::ProcessState::start_thread_pool`] for that. Without a thread
    /// pool this does **not** deadlock: the 1-second condvar timeout re-polls
    /// the service on the calling thread each tick, so it degrades to ~1s
    /// polling and still resolves within ~1s of registration. AOSP
    /// `waitForService` behaves identically — the per-tick re-poll and its
    /// "no guaranteed threads" warning are about *efficiency*, not correctness.
    ///
    /// # Android 10
    ///
    /// The legacy C service manager has no registration notifications, so the
    /// event path is unavailable and the wait transparently falls back to ~1s
    /// polling. The contract is the same (unbounded until the service appears),
    /// except that the legacy protocol cannot distinguish "not registered" from
    /// a transport error, so the wait does not give up early there — it keeps
    /// polling.
    pub fn wait_for_service(&self, name: &str) -> Option<SIBinder> {
        // Fast path: already registered — no callback needed. A transport error
        // means the SM is unreachable, so give up (AOSP's initial
        // `realGetService` error → `nullptr`).
        match self.try_get_service(name) {
            Ok(Some(binder)) => return Some(binder),
            Ok(None) => {}
            Err(err) => {
                log::warn!("wait_for_service: lookup for {name} failed ({err:?})");
                return None;
            }
        }

        let state = Arc::new(WaiterState::default());
        let callback = BnServiceCallback::new_binder(Waiter(state.clone()));

        if let Err(err) = self.register_for_notifications(name, &callback) {
            // Notifications unsupported (Android 10) or the SM is unreachable;
            // either way fall back to polling.
            log::warn!(
                "wait_for_service: notifications unavailable for {name} ({err:?}); \
                 falling back to polling"
            );
            return self.poll_for_service(name);
        }
        // Always unregister, even on early return / panic (AOSP's `Defer`).
        let _unregister = UnregisterOnDrop {
            sm: self,
            name,
            callback: &callback,
        };

        let mut waited_secs: u64 = 0;
        loop {
            {
                let guard = state.inner.lock().unwrap_or_else(|e| e.into_inner());
                let (guard, _) = state
                    .cv
                    .wait_timeout_while(guard, std::time::Duration::from_secs(1), |binder| {
                        binder.is_none()
                    })
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(binder) = guard.as_ref() {
                    return Some(binder.clone());
                }
            }
            // Throttle to ~every 10s so a slow/missing service stays visible
            // without flooding the log every second.
            if waited_secs % 10 == 0 {
                log::warn!("wait_for_service: still waiting for {name} ({waited_secs}s)...");
            }
            waited_secs += 1;
            // Lazy-service race: re-poll each tick (AOSP `realGetService`),
            // giving up if the service manager has become unreachable.
            match self.try_get_service(name) {
                Ok(Some(binder)) => return Some(binder),
                Ok(None) => {}
                Err(err) => {
                    log::warn!("wait_for_service: lookup for {name} failed ({err:?})");
                    return None;
                }
            }
        }
    }

    /// Interface-typed [`wait_for_service`](Self::wait_for_service): block
    /// until `name` is registered, then cast it to `T`. The event-driven
    /// equivalent of AOSP `waitForService` + `interface_cast` (the binding's
    /// `wait_for_interface`). Returns [`StatusCode::NameNotFound`] only when
    /// the wait gives up — see [`wait_for_service`](Self::wait_for_service).
    pub fn wait_for_interface<T: FromIBinder + ?Sized>(&self, name: &str) -> Result<Strong<T>> {
        match self.wait_for_service(name) {
            Some(binder) => FromIBinder::try_from(binder),
            None => Err(StatusCode::NameNotFound),
        }
    }

    /// Unbounded fallback poll used by
    /// [`wait_for_service`](Self::wait_for_service) when the service manager
    /// has no registration notifications (Android 10) or the notification
    /// registration failed. Polls once per second until the service appears
    /// (`Some`) or a transport error shows the service manager is unreachable
    /// (`None`) — the same contract as the event path. On Android 10 a failure
    /// is reported as not-found, so it keeps polling rather than giving up.
    fn poll_for_service(&self, name: &str) -> Option<SIBinder> {
        loop {
            match self.try_get_service(name) {
                Ok(Some(binder)) => return Some(binder),
                Ok(None) => {}
                Err(err) => {
                    log::warn!("poll_for_service: lookup for {name} failed ({err:?}); giving up");
                    return None;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

/// Shared state between a [`Waiter`] callback (registered with the service
/// manager) and the thread blocked in [`ServiceManager::wait_for_service`].
/// `onRegistration` stores the binder and signals `cv`; the waiter observes
/// it under `inner`.
#[derive(Default)]
struct WaiterState {
    inner: std::sync::Mutex<Option<SIBinder>>,
    cv: std::sync::Condvar,
}

/// One-shot [`IServiceCallback`] that records the registered binder and wakes
/// [`ServiceManager::wait_for_service`]. Mirrors the local `Waiter` class
/// inside AOSP `IServiceManager::waitForService`.
struct Waiter(Arc<WaiterState>);

impl Interface for Waiter {}

impl IServiceCallback for Waiter {
    fn onRegistration(&self, _name: &str, service: &SIBinder) -> crate::status::BinderResult<()> {
        let mut guard = self.0.inner.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(service.clone());
        drop(guard);
        // Exactly one thread waits on this state (the matching
        // `wait_for_service` call), mirroring AOSP's `mCv.notify_one()`.
        self.0.cv.notify_one();
        Ok(())
    }
}

/// RAII: unregister the wait callback when the wait ends (success, error, or
/// panic), mirroring the `Defer unregister` in AOSP `waitForService`.
struct UnregisterOnDrop<'a> {
    sm: &'a ServiceManager,
    name: &'a str,
    callback: &'a Strong<dyn IServiceCallback>,
}

impl Drop for UnregisterOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self
            .sm
            .unregister_for_notifications(self.name, self.callback);
    }
}

//------------------------------------------------------------------------------
// Convenience Functions
//------------------------------------------------------------------------------
// The following functions provide a simpler API by using the default ServiceManager instance

/// Convenience function to list services from the default ServiceManager.
///
/// This is equivalent to `default().list_services(dump_priority)`.
///
/// Panics on an uninitialized `ProcessState` — see [`default`]'s `# Panics`.
#[inline]
pub fn list_services(dump_priority: i32) -> Vec<String> {
    // An unavailable ServiceManager yields an empty list.
    default()
        .map(|sm| sm.list_services(dump_priority))
        .unwrap_or_default()
}

/// Convenience function to register for notifications from the default ServiceManager.
///
/// This is equivalent to `default().register_for_notifications(name, callback)`.
#[inline]
pub fn register_for_notifications(
    name: &str,
    callback: &crate::Strong<dyn IServiceCallback>,
) -> Result<()> {
    default()?.register_for_notifications(name, callback)
}

/// Convenience function to unregister from notifications from the default ServiceManager.
///
/// This is equivalent to `default().unregister_for_notifications(name, callback)`.
#[inline]
pub fn unregister_for_notifications(
    name: &str,
    callback: &crate::Strong<dyn IServiceCallback>,
) -> Result<()> {
    default()?.unregister_for_notifications(name, callback)
}

/// Convenience function to register a client-presence callback on the default
/// ServiceManager.
///
/// Equivalent to `default().register_client_callback(name, service, callback)`.
/// See [`ServiceManager::register_client_callback`].
#[inline]
pub fn register_client_callback(
    name: &str,
    service: &SIBinder,
    callback: &crate::Strong<dyn IClientCallback>,
) -> Result<()> {
    default()?.register_client_callback(name, service, callback)
}

/// Convenience function to attempt unregistering a service from the default
/// ServiceManager.
///
/// Equivalent to `default().try_unregister_service(name, service)`.
/// See [`ServiceManager::try_unregister_service`].
#[inline]
pub fn try_unregister_service(name: &str, service: &SIBinder) -> Result<()> {
    default()?.try_unregister_service(name, service)
}

/// Convenience function to add a service to the default ServiceManager.
///
/// This is equivalent to `default().add_service(identifier, binder)`. `binder`
/// accepts anything convertible into [`SIBinder`], so a typed
/// `Strong<dyn IFoo>` can be passed directly without `.as_binder()`.
#[inline]
pub fn add_service(
    identifier: &str,
    binder: impl Into<SIBinder>,
) -> std::result::Result<(), Status> {
    // `?` converts a StatusCode init failure into Status via From<StatusCode>.
    default()?.add_service(identifier, binder)
}

/// [`ServiceManager::add_lazy_service`] on the default service manager.
pub(crate) fn add_lazy_service(
    identifier: &str,
    binder: impl Into<SIBinder>,
) -> std::result::Result<(), Status> {
    default()?.add_lazy_service(identifier, binder)
}

/// `default().register_client_callback_status(..)`; see [`ServiceManager::register_client_callback_status`].
pub(crate) fn register_client_callback_status(
    name: &str,
    service: &SIBinder,
    callback: &crate::Strong<dyn IClientCallback>,
) -> std::result::Result<(), Status> {
    default()?.register_client_callback_status(name, service, callback)
}

/// `default().try_unregister_service_status(..)`; see [`ServiceManager::try_unregister_service_status`].
pub(crate) fn try_unregister_service_status(
    name: &str,
    service: &SIBinder,
) -> std::result::Result<(), Status> {
    default()?.try_unregister_service_status(name, service)
}

/// Convenience function to wait for a service from the default
/// ServiceManager.
///
/// Equivalent to `default().wait_for_service(name)`; returns `None` if the
/// default service manager is unavailable. The event-driven replacement for
/// hand-rolled client retry loops — see
/// [`ServiceManager::wait_for_service`] for the blocking and thread-pool
/// contract.
///
/// Panics on an uninitialized `ProcessState` — see [`default`]'s `# Panics`.
#[inline]
pub fn wait_for_service(name: &str) -> Option<SIBinder> {
    default().ok()?.wait_for_service(name)
}

/// Convenience function to wait for an interface from the default
/// ServiceManager.
///
/// Equivalent to `default().wait_for_interface(name)` — the event-driven,
/// AOSP `waitForService`-style alternative to polling around
/// [`try_get_interface`]. See [`ServiceManager::wait_for_service`].
#[inline]
pub fn wait_for_interface<T: FromIBinder + ?Sized>(name: &str) -> Result<Strong<T>> {
    default()?.wait_for_interface(name)
}

/// Convenience function to check if a service is available from the default ServiceManager.
///
/// This is equivalent to `default().check_service(name)`.
///
/// Panics on an uninitialized `ProcessState` — see [`default`]'s `# Panics`.
#[inline]
pub fn check_service(name: &str) -> Option<SIBinder> {
    default().ok()?.check_service(name)
}

/// Convenience function to resolve an interface **without blocking** from the
/// default ServiceManager.
///
/// Equivalent to `default().check_interface(name)` — the immediate,
/// non-blocking counterpart to [`wait_for_interface`]. Returns
/// [`StatusCode::NameNotFound`] at once if the service is not registered.
#[inline]
pub fn check_interface<T: FromIBinder + ?Sized>(name: &str) -> Result<Strong<T>> {
    default()?.check_interface(name)
}

/// Convenience function for an error-preserving, non-blocking lookup from the
/// default ServiceManager.
///
/// Equivalent to `default().try_get_service(name)`, except a ServiceManager
/// that cannot be reached at all surfaces as `Err` rather than `Ok(None)`. Use
/// this (over [`check_service`]) when you must distinguish "service not
/// registered" (`Ok(None)`) from "service manager unreachable" (`Err`). See
/// [`ServiceManager::try_get_service`].
#[inline]
pub fn try_get_service(name: &str) -> Result<Option<SIBinder>> {
    default()?.try_get_service(name)
}

/// Convenience function for an error-preserving, non-blocking interface lookup
/// from the default ServiceManager.
///
/// Equivalent to `default().try_get_interface(name)`. `Ok(Some)` found and
/// cast, `Ok(None)` not registered, `Err` on a transport/SM failure or a
/// descriptor mismatch. See [`ServiceManager::try_get_interface`].
#[inline]
pub fn try_get_interface<T: FromIBinder + ?Sized>(name: &str) -> Result<Option<Strong<T>>> {
    default()?.try_get_interface(name)
}

/// Convenience function to check if a service is declared from the default ServiceManager.
///
/// This is equivalent to `default().is_declared(name)`.
///
/// Panics on an uninitialized `ProcessState` — see [`default`]'s `# Panics`.
/// A `false` here means "not declared", never "no service manager".
#[inline]
pub fn is_declared(name: &str) -> bool {
    default().map(|sm| sm.is_declared(name)).unwrap_or(false)
}

/// Convenience function for [`ServiceManager::try_is_declared`] — the
/// error-preserving counterpart to [`is_declared`].
#[inline]
pub fn try_is_declared(name: &str) -> std::result::Result<bool, Status> {
    default()?.try_is_declared(name)
}

/// Convenience function for [`ServiceManager::get_declared_instances`].
///
/// Panics on an uninitialized `ProcessState` — see [`default`]'s `# Panics`.
#[inline]
pub fn get_declared_instances(iface: &str) -> Vec<String> {
    default()
        .map(|sm| sm.get_declared_instances(iface))
        .unwrap_or_default()
}

/// Convenience function for [`ServiceManager::try_get_declared_instances`] —
/// the error-preserving counterpart to [`get_declared_instances`].
#[inline]
pub fn try_get_declared_instances(iface: &str) -> std::result::Result<Vec<String>, Status> {
    default()?.try_get_declared_instances(iface)
}

/// Convenience function for [`ServiceManager::get_connection_info`].
#[inline]
pub fn get_connection_info(name: &str) -> Option<ConnectionInfo> {
    default().ok().and_then(|sm| sm.get_connection_info(name))
}

/// Convenience function for [`ServiceManager::try_get_connection_info`] —
/// the error-preserving counterpart to [`get_connection_info`].
#[inline]
pub fn try_get_connection_info(name: &str) -> std::result::Result<Option<ConnectionInfo>, Status> {
    default()?.try_get_connection_info(name)
}

/// Convenience function for [`ServiceManager::try_list_services`] — the
/// error-preserving counterpart to [`list_services`]. A denied `list` and
/// an empty registry are the same answer through [`list_services`]; they
/// are `Err` and `Ok(vec![])` here.
#[inline]
pub fn try_list_services(dump_priority: i32) -> std::result::Result<Vec<String>, Status> {
    default()?.try_list_services(dump_priority)
}

/// Convenience function for [`ServiceManager::try_get_service_debug_info`] —
/// the `Status`-preserving counterpart to [`get_service_debug_info`].
#[inline]
pub fn try_get_service_debug_info() -> std::result::Result<Vec<ServiceDebugInfo>, Status> {
    default()?.try_get_service_debug_info()
}

/// Convenience function to get debug information about all services from the default ServiceManager.
///
/// This is equivalent to `default().get_service_debug_info()`.
/// Note that this feature may not be available on all Android versions.
#[inline]
pub fn get_service_debug_info() -> Result<Vec<ServiceDebugInfo>> {
    default()?.get_service_debug_info()
}
