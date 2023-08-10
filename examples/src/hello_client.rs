include!("./rsbinder_generated.rs");

use crate::aidl::android::os::IServiceManager;
use std::env;
use env_logger::Env;
use rsbinder::*;

// include!(concat!(env!("OUT_DIR"), "/rsbinder_generated.rs"));

fn main() -> Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    let process = ProcessState::as_self();
    process.init(DEFAULT_BINDER_PATH, 0);

    let service_manager = process.context_object()?;

    let service_manager = crate::aidl::android::os::BpServiceManager::from_binder(service_manager)?;
    service_manager.get_service("vold")?; // inputflinger

    Ok(())
}