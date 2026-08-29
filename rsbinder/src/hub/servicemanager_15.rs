// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Android 15's service manager as it exists from `android-15.0.0_r6` on.
//!
//! The public documentation for this protocol — which builds it covers, why
//! the numbering split exists, and why nothing here parses the `Service`
//! union — is on [`hub::android_15`](super::android_15), the module these
//! items are re-exported through. This one is private, so its own docs would
//! not ship. The transaction-code pins live in `hub/numbering_pins.rs`,
//! which runs on every host `cargo test` (this module never does — it is
//! behind `target_os = "android"`).

include!(concat!(env!("OUT_DIR"), "/service_manager_15.rs"));

crate::hub::impl_sm_module_body! { @custom_check_service
    pub use android::os::IServiceManager::FLAG_IS_LAZY_SERVICE;
    pub use android::os::ServiceDebugInfo::ServiceDebugInfo;

    /// Retrieve an existing service called @a name from the service manager.
    /// Non-blocking. Returns null if the service does not exist.
    ///
    /// Sent as `getService` (code 0), not `checkService`: only `getService`
    /// returns a bare `@nullable IBinder` on every release in this module's
    /// range (see the module docs). Both answer from the same table without
    /// blocking, but they are **not** side-effect free in the same way — for
    /// a name that is not registered, AOSP's servicemanager runs
    /// `tryStartService` (`ctl.interface_start aidl/<name>`) on `getService`
    /// and not on `checkService`. On Android 15 r6+ a `check_service` for an
    /// unregistered *lazy* service therefore starts it, where every other
    /// Android version leaves it alone.
    pub fn check_service(sm: &BpServiceManager, name: &str) -> Option<SIBinder> {
        get_service(sm, name)
    }

    /// `add_service` + `FLAG_IS_LAZY_SERVICE`; `r6`–`r19` predate the constant and store the bit inert.
    pub(crate) fn add_lazy_service(
        sm: &BpServiceManager,
        identifier: &str,
        binder: SIBinder,
    ) -> std::result::Result<(), Status> {
        sm.addService(
            identifier,
            &binder,
            false,
            DUMP_FLAG_PRIORITY_DEFAULT | FLAG_IS_LAZY_SERVICE,
        )
    }

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

    /// Connection info declared for `name`, if any.
    pub fn get_connection_info(
        sm: &BpServiceManager,
        name: &str,
    ) -> Option<android::os::ConnectionInfo::ConnectionInfo> {
        match sm.getConnectionInfo(name) {
            Ok(result) => result,
            Err(err) => {
                log::error!("Failed to get_connection_info({name}): {err}");
                None
            }
        }
    }
}
