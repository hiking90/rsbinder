// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, OnceLock};

#[cfg(target_os = "android")]
mod servicemanager_11;
#[cfg(target_os = "android")]
pub mod android_11 {
    pub use super::servicemanager_11::*;
}

#[cfg(target_os = "android")]
mod servicemanager_12;
#[cfg(target_os = "android")]
pub mod android_12 {
    pub use super::servicemanager_12::*;
}

mod servicemanager_13;
pub mod android_13 {
    pub use super::servicemanager_13::*;
}

#[cfg(target_os = "android")]
mod servicemanager_14;
#[cfg(target_os = "android")]
pub mod android_14 {
    pub use super::servicemanager_14::*;
}

#[cfg(target_os = "android")]
mod servicemanager_16;
#[cfg(target_os = "android")]
pub mod android_16 {
    pub use super::servicemanager_16::*;
}

use crate::*;

pub use android_13::{
    DUMP_FLAG_PRIORITY_ALL,
    DUMP_FLAG_PRIORITY_CRITICAL,
    DUMP_FLAG_PRIORITY_DEFAULT,
    DUMP_FLAG_PRIORITY_HIGH,
    DUMP_FLAG_PRIORITY_NORMAL,
    IServiceCallback,
    BnServiceCallback,
    ServiceDebugInfo
};

/// Android SDK version constants
#[cfg(target_os = "android")]
pub mod sdk_versions {
    /// Android 16 (API level 36)
    pub const ANDROID_16: u32 = 36;
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
    #[cfg(target_os = "android")]
    Android11(android_11::BpServiceManager),
    #[cfg(target_os = "android")]
    Android12(android_12::BpServiceManager),
    Android13(android_13::BpServiceManager),
    #[cfg(target_os = "android")]
    Android14(android_14::BpServiceManager),
    #[cfg(target_os = "android")]
    Android16(android_16::BpServiceManager),
}

pub fn default() -> Arc<ServiceManager> {
    static GLOBAL_SM: OnceLock<Arc<ServiceManager>> = OnceLock::new();

    GLOBAL_SM.get_or_init(|| {
        let process = ProcessState::as_self();
        let context = process.context_object()
            .expect("Failed to get context_object during ServiceManager initialization");
        let sdk_version = crate::get_android_sdk_version();

        const ERROR_MSG: &str = "Failed to create BpServiceManager from binder during ServiceManager initialization";

        #[cfg(target_os = "android")]
        let service_manager = {
            macro_rules! create_service_manager {
                ($variant:ident, $module:ident) => {
                    ServiceManager::$variant($module::BpServiceManager::from_binder(context).expect(ERROR_MSG))
                };
            }

            if sdk_version >= sdk_versions::ANDROID_16 {
                create_service_manager!(Android16, android_16)
            } else if sdk_version >= sdk_versions::ANDROID_14 {
                create_service_manager!(Android14, android_14)
            } else if sdk_version >= sdk_versions::ANDROID_13 {
                create_service_manager!(Android13, android_13)
            } else if sdk_version >= sdk_versions::ANDROID_12 {
                create_service_manager!(Android12, android_12)
            } else if sdk_version >= sdk_versions::ANDROID_11 {
                create_service_manager!(Android11, android_11)
            } else {
                panic!("default: Unsupported Android SDK version: {}", sdk_version);
            }
        };

        #[cfg(not(target_os = "android"))]
        let service_manager = ServiceManager::Android13(android_13::BpServiceManager::from_binder(context)
            .expect(ERROR_MSG));

        Arc::new(service_manager)
    }).clone()
}

impl ServiceManager {
    pub fn get_service(&self, name: &str) -> Option<SIBinder> {
        match self {
            #[cfg(target_os = "android")]
            ServiceManager::Android11(sm) => android_11::get_service(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android12(sm) => android_12::get_service(sm, name),
            ServiceManager::Android13(sm) => android_13::get_service(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android14(sm) => android_14::get_service(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android16(sm) => {
                android_16::get_service(sm, name)
                    .and_then(|s| s.service)
            }
        }
    }

    pub fn get_interface<T: FromIBinder + ?Sized>(&self, name: &str) -> Result<Strong<T>> {
        match self {
            #[cfg(target_os = "android")]
            ServiceManager::Android11(sm) => android_11::get_interface(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android12(sm) => android_12::get_interface(sm, name),
            ServiceManager::Android13(sm) => android_13::get_interface(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android14(sm) => android_14::get_interface(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android16(sm) => android_16::get_interface(sm, name),
        }
    }

    pub fn check_service(&self, name: &str) -> Option<SIBinder> {
        match self {
            #[cfg(target_os = "android")]
            ServiceManager::Android11(sm) => android_11::check_service(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android12(sm) => android_12::check_service(sm, name),
            ServiceManager::Android13(sm) => android_13::check_service(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android14(sm) => android_14::check_service(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android16(sm) => {
                android_16::check_service(sm, name)
                    .and_then(|s| s.service)
            }
        }
    }

    pub fn is_declared(&self, name: &str) -> bool {
        match self {
            #[cfg(target_os = "android")]
            ServiceManager::Android11(sm) => android_11::is_declared(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android12(sm) => android_12::is_declared(sm, name),
            ServiceManager::Android13(sm) => android_13::is_declared(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android14(sm) => android_14::is_declared(sm, name),
            #[cfg(target_os = "android")]
            ServiceManager::Android16(sm) => android_16::is_declared(sm, name),
        }
    }

    pub fn list_services(&self, dump_priority: i32) -> Vec<String> {
        match self {
            #[cfg(target_os = "android")]
            ServiceManager::Android11(sm) => android_11::list_services(sm, dump_priority),
            #[cfg(target_os = "android")]
            ServiceManager::Android12(sm) => android_12::list_services(sm, dump_priority),
            ServiceManager::Android13(sm) => android_13::list_services(sm, dump_priority),
            #[cfg(target_os = "android")]
            ServiceManager::Android14(sm) => android_14::list_services(sm, dump_priority),
            #[cfg(target_os = "android")]
            ServiceManager::Android16(sm) => android_16::list_services(sm, dump_priority),
        }
    }

    pub fn add_service(
        &self,
        identifier: &str,
        binder: SIBinder,
    ) -> std::result::Result<(), Status> {
        match self {
            #[cfg(target_os = "android")]
            ServiceManager::Android11(sm) => android_11::add_service(sm, identifier, binder),
            #[cfg(target_os = "android")]
            ServiceManager::Android12(sm) => android_12::add_service(sm, identifier, binder),
            ServiceManager::Android13(sm) => android_13::add_service(sm, identifier, binder),
            #[cfg(target_os = "android")]
            ServiceManager::Android14(sm) => android_14::add_service(sm, identifier, binder),
            #[cfg(target_os = "android")]
            ServiceManager::Android16(sm) => android_16::add_service(sm, identifier, binder),
        }
    }

    pub fn get_service_debug_info(&self) -> Result<Vec<ServiceDebugInfo>> {
        match self {
            #[cfg(target_os = "android")]
            ServiceManager::Android11(_) => {
                log::error!("get_service_debug_info: Unsupported Android SDK version: {}", crate::get_android_sdk_version());
                Err(StatusCode::UnknownTransaction)
            }
            #[cfg(target_os = "android")]
            ServiceManager::Android12(sm) => {
                // SAFETY: Converting android_12::ServiceDebugInfo to android_13::ServiceDebugInfo is safe because:
                // 1. Both types represent identical AIDL parcelable definitions (android.os.ServiceDebugInfo)
                // 2. Both have the same memory layout: name (String) + debugPid (i32)
                // 3. Both are generated from the same ServiceDebugInfo.aidl file structure
                let a12_result = android_12::get_service_debug_info(sm)?;
                let a13_result: Vec<ServiceDebugInfo> = unsafe {
                    std::mem::transmute(a12_result)
                };
                Ok(a13_result)
            }
            ServiceManager::Android13(sm) => android_13::get_service_debug_info(sm),
            #[cfg(target_os = "android")]
            ServiceManager::Android14(sm) => {
                // SAFETY: Converting android_14::ServiceDebugInfo to android_13::ServiceDebugInfo is safe because:
                // 1. Both types represent identical AIDL parcelable definitions (android.os.ServiceDebugInfo)
                // 2. Both have the same memory layout: name (String) + debugPid (i32)
                // 3. Both are generated from the same ServiceDebugInfo.aidl file structure
                let a14_result = android_14::get_service_debug_info(sm)?;
                let a13_result: Vec<ServiceDebugInfo> = unsafe {
                    std::mem::transmute(a14_result)
                };
                Ok(a13_result)
            }
            #[cfg(target_os = "android")]
            ServiceManager::Android16(sm) => {
                // SAFETY: Converting android_16::ServiceDebugInfo to android_13::ServiceDebugInfo is safe because:
                // 1. Both types represent identical AIDL parcelable definitions (android.os.ServiceDebugInfo)
                // 2. Both have the same memory layout: name (String) + debugPid (i32)
                // 3. Both are generated from the same ServiceDebugInfo.aidl file structure
                let a16_result = android_16::get_service_debug_info(sm)?;
                let a13_result: Vec<ServiceDebugInfo> = unsafe {
                    std::mem::transmute(a16_result)
                };
                Ok(a13_result)
            }
        }
    }

    pub fn register_for_notifications(
        &self,
        name: &str,
        callback: &crate::Strong<dyn IServiceCallback>,
    ) -> Result<()> {
        match self {
            #[cfg(target_os = "android")]
            ServiceManager::Android11(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _ as *const crate::Strong<dyn android_11::IServiceCallback>)
                };
                android_11::register_for_notifications(sm, name, callback)
            },
            #[cfg(target_os = "android")]
            ServiceManager::Android12(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _ as *const crate::Strong<dyn android_12::IServiceCallback>)
                };
                android_12::register_for_notifications(sm, name, callback)
            },
            ServiceManager::Android13(sm) => android_13::register_for_notifications(sm, name, callback),
            #[cfg(target_os = "android")]
            ServiceManager::Android14(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _ as *const crate::Strong<dyn android_14::IServiceCallback>)
                };
                android_14::register_for_notifications(sm, name, callback)
            },
            #[cfg(target_os = "android")]
            ServiceManager::Android16(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _ as *const crate::Strong<dyn android_16::IServiceCallback>)
                };
                android_16::register_for_notifications(sm, name, callback)
            },
        }
    }

    pub fn unregister_for_notifications(
        &self,
        name: &str,
        callback: &crate::Strong<dyn IServiceCallback>,
    ) -> Result<()> {
        match self {
            #[cfg(target_os = "android")]
            ServiceManager::Android11(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _ as *const crate::Strong<dyn android_11::IServiceCallback>)
                };
                android_11::unregister_for_notifications(sm, name, callback)
            },
            #[cfg(target_os = "android")]
            ServiceManager::Android12(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _ as *const crate::Strong<dyn android_12::IServiceCallback>)
                };
                android_12::unregister_for_notifications(sm, name, callback)
            },
            ServiceManager::Android13(sm) => android_13::unregister_for_notifications(sm, name, callback),
            #[cfg(target_os = "android")]
            ServiceManager::Android14(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _ as *const crate::Strong<dyn android_14::IServiceCallback>)
                };
                android_14::unregister_for_notifications(sm, name, callback)
            },
            #[cfg(target_os = "android")]
            ServiceManager::Android16(sm) => {
                // SAFETY: This transmutation is safe because both types represent the same AIDL interface
                let callback = unsafe {
                    &*(callback as *const _ as *const crate::Strong<dyn android_16::IServiceCallback>)
                };
                android_16::unregister_for_notifications(sm, name, callback)
            },
        }
    }
}

#[inline]
pub fn get_interface<T: FromIBinder + ?Sized>(
    name: &str,
) -> Result<Strong<T>> {
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
pub fn add_service(
    identifier: &str,
    binder: SIBinder,
) -> std::result::Result<(), Status> {
    default().add_service(identifier, binder)
}

#[inline]
pub fn get_service(
    name: &str,
) -> Option<SIBinder> {
    default().get_service(name)
}

#[inline]
pub fn check_service(
    name: &str,
) -> Option<SIBinder> {
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