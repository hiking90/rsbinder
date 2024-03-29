// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

include!(concat!(env!("OUT_DIR"), "/service_manager.rs"));

use std::sync::{Arc, Once};
use std::sync::atomic::{AtomicBool, Ordering};

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

static INIT: Once = Once::new();
static mut GLOBAL_SM: Option<Arc<BpServiceManager>> = None;  // Assume SM is i32 for simplicity
static IS_INIT: AtomicBool = AtomicBool::new(false);

/// Retrieve the default service manager.
pub fn default() -> Arc<BpServiceManager> {
    unsafe {
        INIT.call_once(|| {
            let process = ProcessState::as_self();
            let service_manager = process.context_object().unwrap();
            let service_manager = BpServiceManager::from_binder(service_manager).unwrap();
            GLOBAL_SM = Some(Arc::new(service_manager));  // Replace 0 with your initial value
            IS_INIT.store(true, Ordering::SeqCst);
        });

        if IS_INIT.load(Ordering::SeqCst) {
            GLOBAL_SM.as_ref().unwrap().clone()
        } else {
            panic!("Failed to initialize GLOBAL_SM");
        }
    }
}

/// Retrieve an existing service, blocking for a few seconds if it doesn't yet
/// exist.
pub fn get_service(name: &str) -> Option<SIBinder> {
    match default().getService(name) {
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
pub fn check_service(name: &str) -> Option<SIBinder> {
    match default().checkService(name) {
        Ok(result) => result,
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
    default().registerForNotifications(name, callback).map_err(|e| e.into())
}

/// Unregisters all requests for notifications for a specific callback.
pub fn unregister_for_notifications(name: &str, callback: &crate::Strong<dyn IServiceCallback>) -> Result<()> {
    default().unregisterForNotifications(name, callback).map_err(|e| e.into())
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
    let service = get_service(name);
    match service {
        Some(service) => FromIBinder::try_from(service),
        None => Err(StatusCode::NameNotFound),
    }
}

#[cfg(test)]
mod tests {
    #![allow(non_snake_case)]

    use super::*;
    use std::sync::OnceLock;

    fn setup() {
        static INIT: OnceLock<bool> = OnceLock::new();

        let _ = INIT.get_or_init(|| {
            env_logger::init();
            crate::ProcessState::init(crate::DEFAULT_BINDER_PATH, 0);
            true
        });
    }

    #[test]
    fn test_get_check_list_service() -> crate::Result<()> {
        setup();

        #[cfg(target_os = "android")]
        {
            let manager_name = "manager";
            let binder = get_service(manager_name);
            assert!(binder.is_some());

            let binder = check_service(manager_name);
            assert!(binder.is_some());
        }

        let unknown_name = "unknown_service";
        let binder = get_service(unknown_name);
        assert!(binder.is_none());
        let binder = check_service(unknown_name);
        assert!(binder.is_none());

        let services = list_services(DUMP_FLAG_PRIORITY_DEFAULT);
        assert!(!services.is_empty());

        Ok(())
    }

    #[test]
    fn test_notifications() -> crate::Result<()> {
        setup();

        struct MyServiceCallback {}
        impl crate::Interface for MyServiceCallback {}
        impl IServiceCallback for MyServiceCallback {
            fn onRegistration(&self, name: &str, service: &crate::SIBinder) -> crate::status::Result<()> {
                println!("onRegistration: {} {:?}", name, service);
                Ok(())
            }
        }

        let callback = BnServiceCallback::new_binder(MyServiceCallback{});

        register_for_notifications("mytest_service", &callback)?;

        unregister_for_notifications("mytest_service", &callback)?;

        Ok(())
    }

    #[test]
    fn test_others() -> crate::Result<()> {
        setup();

        assert!(!is_declared("android.hardware.usb.IUsb/default"));

        Ok(())
    }
}
