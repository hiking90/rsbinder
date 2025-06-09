// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

include!(concat!(env!("OUT_DIR"), "/service_manager_16.rs"));

use std::sync::Arc;
use std::sync::OnceLock;

use crate::*;
pub use android::os::IServiceManager::{
    IServiceManager, BpServiceManager, BnServiceManager,
    DUMP_FLAG_PRIORITY_CRITICAL,
    DUMP_FLAG_PRIORITY_HIGH,
    DUMP_FLAG_PRIORITY_NORMAL,
    DUMP_FLAG_PRIORITY_DEFAULT,
    DUMP_FLAG_PRIORITY_ALL,
    DUMP_FLAG_PROTO,
    FLAG_IS_LAZY_SERVICE,
};

pub use android::os::IServiceCallback::{
    IServiceCallback, BnServiceCallback,
};

static GLOBAL_SM: OnceLock<Arc<BpServiceManager>> = OnceLock::new();

/// Retrieve the default service manager.
pub fn default() -> Arc<BpServiceManager> {
    GLOBAL_SM.get_or_init(|| {
        let process = ProcessState::as_self();
        let context = process.context_object()
            .expect("Failed to get context_object during ServiceManager initialization");
        let service_manager = BpServiceManager::from_binder(context)
            .expect("Failed to create BpServiceManager from binder during ServiceManager initialization");
        Arc::new(service_manager)
    }).clone()
}

/// Retrieve an existing service, blocking for a few seconds if it doesn't yet
/// exist.
pub fn get_service(name: &str) -> Option<android::os::ServiceWithMetadata::ServiceWithMetadata> {
    match default().getService2(name) {
        Ok(service) => {
            match service {
                android::os::Service::Service::ServiceWithMetadata(service) => Some(service),
                android::os::Service::Service::Accessor(_accessor) => {
                    log::warn!("Service {} is an Accessor, not a ServiceWithMetadata", name);
                    None
                }
            }
        }
        Err(err) => {
            log::error!("Failed to get service {}: {}", name, err);
            None
        }
    }
}

/// Retrieve an existing service called @a name from the service
/// manager. Non-blocking. Returns null if the service does not
/// exist.
pub fn check_service(name: &str) -> Option<android::os::ServiceWithMetadata::ServiceWithMetadata> {
    match default().checkService2(name) {
        Ok(service) => {
            match service {
                android::os::Service::Service::ServiceWithMetadata(service) => Some(service),
                android::os::Service::Service::Accessor(_accessor) => {
                    log::warn!("Service {} is an Accessor, not a ServiceWithMetadata", name);
                    None
                }
            }
        }
        Err(err) => {
            log::error!("Failed to check service {}: {}", name, err);
            None
        }
    }
}

/// Return a list of all currently running services.
pub fn list_services(dump_priority: i32) -> Vec<String> {
    match default().listServices(dump_priority) {
        Ok(result) => result,
        Err(err) => {
            log::error!("Failed to list services: {}", err);
            Vec::new()
        }
    }
}

pub fn add_service(identifier: &str, binder: SIBinder) -> std::result::Result<(), Status> {
    default().addService(identifier, &binder, false, DUMP_FLAG_PRIORITY_DEFAULT)
}

/// Request a callback when a service is registered.
pub fn register_for_notifications(name: &str, callback: &crate::Strong<dyn IServiceCallback>) -> Result<()> {
    default().registerForNotifications(name, callback)
        .map_err(|e| e.into())
}

/// Unregisters all requests for notifications for a specific callback.
pub fn unregister_for_notifications(name: &str, callback: &crate::Strong<dyn IServiceCallback>) -> Result<()> {
    default().unregisterForNotifications(name, callback)
        .map_err(|e| e.into())
}

/// Returns whether a given interface is declared on the device, even if it
/// is not started yet. For instance, this could be a service declared in the VINTF
/// manifest.
pub fn is_declared(name: &str) -> bool {
    match default().isDeclared(name) {
        Ok(result) => result,
        Err(err) => {
            log::error!("Failed to is_declared({}): {}", name, err);
            false
        }
    }
}

pub fn get_interface<T: FromIBinder + ?Sized>(name: &str) -> Result<Strong<T>> {
    match get_service(name) {
        Some(service) => {
            match service.service {
                Some(service) => FromIBinder::try_from(service).map_err(|e| e.into()),
                None => {
                    log::error!("Service {} is not a valid IBinder", name);
                    Err(StatusCode::NameNotFound)
                }
            }
        }
        None => {
            log::error!("Failed to get interface {}", name);
            Err(StatusCode::NameNotFound)
        }
    }
}

pub fn get_service_debug_info() -> Result<Vec<android::os::ServiceDebugInfo::ServiceDebugInfo>> {
    default().getServiceDebugInfo()
        .map_err(|e| e.into())
}