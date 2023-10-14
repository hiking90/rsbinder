include!(concat!(env!("OUT_DIR"), "/service_manager.rs"));

use std::sync::{Arc, Once};
use std::sync::atomic::{AtomicBool, Ordering};

use rsbinder::*;
use android::os::IServiceManager::{IServiceManager, BpServiceManager};

static INIT: Once = Once::new();
static mut GLOBAL_SM: Option<Arc<BpServiceManager>> = None;  // Assume SM is i32 for simplicity
static IS_INIT: AtomicBool = AtomicBool::new(false);

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
pub fn get_service(name: &str) -> Option<StrongIBinder> {
    default().getService(name).unwrap()
}
