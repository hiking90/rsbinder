// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

mod servicemanager;
pub mod v1 {
    pub use super::servicemanager::*;
}

mod servicemanager2;
pub mod v2 {
    pub use super::servicemanager2::*;
}

use crate::*;

pub use v1::{
    DUMP_FLAG_PRIORITY_ALL,
    DUMP_FLAG_PRIORITY_CRITICAL,
    DUMP_FLAG_PRIORITY_DEFAULT,
    DUMP_FLAG_PRIORITY_HIGH,
    DUMP_FLAG_PRIORITY_NORMAL,
    IServiceCallback,
    BnServiceCallback
};

const SERVICE_MANAGER_V2_VERSION: u32 = 36;

pub fn get_interface<T: FromIBinder + ?Sized>(name: &str) -> Result<Strong<T>> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= SERVICE_MANAGER_V2_VERSION {
            v2::get_interface(name)
        } else {
            v1::get_interface(name)
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        v1::get_interface(name)
    }
}

pub fn list_services(dump_priority: i32) -> Vec<String> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= SERVICE_MANAGER_V2_VERSION {
            v2::list_services(dump_priority)
        } else {
            v1::list_services(dump_priority)
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        v1::list_services()
    }
}

pub fn register_for_notifications(
    name: &str,
    callback: &crate::Strong<dyn v1::IServiceCallback>,
) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= SERVICE_MANAGER_V2_VERSION {
            // SAFETY: This transmutation is safe because both v1::IServiceCallback and v2::IServiceCallback
            // are generated from the same AIDL interface definition and have identical memory layouts,
            // vtable structures, and ABI compatibility. The only difference is the module namespace.
            // Both traits represent the same underlying Android Binder interface contract.
            let v2_callback = unsafe {
                &*(callback as *const _ as *const crate::Strong<dyn v2::IServiceCallback>)
            };
            v2::register_for_notifications(name, v2_callback)
        } else {
            v1::register_for_notifications(name, callback)
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        v1::register_for_notifications(name, callback)
    }
}

pub fn unregister_for_notifications(
    name: &str,
    callback: &crate::Strong<dyn v1::IServiceCallback>,
) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= SERVICE_MANAGER_V2_VERSION {
            // SAFETY: This transmutation is safe because both v1::IServiceCallback and v2::IServiceCallback
            // are generated from the same AIDL interface definition and have identical memory layouts,
            // vtable structures, and ABI compatibility. The only difference is the module namespace.
            // Both traits represent the same underlying Android Binder interface contract.
            let v2_callback = unsafe {
                &*(callback as *const _ as *const crate::Strong<dyn v2::IServiceCallback>)
            };
            v2::unregister_for_notifications(name, v2_callback)
        } else {
            v1::unregister_for_notifications(name, callback)
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        v1::unregister_for_notifications(name, callback)
    }
}

pub fn add_service(
    identifier: &str,
    binder: SIBinder,
) -> std::result::Result<(), Status> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= SERVICE_MANAGER_V2_VERSION {
            println!("Call v2::add_service for identifier: {}", identifier);
            v2::add_service(identifier, binder)
        } else {
            println!("Call v1::add_service for identifier: {}", identifier);
            v1::add_service(identifier, binder)
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        v1::add_service(identifier, binder)
    }
}

pub fn get_service(
    name: &str,
) -> Option<SIBinder> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= SERVICE_MANAGER_V2_VERSION {
            v2::get_service(name).map(|s| s.service).flatten()
        } else {
            v1::get_service(name)
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        v1::get_service(name)
    }
}

pub fn check_service(
    name: &str,
) -> Option<SIBinder> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= SERVICE_MANAGER_V2_VERSION {
            v2::check_service(name).map(|s| s.service).flatten()
        } else {
            v1::check_service(name)
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        v1::check_service(name)
    }
}

pub fn is_declared(name: &str) -> bool {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= SERVICE_MANAGER_V2_VERSION {
            v2::is_declared(name)
        } else {
            v1::is_declared(name)
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        v1::is_declared(name)
    }
}

pub fn get_service_debug_info() -> Result<Vec<v1::ServiceDebugInfo>> {
    #[cfg(target_os = "android")]
    {
        if crate::get_android_sdk_version() >= SERVICE_MANAGER_V2_VERSION {
            // SAFETY: Converting v2::ServiceDebugInfo to v1::ServiceDebugInfo is safe because:
            // 1. Both types represent identical AIDL parcelable definitions (android.os.ServiceDebugInfo)
            // 2. Both have the same memory layout: name (String) + debugPid (i32)
            // 3. Both are generated from the same ServiceDebugInfo.aidl file structure
            // 4. The binary representation and field ordering are identical between v1 and v2
            // 5. There are no version-specific changes in the ServiceDebugInfo parcelable structure
            // 6. Both use the same Rust repr and derive the same traits from the AIDL compiler
            let v2_result = v2::get_service_debug_info()?;
            let v1_result: Vec<v1::ServiceDebugInfo> = unsafe {
                std::mem::transmute(v2_result)
            };
            Ok(v1_result)
        } else {
            v1::get_service_debug_info()
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        v1::get_service_debug_info()
    }
}