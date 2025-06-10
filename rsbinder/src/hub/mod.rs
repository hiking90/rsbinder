// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

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

const ANDROID_16_SDK_VERSION: u32 = 36;
const ANDROID_14_SDK_VERSION: u32 = 34;
const ANDROID_13_SDK_VERSION: u32 = 33;
const ANDROID_12_SDK_VERSION: u32 = 31;
const ANDROID_11_SDK_VERSION: u32 = 30;

pub fn get_interface<T: FromIBinder + ?Sized>(name: &str) -> Result<Strong<T>> {
    #[cfg(target_os = "android")]
    {
        let sdk_version = crate::get_android_sdk_version();
        if sdk_version >= ANDROID_16_SDK_VERSION {
            android_16::get_interface(name)
        } else if sdk_version >= ANDROID_14_SDK_VERSION {
            android_14::get_interface(name)
        } else if sdk_version >= ANDROID_13_SDK_VERSION {
            android_13::get_interface(name)
        } else if sdk_version >= ANDROID_12_SDK_VERSION {
            android_12::get_interface(name)
        } else if sdk_version >= ANDROID_11_SDK_VERSION {
            android_11::get_interface(name)
        } else {
            log::error!("get_interface: Unsupported Android SDK version: {}", sdk_version);
            Err(StatusCode::UnknownTransaction)
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        android_13::get_interface(name)
    }
}

pub fn list_services(dump_priority: i32) -> Vec<String> {
    #[cfg(target_os = "android")]
    {
        let sdk_version = crate::get_android_sdk_version();
        if sdk_version >= ANDROID_16_SDK_VERSION {
            android_16::list_services(dump_priority)
        } else if sdk_version >= ANDROID_14_SDK_VERSION {
            android_14::list_services(dump_priority)
        } else if sdk_version >= ANDROID_13_SDK_VERSION {
            android_13::list_services(dump_priority)
        } else if sdk_version >= ANDROID_12_SDK_VERSION {
            android_12::list_services(dump_priority)
        } else if sdk_version >= ANDROID_11_SDK_VERSION {
            android_11::list_services(dump_priority)
        } else {
            log::error!("list_services: Unsupported Android SDK version: {}", sdk_version);
            Vec::new()
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        android_13::list_services(dump_priority)
    }
}

pub fn register_for_notifications(
    name: &str,
    callback: &crate::Strong<dyn IServiceCallback>,
) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        let sdk_version = crate::get_android_sdk_version();
        if sdk_version >= ANDROID_16_SDK_VERSION {
            let callback = unsafe {
                &*(callback as *const _ as *const crate::Strong<dyn android_16::IServiceCallback>)
            };
            android_16::register_for_notifications(name, callback)
        } else if sdk_version >= ANDROID_14_SDK_VERSION {
            // SAFETY: This transmutation is safe because both android_13::IServiceCallback and android_16::IServiceCallback
            // are generated from the same AIDL interface definition and have identical memory layouts,
            // vtable structures, and ABI compatibility. The only difference is the module namespace.
            // Both traits represent the same underlying Android Binder interface contract.
            let callback = unsafe {
                &*(callback as *const _ as *const crate::Strong<dyn android_14::IServiceCallback>)
            };
            android_14::register_for_notifications(name, callback)
        } else if sdk_version >= ANDROID_13_SDK_VERSION {
            android_13::register_for_notifications(name, callback)
        } else if sdk_version >= ANDROID_12_SDK_VERSION {
            let callback = unsafe {
                &*(callback as *const _ as *const crate::Strong<dyn android_12::IServiceCallback>)
            };
            android_12::register_for_notifications(name, callback)
        } else if sdk_version >= ANDROID_11_SDK_VERSION {
            let callback = unsafe {
                &*(callback as *const _ as *const crate::Strong<dyn android_11::IServiceCallback>)
            };
            android_11::register_for_notifications(name, callback)
        } else {
            log::error!("register_for_notifications: Unsupported Android SDK version: {}", sdk_version);
            Err(StatusCode::UnknownTransaction)
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        android_13::register_for_notifications(name, callback)
    }
}

pub fn unregister_for_notifications(
    name: &str,
    callback: &crate::Strong<dyn IServiceCallback>,
) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= ANDROID_16_SDK_VERSION {
            // SAFETY: This transmutation is safe because both android_13::IServiceCallback and android_16::IServiceCallback
            // are generated from the same AIDL interface definition and have identical memory layouts,
            // vtable structures, and ABI compatibility. The only difference is the module namespace.
            // Both traits represent the same underlying Android Binder interface contract.
            let callback = unsafe {
                &*(callback as *const _ as *const crate::Strong<dyn android_16::IServiceCallback>)
            };
            android_16::unregister_for_notifications(name, callback)
        } else if crate::get_android_sdk_version() >= ANDROID_14_SDK_VERSION {
            let callback = unsafe {
                &*(callback as *const _ as *const crate::Strong<dyn android_14::IServiceCallback>)
            };
            android_14::unregister_for_notifications(name, callback)
        } else if crate::get_android_sdk_version() >= ANDROID_13_SDK_VERSION {
            android_13::unregister_for_notifications(name, callback)
        } else if crate::get_android_sdk_version() >= ANDROID_12_SDK_VERSION {
            let callback = unsafe {
                &*(callback as *const _ as *const crate::Strong<dyn android_12::IServiceCallback>)
            };
            android_12::unregister_for_notifications(name, callback)
        } else if crate::get_android_sdk_version() >= ANDROID_11_SDK_VERSION {
            let callback = unsafe {
                &*(callback as *const _ as *const crate::Strong<dyn android_11::IServiceCallback>)
            };
            android_11::unregister_for_notifications(name, callback)
        } else {
            log::error!("unregister_for_notifications: Unsupported Android SDK version: {}", crate::get_android_sdk_version());
            Err(StatusCode::UnknownTransaction)
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        android_13::unregister_for_notifications(name, callback)
    }
}

pub fn add_service(
    identifier: &str,
    binder: SIBinder,
) -> std::result::Result<(), Status> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= ANDROID_16_SDK_VERSION {
            android_16::add_service(identifier, binder)
        } else if crate::get_android_sdk_version() >= ANDROID_14_SDK_VERSION {
            android_14::add_service(identifier, binder)
        } else if crate::get_android_sdk_version() >= ANDROID_13_SDK_VERSION {
            android_13::add_service(identifier, binder)
        } else if crate::get_android_sdk_version() >= ANDROID_12_SDK_VERSION {
            android_12::add_service(identifier, binder)
        } else if crate::get_android_sdk_version() >= ANDROID_11_SDK_VERSION {
            android_11::add_service(identifier, binder)
        } else {
            log::error!("add_service: Unsupported Android SDK version: {}", crate::get_android_sdk_version());
            Err(StatusCode::UnknownTransaction.into())
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        android_13::add_service(identifier, binder)
    }
}

pub fn get_service(
    name: &str,
) -> Option<SIBinder> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= ANDROID_16_SDK_VERSION {
            android_16::get_service(name).and_then(|s| s.service)
        } else if crate::get_android_sdk_version() >= ANDROID_14_SDK_VERSION {
            android_14::get_service(name)
        } else if crate::get_android_sdk_version() >= ANDROID_13_SDK_VERSION {
            android_13::get_service(name)
        } else if crate::get_android_sdk_version() >= ANDROID_12_SDK_VERSION {
            android_12::get_service(name)
        } else if crate::get_android_sdk_version() >= ANDROID_11_SDK_VERSION {
            android_11::get_service(name)
        } else {
            log::error!("get_service: Unsupported Android SDK version: {}", crate::get_android_sdk_version());
            None
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        android_13::get_service(name)
    }
}

pub fn check_service(
    name: &str,
) -> Option<SIBinder> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= ANDROID_16_SDK_VERSION {
            android_16::check_service(name).and_then(|s| s.service)
        } else if crate::get_android_sdk_version() >= ANDROID_14_SDK_VERSION {
            android_14::check_service(name)
        } else if crate::get_android_sdk_version() >= ANDROID_13_SDK_VERSION {
            android_13::check_service(name)
        } else if crate::get_android_sdk_version() >= ANDROID_12_SDK_VERSION {
            android_12::check_service(name)
        } else if crate::get_android_sdk_version() >= ANDROID_11_SDK_VERSION {
            android_11::check_service(name)
        } else {
            log::error!("check_service: Unsupported Android SDK version: {}", crate::get_android_sdk_version());
            None
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        android_13::check_service(name)
    }
}

pub fn is_declared(name: &str) -> bool {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= ANDROID_16_SDK_VERSION {
            android_16::is_declared(name)
        } else if crate::get_android_sdk_version() >= ANDROID_14_SDK_VERSION {
            android_14::is_declared(name)
        } else if crate::get_android_sdk_version() >= ANDROID_13_SDK_VERSION {
            android_13::is_declared(name)
        } else if crate::get_android_sdk_version() >= ANDROID_12_SDK_VERSION {
            android_12::is_declared(name)
        } else if crate::get_android_sdk_version() >= ANDROID_11_SDK_VERSION {
            android_11::is_declared(name)
        } else {
            log::error!("is_declared: Unsupported Android SDK version: {}", crate::get_android_sdk_version());
            false
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        android_13::is_declared(name)
    }
}

pub fn get_service_debug_info() -> Result<Vec<ServiceDebugInfo>> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= ANDROID_16_SDK_VERSION {
            // SAFETY: Converting android_16::ServiceDebugInfo to android_13::ServiceDebugInfo is safe because:
            // 1. Both types represent identical AIDL parcelable definitions (android.os.ServiceDebugInfo)
            // 2. Both have the same memory layout: name (String) + debugPid (i32)
            // 3. Both are generated from the same ServiceDebugInfo.aidl file structure
            // 4. The binary representation and field ordering are identical between v1 and v2
            // 5. There are no version-specific changes in the ServiceDebugInfo parcelable structure
            // 6. Both use the same Rust repr and derive the same traits from the AIDL compiler
            let a16_result = android_16::get_service_debug_info()?;
            let a13_result: Vec<ServiceDebugInfo> = unsafe {
                std::mem::transmute(a16_result)
            };
            Ok(a13_result)
        } else if crate::get_android_sdk_version() >= ANDROID_14_SDK_VERSION {
            let a14_result = android_14::get_service_debug_info()?;
            let a13_result: Vec<ServiceDebugInfo> = unsafe {
                std::mem::transmute(a14_result)
            };
            Ok(a13_result)
        } else if crate::get_android_sdk_version() >= ANDROID_13_SDK_VERSION {
            android_13::get_service_debug_info()
        } else if crate::get_android_sdk_version() >= ANDROID_12_SDK_VERSION {
            let a12_result = android_12::get_service_debug_info()?;
            let a13_result: Vec<ServiceDebugInfo> = unsafe {
                std::mem::transmute(a12_result)
            };
            Ok(a13_result)
        } else {
            log::error!("get_service_debug_info: Unsupported Android SDK version: {}", crate::get_android_sdk_version());
            Err(StatusCode::UnknownTransaction)
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        android_13::get_service_debug_info()
    }
}