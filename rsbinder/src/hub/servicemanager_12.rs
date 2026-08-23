// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

include!(concat!(env!("OUT_DIR"), "/service_manager_12.rs"));

crate::hub::impl_sm_module_body! {
    pub use android::os::ServiceDebugInfo::ServiceDebugInfo;

    pub fn get_service_debug_info(
        sm: &BpServiceManager,
    ) -> Result<Vec<ServiceDebugInfo>> {
        sm.getServiceDebugInfo().map_err(|e| e.into())
    }

    /// Every declared instance of `iface`. An interface declared as
    /// `pack.age.IFoo/foo` contributes `"foo"` when asked for
    /// `pack.age.IFoo`. An error is reported as "none declared", matching
    /// the other lookup helpers here.
    pub fn get_declared_instances(sm: &BpServiceManager, iface: &str) -> Vec<String> {
        match sm.getDeclaredInstances(iface) {
            Ok(result) => result,
            Err(err) => {
                log::error!("Failed to get_declared_instances({iface}): {err}");
                Vec::new()
            }
        }
    }
}
