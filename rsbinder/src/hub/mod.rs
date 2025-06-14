// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, OnceLock};

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
    /// Android 12 (API level 31)
    pub const ANDROID_12: u32 = 31;
    /// Android 11 (API level 30)
    pub const ANDROID_11: u32 = 30;

    /// Minimum supported Android SDK version
    pub const MIN_SUPPORTED: u32 = ANDROID_11;
    /// Maximum supported Android SDK version
    pub const MAX_SUPPORTED: u32 = ANDROID_16;
}

pub enum ServiceManager {
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

pub fn default() -> Arc<ServiceManager> {
    static GLOBAL_SM: OnceLock<Arc<ServiceManager>> = OnceLock::new();

    GLOBAL_SM.get_or_init(|| {
        let process = ProcessState::as_self();
        let context = process.context_object()
            .expect("Failed to get context_object during ServiceManager initialization");
        #[cfg(target_os = "android")]
        let sdk_version = crate::get_android_sdk_version();

        const ERROR_MSG: &str = "Failed to create BpServiceManager from binder during ServiceManager initialization";

        #[cfg(target_os = "android")]
        let service_manager = {
            macro_rules! create_service_manager {
                ($variant:ident, $module:ident) => {
                    ServiceManager::$variant($module::BpServiceManager::from_binder(context).expect(ERROR_MSG))
                };
            }

            match sdk_version {
                sdk_versions::ANDROID_16 => create_service_manager!(Android16, android_16),
                #[cfg(feature = "android_14")]
                sdk_versions::ANDROID_14 | sdk_versions::ANDROID_15 => create_service_manager!(Android14, android_14),
                #[cfg(feature = "android_13")]
                sdk_versions::ANDROID_13 => create_service_manager!(Android13, android_13),
                #[cfg(feature = "android_12")]
                sdk_versions::ANDROID_12 => create_service_manager!(Android12, android_12),
                #[cfg(feature = "android_11")]
                sdk_versions::ANDROID_11 => create_service_manager!(Android11, android_11),
                _ => panic!("default: Unsupported Android SDK version: {}", sdk_version),
            }
        };

        #[cfg(not(target_os = "android"))]
        let service_manager = ServiceManager::Android16(android_16::BpServiceManager::from_binder(context)
            .expect(ERROR_MSG));

        Arc::new(service_manager)
    }).clone()
}

impl ServiceManager {
    pub fn get_service(&self, name: &str) -> Option<SIBinder> {
        match self {
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

    pub fn get_interface<T: FromIBinder + ?Sized>(&self, name: &str) -> Result<Strong<T>> {
        match self {
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

    pub fn check_service(&self, name: &str) -> Option<SIBinder> {
        match self {
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
    pub fn is_declared(&self, name: &str) -> bool {
        match self {
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

    pub fn list_services(&self, dump_priority: i32) -> Vec<String> {
        match self {
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

    pub fn add_service(
        &self,
        identifier: &str,
        binder: SIBinder,
    ) -> std::result::Result<(), Status> {
        match self {
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
    pub fn get_service_debug_info(&self) -> Result<Vec<ServiceDebugInfo>> {
        match self {
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
                // SAFETY: Converting android_12::ServiceDebugInfo to android_16::ServiceDebugInfo is safe because:
                // 1. Both types represent identical AIDL parcelable definitions (android.os.ServiceDebugInfo)
                // 2. Both have the same memory layout: name (String) + debugPid (i32)
                // 3. Both are generated from the same ServiceDebugInfo.aidl file structure
                let a12_result = android_12::get_service_debug_info(sm)?;
                let a16_result: Vec<ServiceDebugInfo> = unsafe { std::mem::transmute(a12_result) };
                Ok(a16_result)
            }
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                // SAFETY: Converting android_13::ServiceDebugInfo to android_16::ServiceDebugInfo is safe because:
                // 1. Both types represent identical AIDL parcelable definitions (android.os.ServiceDebugInfo)
                // 2. Both have the same memory layout: name (String) + debugPid (i32)
                // 3. Both are generated from the same ServiceDebugInfo.aidl file structure
                let a13_result = android_13::get_service_debug_info(sm)?;
                let a16_result: Vec<ServiceDebugInfo> = unsafe { std::mem::transmute(a13_result) };
                Ok(a16_result)
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                // SAFETY: Converting android_14::ServiceDebugInfo to android_16::ServiceDebugInfo is safe because:
                // 1. Both types represent identical AIDL parcelable definitions (android.os.ServiceDebugInfo)
                // 2. Both have the same memory layout: name (String) + debugPid (i32)
                // 3. Both are generated from the same ServiceDebugInfo.aidl file structure
                let a14_result = android_14::get_service_debug_info(sm)?;
                let a16_result: Vec<ServiceDebugInfo> = unsafe { std::mem::transmute(a14_result) };
                Ok(a16_result)
            }
            ServiceManager::Android16(sm) => android_16::get_service_debug_info(sm),
        }
    }

    pub fn register_for_notifications(
        &self,
        name: &str,
        callback: &crate::Strong<dyn IServiceCallback>,
    ) -> Result<()> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _
                        as *const crate::Strong<dyn android_11::IServiceCallback>)
                };
                android_11::register_for_notifications(sm, name, callback)
            }
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _
                        as *const crate::Strong<dyn android_12::IServiceCallback>)
                };
                android_12::register_for_notifications(sm, name, callback)
            }
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _
                        as *const crate::Strong<dyn android_13::IServiceCallback>)
                };
                android_13::register_for_notifications(sm, name, callback)
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _
                        as *const crate::Strong<dyn android_14::IServiceCallback>)
                };
                android_14::register_for_notifications(sm, name, callback)
            }
            ServiceManager::Android16(sm) => {
                android_16::register_for_notifications(sm, name, callback)
            }
        }
    }

    pub fn unregister_for_notifications(
        &self,
        name: &str,
        callback: &crate::Strong<dyn IServiceCallback>,
    ) -> Result<()> {
        match self {
            #[cfg(all(target_os = "android", feature = "android_11"))]
            ServiceManager::Android11(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _
                        as *const crate::Strong<dyn android_11::IServiceCallback>)
                };
                android_11::unregister_for_notifications(sm, name, callback)
            }
            #[cfg(all(target_os = "android", feature = "android_12"))]
            ServiceManager::Android12(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _
                        as *const crate::Strong<dyn android_12::IServiceCallback>)
                };
                android_12::unregister_for_notifications(sm, name, callback)
            }
            #[cfg(all(target_os = "android", feature = "android_13"))]
            ServiceManager::Android13(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _
                        as *const crate::Strong<dyn android_13::IServiceCallback>)
                };
                android_13::unregister_for_notifications(sm, name, callback)
            }
            #[cfg(all(target_os = "android", feature = "android_14"))]
            ServiceManager::Android14(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _
                        as *const crate::Strong<dyn android_14::IServiceCallback>)
                };
                android_14::unregister_for_notifications(sm, name, callback)
            }
            ServiceManager::Android16(sm) => {
                android_16::unregister_for_notifications(sm, name, callback)
            }
        }
    }
}

#[inline]
pub fn get_interface<T: FromIBinder + ?Sized>(name: &str) -> Result<Strong<T>> {
    default().get_interface(name)
}

#[inline]
pub fn list_services(dump_priority: i32) -> Vec<String> {
    default().list_services(dump_priority)
}

#[inline]
pub fn register_for_notifications(
    name: &str,
    callback: &crate::Strong<dyn IServiceCallback>,
) -> Result<()> {
    default().register_for_notifications(name, callback)
}

#[inline]
pub fn unregister_for_notifications(
    name: &str,
    callback: &crate::Strong<dyn IServiceCallback>,
) -> Result<()> {
    default().unregister_for_notifications(name, callback)
}

#[inline]
pub fn add_service(identifier: &str, binder: SIBinder) -> std::result::Result<(), Status> {
    default().add_service(identifier, binder)
}

#[inline]
pub fn get_service(name: &str) -> Option<SIBinder> {
    default().get_service(name)
}

#[inline]
pub fn check_service(name: &str) -> Option<SIBinder> {
    default().check_service(name)
}

#[inline]
pub fn is_declared(name: &str) -> bool {
    default().is_declared(name)
}

#[inline]
pub fn get_service_debug_info() -> Result<Vec<ServiceDebugInfo>> {
    default().get_service_debug_info()
}
