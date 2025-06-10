// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

include!(concat!(env!("OUT_DIR"), "/service_manager_12.rs"));

use crate::*;
pub use android::os::IServiceManager::{
    IServiceManager, BpServiceManager, BnServiceManager,
    DUMP_FLAG_PRIORITY_CRITICAL,
    DUMP_FLAG_PRIORITY_HIGH,
    DUMP_FLAG_PRIORITY_NORMAL,
    DUMP_FLAG_PRIORITY_DEFAULT,
    DUMP_FLAG_PRIORITY_ALL,
    DUMP_FLAG_PROTO,
};

pub use android::os::IServiceCallback::{
    IServiceCallback, BnServiceCallback,
};

pub use android::os::ServiceDebugInfo::ServiceDebugInfo;

/// Retrieve an existing service, blocking for a few seconds if it doesn't yet
/// exist.
pub fn get_service(sm: &BpServiceManager, name: &str) -> Option<SIBinder> {
    match sm.getService(name) {
        Ok(result) => result,
        Err(err) => {
            log::error!("Failed to get service {}: {:?}", name, err);
            None
        }
    }
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

pub fn add_service(sm: &BpServiceManager, identifier: &str, binder: SIBinder) -> std::result::Result<(), Status> {
    sm.addService(identifier, &binder, false, DUMP_FLAG_PRIORITY_DEFAULT)
}

/// Request a callback when a service is registered.
pub fn register_for_notifications(sm: &BpServiceManager, name: &str, callback: &crate::Strong<dyn IServiceCallback>) -> Result<()> {
    sm.registerForNotifications(name, callback)
        .map_err(|e| e.into())
}

/// Unregisters all requests for notifications for a specific callback.
pub fn unregister_for_notifications(sm: &BpServiceManager, name: &str, callback: &crate::Strong<dyn IServiceCallback>) -> Result<()> {
    sm.unregisterForNotifications(name, callback)
        .map_err(|e| e.into())
}

/// Returns whether a given interface is declared on the device, even if it
/// is not started yet. For instance, this could be a service declared in the VINTF
/// manifest.
pub fn is_declared(sm: &BpServiceManager, name: &str) -> bool {
    match sm.isDeclared(name) {
        Ok(result) => result,
        Err(err) => {
            log::error!("Failed to is_declared({}): {}", name, err);
            false
        }
    }
}

pub fn get_interface<T: FromIBinder + ?Sized>(sm: &BpServiceManager, name: &str) -> Result<Strong<T>> {
    match sm.getService(name) {
        Ok(Some(service)) => {
            FromIBinder::try_from(service)
        }
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

pub fn get_service_debug_info(sm: &BpServiceManager) -> Result<Vec<android::os::ServiceDebugInfo::ServiceDebugInfo>> {
    sm.getServiceDebugInfo()
        .map_err(|e| e.into())
}