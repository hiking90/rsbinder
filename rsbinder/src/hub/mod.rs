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
//! unreachable" (`Err`). The deprecated `get_service`/`get_interface` are
//! superseded by these. On the Android 10 legacy C service manager, which
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
//!     let sm = hub::android_16::BpServiceManager::getService().unwrap();
//!     // Use Android 16 specific methods here
//! }
//! ```

use std::sync::{Arc, OnceLock};

/// The common body of every per-version `servicemanager_N` module
/// (Android 11 through 14). Each call expands to the same
/// `BpServiceManager` re-exports + dispatch wrappers; version-specific
/// additions (e.g. `get_service_debug_info` since 12) go in the
/// `$($extra:tt)*` repetition. Caller emits `include!(...)` for the
/// generated AIDL bindings *before* invoking this macro so that the
/// `android::os::*` paths below resolve in the caller's scope.
#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
    )
))]
macro_rules! impl_sm_module_body {
    ($($extra:tt)*) => {
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
}
#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
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
    #[cfg(feature = "rpc")]
    pub use super::accessor_16::{
        __fuzz_accessor_error_decode, accessor_error_name, resolve_accessor, BnAccessor,
        BpAccessor, IAccessor, IAccessorDefault, IAccessorDefaultRef,
        ERROR_CONNECTION_INFO_NOT_FOUND, ERROR_FAILED_TO_CONNECT_EACCES,
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
    BnClientCallback, BnServiceCallback, IClientCallback, IServiceCallback, ServiceDebugInfo,
    DUMP_FLAG_PRIORITY_ALL, DUMP_FLAG_PRIORITY_CRITICAL, DUMP_FLAG_PRIORITY_DEFAULT,
    DUMP_FLAG_PRIORITY_HIGH, DUMP_FLAG_PRIORITY_NORMAL, DUMP_FLAG_PROTO,
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
    Android16(android_16::BpServiceManager),
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
            // UAPI is unchanged), so it is served by the `android_16` module,
            // mirroring how Android 15 is served by `android_14`.
            sdk_versions::ANDROID_16 | sdk_versions::ANDROID_17 => {
                create_service_manager!(Android16, android_16)
            }
            // Android 15 (SDK 35) shares Android 14's service-manager wire
            // format, so it is served by the `android_14` feature — there
            // is no separate `android_15` feature. A build that enables
            // only `android_16` therefore returns `InvalidOperation` on an
            // Android 15 device; enable `android_14` to cover 14 *and* 15.
            #[cfg(feature = "android_14")]
            sdk_versions::ANDROID_14 | sdk_versions::ANDROID_15 => {
                create_service_manager!(Android14, android_14)
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
        feature = "android_14"
    )
))]
struct ForwardServiceCallback(crate::SIBinder);

#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14"
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
/// dispatch arms on Android 11–14.
#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
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
/// dispatch arms on Android 12–14 (16 returns the unified type directly).
#[cfg(all(
    target_os = "android",
    any(feature = "android_12", feature = "android_13", feature = "android_14",)
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

/// Emits the per-version `IServiceCallback` impl for [`ForwardServiceCallback`]
/// (one per supported pre-16 version; collapses what was 4× duplicated).
macro_rules! forward_service_callback_impl {
    ($modu:ident, $feat:literal) => {
        #[cfg(all(target_os = "android", feature = $feat))]
        impl $modu::IServiceCallback for ForwardServiceCallback {
            fn r#onRegistration(
                &self,
                _name: &str,
                _binder: &crate::SIBinder,
            ) -> crate::status::Result<()> {
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

/// `IClientCallback` analogue of [`ForwardServiceCallback`], used by
/// `register_client_callback` on Android 11–14. Same rationale: the
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
        feature = "android_14"
    )
))]
struct ForwardClientCallback(crate::SIBinder);

#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14"
    )
))]
impl crate::Interface for ForwardClientCallback {
    fn as_binder(&self) -> crate::SIBinder {
        self.0.clone()
    }
}

/// Build a per-version `Strong<dyn IClientCallback>` wrapping the unified
/// callback into [`ForwardClientCallback`]. Used by the
/// `register_client_callback` dispatch arms on Android 11–14.
#[cfg(all(
    target_os = "android",
    any(
        feature = "android_11",
        feature = "android_12",
        feature = "android_13",
        feature = "android_14",
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
            ) -> crate::status::Result<()> {
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

impl ServiceManager {
    /// Resolve a service by name through the `getService` wire call.
    ///
    /// On Android 11+ this is a **single attempt** — the wire call answers
    /// with whatever is registered *now* (AOSP's "block a few seconds" was a
    /// libbinder client-side poll, not the wire semantics). On the Android 10
    /// legacy C service manager, whose `GET_SERVICE` wire call is itself
    /// non-blocking, it polls ~5s client-side to mirror AOSP. That
    /// inconsistency is why this is deprecated: use
    /// [`wait_for_service`](Self::wait_for_service) to block until the service
    /// appears, or [`check_service`](Self::check_service) for a uniformly
    /// non-blocking lookup.
    #[deprecated(
        note = "inconsistent wait behavior across versions; use `wait_for_service` \
                to block until the service appears, or `check_service` for a \
                non-blocking lookup"
    )]
    pub fn get_service(&self, name: &str) -> Option<SIBinder> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(sm) => android_10::get_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::get_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::get_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::get_service(sm, name),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::get_service(sm, name),
            ServiceManager::Android16(sm) => {
                android_16::get_service(sm, name).and_then(|s| s.service)
            }
        }
    }

    /// Resolve a service by name and cast it to the interface `T`, using the
    /// same `getService` wire call as [`get_service`](Self::get_service).
    ///
    /// Inherits `get_service`'s version-dependent wait behavior (single
    /// attempt on Android 11+, ~5s client poll on Android 10), so it is
    /// deprecated for the same reason: use
    /// [`wait_for_interface`](Self::wait_for_interface) to block until the
    /// service appears, or [`check_interface`](Self::check_interface) for a
    /// non-blocking lookup.
    #[deprecated(
        note = "inconsistent wait behavior across versions; use `wait_for_interface` \
                to block until the service appears, or `check_interface` for a \
                non-blocking lookup"
    )]
    pub fn get_interface<T: FromIBinder + ?Sized>(&self, name: &str) -> Result<Strong<T>> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_10"))]
            ServiceManager::Android10(sm) => android_10::get_interface(sm, name),
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => android_11::get_interface(sm, name),
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => android_12::get_interface(sm, name),
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => android_13::get_interface(sm, name),
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => android_14::get_interface(sm, name),
            ServiceManager::Android16(sm) => android_16::get_interface(sm, name),
        }
    }

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
            ServiceManager::Android16(sm) => android_16::is_declared(sm, name),
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
            ServiceManager::Android16(sm) => android_16::add_service(sm, identifier, binder),
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
            ServiceManager::Android16(sm) => android_16::get_service_debug_info(sm),
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
            ServiceManager::Android16(sm) => {
                android_16::unregister_for_notifications(sm, name, callback)
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
            ServiceManager::Android16(sm) => android_16::try_unregister_service(sm, name, service),
        }
    }

    /// Error-preserving, non-blocking lookup: `Ok(Some)` found, `Ok(None)` not
    /// registered, `Err` on a transport/SM failure — the distinction that
    /// [`check_service`](Self::check_service) and the deprecated
    /// [`get_service`](Self::get_service) both collapse to `None`. Reach for
    /// this when you must tell "the service isn't there" apart from "the
    /// service manager is unreachable" (e.g. to fail fast instead of retrying).
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

/// Convenience function to get an interface from the default ServiceManager.
///
/// Equivalent to `default().get_interface(name)`; see
/// [`ServiceManager::get_interface`] for its version-dependent wait behavior.
/// Use [`wait_for_interface`] to block until the service appears, or
/// [`check_interface`] for a non-blocking lookup.
#[deprecated(
    note = "inconsistent wait behavior across versions; use `wait_for_interface` \
            to block until the service appears, or `check_interface` for a \
            non-blocking lookup"
)]
#[allow(deprecated)]
#[inline]
pub fn get_interface<T: FromIBinder + ?Sized>(name: &str) -> Result<Strong<T>> {
    default()?.get_interface(name)
}

/// Convenience function to list services from the default ServiceManager.
///
/// This is equivalent to `default().list_services(dump_priority)`.
#[inline]
pub fn list_services(dump_priority: i32) -> Vec<String> {
    // Consistent with this wrapper's existing error-swallowing contract:
    // an unavailable ServiceManager yields an empty list rather than a panic.
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

/// Convenience function to get a service from the default ServiceManager.
///
/// Equivalent to `default().get_service(name)`; see
/// [`ServiceManager::get_service`] for its version-dependent wait behavior.
/// Use [`wait_for_service`] to block until the service appears, or
/// [`check_service`] for a non-blocking lookup.
#[deprecated(
    note = "inconsistent wait behavior across versions; use `wait_for_service` \
            to block until the service appears, or `check_service` for a \
            non-blocking lookup"
)]
#[allow(deprecated)]
#[inline]
pub fn get_service(name: &str) -> Option<SIBinder> {
    default().ok()?.get_service(name)
}

/// Convenience function to wait for a service from the default
/// ServiceManager.
///
/// Equivalent to `default().wait_for_service(name)`; returns `None` if the
/// default service manager is unavailable. The event-driven replacement for
/// hand-rolled client retry loops — see
/// [`ServiceManager::wait_for_service`] for the blocking and thread-pool
/// contract.
#[inline]
pub fn wait_for_service(name: &str) -> Option<SIBinder> {
    default().ok()?.wait_for_service(name)
}

/// Convenience function to wait for an interface from the default
/// ServiceManager.
///
/// Equivalent to `default().wait_for_interface(name)` — the event-driven,
/// AOSP `waitForService`-style replacement for polling around
/// [`get_interface`]. See [`ServiceManager::wait_for_service`].
#[inline]
pub fn wait_for_interface<T: FromIBinder + ?Sized>(name: &str) -> Result<Strong<T>> {
    default()?.wait_for_interface(name)
}

/// Convenience function to check if a service is available from the default ServiceManager.
///
/// This is equivalent to `default().check_service(name)`.
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
#[inline]
pub fn is_declared(name: &str) -> bool {
    default().map(|sm| sm.is_declared(name)).unwrap_or(false)
}

/// Convenience function to get debug information about all services from the default ServiceManager.
///
/// This is equivalent to `default().get_service_debug_info()`.
/// Note that this feature may not be available on all Android versions.
#[inline]
pub fn get_service_debug_info() -> Result<Vec<ServiceDebugInfo>> {
    default()?.get_service_debug_info()
}
