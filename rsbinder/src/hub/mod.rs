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
//! // Get a service by name
//! let service = hub::get_service("example_service");
//!
//! // List all available services
//! let services = hub::list_services(hub::DUMP_FLAG_PRIORITY_ALL);
//! ```
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

mod servicemanager_16;
pub mod android_16 {
    pub use super::servicemanager_16::*;
}

use crate::*;

// Export Android 16 types as the default public API
pub use android_16::{
    BnServiceCallback, IServiceCallback, ServiceDebugInfo, DUMP_FLAG_PRIORITY_ALL,
    DUMP_FLAG_PRIORITY_CRITICAL, DUMP_FLAG_PRIORITY_DEFAULT, DUMP_FLAG_PRIORITY_HIGH,
    DUMP_FLAG_PRIORITY_NORMAL,
};

/// Android SDK version constants
#[cfg(target_os = "android")]
pub mod sdk_versions {
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
    pub const MAX_SUPPORTED: u32 = ANDROID_16;
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
            sdk_versions::ANDROID_16 => create_service_manager!(Android16, android_16),
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

impl ServiceManager {
    /// Retrieves a service by name.
    ///
    /// This method is version-agnostic and works across all supported Android versions.
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

    /// Retrieves a service by name and attempts to cast it to the specified interface type.
    ///
    /// This method is version-agnostic and works across all supported Android versions.
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
    /// This method is version-agnostic and works across all supported Android versions.
    pub fn add_service(
        &self,
        identifier: &str,
        binder: SIBinder,
    ) -> std::result::Result<(), Status> {
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
            ServiceManager::Android12(sm) => {
                let result = android_12::get_service_debug_info(sm)?;
                Ok(result
                    .into_iter()
                    .map(|info| ServiceDebugInfo {
                        name: info.name,
                        debugPid: info.debugPid,
                    })
                    .collect())
            }
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                let result = android_13::get_service_debug_info(sm)?;
                Ok(result
                    .into_iter()
                    .map(|info| ServiceDebugInfo {
                        name: info.name,
                        debugPid: info.debugPid,
                    })
                    .collect())
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                let result = android_14::get_service_debug_info(sm)?;
                Ok(result
                    .into_iter()
                    .map(|info| ServiceDebugInfo {
                        name: info.name,
                        debugPid: info.debugPid,
                    })
                    .collect())
            }
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
            ServiceManager::Android11(sm) => {
                let callback: crate::Strong<dyn android_11::IServiceCallback> =
                    crate::Strong::new(Box::new(ForwardServiceCallback(callback.as_binder())));
                android_11::register_for_notifications(sm, name, &callback)
            }
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => {
                let callback: crate::Strong<dyn android_12::IServiceCallback> =
                    crate::Strong::new(Box::new(ForwardServiceCallback(callback.as_binder())));
                android_12::register_for_notifications(sm, name, &callback)
            }
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                let callback: crate::Strong<dyn android_13::IServiceCallback> =
                    crate::Strong::new(Box::new(ForwardServiceCallback(callback.as_binder())));
                android_13::register_for_notifications(sm, name, &callback)
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                let callback: crate::Strong<dyn android_14::IServiceCallback> =
                    crate::Strong::new(Box::new(ForwardServiceCallback(callback.as_binder())));
                android_14::register_for_notifications(sm, name, &callback)
            }
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
            ServiceManager::Android11(sm) => {
                let callback: crate::Strong<dyn android_11::IServiceCallback> =
                    crate::Strong::new(Box::new(ForwardServiceCallback(callback.as_binder())));
                android_11::unregister_for_notifications(sm, name, &callback)
            }
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => {
                let callback: crate::Strong<dyn android_12::IServiceCallback> =
                    crate::Strong::new(Box::new(ForwardServiceCallback(callback.as_binder())));
                android_12::unregister_for_notifications(sm, name, &callback)
            }
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                let callback: crate::Strong<dyn android_13::IServiceCallback> =
                    crate::Strong::new(Box::new(ForwardServiceCallback(callback.as_binder())));
                android_13::unregister_for_notifications(sm, name, &callback)
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                let callback: crate::Strong<dyn android_14::IServiceCallback> =
                    crate::Strong::new(Box::new(ForwardServiceCallback(callback.as_binder())));
                android_14::unregister_for_notifications(sm, name, &callback)
            }
            ServiceManager::Android16(sm) => {
                android_16::unregister_for_notifications(sm, name, callback)
            }
        }
    }
}

//------------------------------------------------------------------------------
// Convenience Functions
//------------------------------------------------------------------------------
// The following functions provide a simpler API by using the default ServiceManager instance

/// Convenience function to get an interface from the default ServiceManager.
///
/// This is equivalent to `default().get_interface(name)`.
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

/// Convenience function to add a service to the default ServiceManager.
///
/// This is equivalent to `default().add_service(identifier, binder)`.
#[inline]
pub fn add_service(identifier: &str, binder: SIBinder) -> std::result::Result<(), Status> {
    // `?` converts a StatusCode init failure into Status via From<StatusCode>.
    default()?.add_service(identifier, binder)
}

/// Convenience function to get a service from the default ServiceManager.
///
/// This is equivalent to `default().get_service(name)`.
#[inline]
pub fn get_service(name: &str) -> Option<SIBinder> {
    default().ok()?.get_service(name)
}

/// Convenience function to check if a service is available from the default ServiceManager.
///
/// This is equivalent to `default().check_service(name)`.
#[inline]
pub fn check_service(name: &str) -> Option<SIBinder> {
    default().ok()?.check_service(name)
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
