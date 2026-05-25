// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

include!(concat!(env!("OUT_DIR"), "/service_manager_13.rs"));

crate::hub::impl_sm_module_body! {
    pub use android::os::ServiceDebugInfo::ServiceDebugInfo;

    pub fn get_service_debug_info(
        sm: &BpServiceManager,
    ) -> Result<Vec<ServiceDebugInfo>> {
        sm.getServiceDebugInfo().map_err(|e| e.into())
    }
}
