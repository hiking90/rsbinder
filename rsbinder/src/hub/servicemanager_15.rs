// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Android 15's service manager as it exists from `android-15.0.0_r6` on.
//!
//! The public documentation for this protocol — which builds it covers, why
//! the numbering split exists, and why nothing here parses the `Service`
//! union — is on [`hub::android_15`](super::android_15), the module these
//! items are re-exported through. This one is private, so its own docs would
//! not ship.

include!(concat!(env!("OUT_DIR"), "/service_manager_15.rs"));

crate::hub::impl_sm_module_body! { @custom_check_service
    pub use android::os::IServiceManager::FLAG_IS_LAZY_SERVICE;
    pub use android::os::ServiceDebugInfo::ServiceDebugInfo;

    /// Retrieve an existing service called @a name from the service manager.
    /// Non-blocking. Returns null if the service does not exist.
    ///
    /// Sent as `getService` (code 0), not `checkService` — both answer from
    /// the same table without blocking, and only `getService` returns a bare
    /// `@nullable IBinder` on every release in this module's range. See the
    /// module docs.
    pub fn check_service(sm: &BpServiceManager, name: &str) -> Option<SIBinder> {
        get_service(sm, name)
    }

    /// `add_service` with `FLAG_IS_LAZY_SERVICE` set, for
    /// [`LazyServiceRegistrar`](crate::lazy_service::LazyServiceRegistrar).
    /// AOSP `LazyServiceRegistrar::registerServiceLocked` ORs the flag into
    /// `dumpFlags` itself and warns if the caller pre-set it, so this is not
    /// exposed as a general `dumpFlags` parameter.
    ///
    /// AOSP added the constant in `android-15.0.0_r20`; on `r6`-`r19` the
    /// service manager stores `dumpPriority` verbatim and never reads the
    /// bit, which is inert rather than an error (its only validation is a
    /// warning when *no* `DUMP_FLAG_PRIORITY_*` bit is set, and
    /// `DUMP_FLAG_PRIORITY_DEFAULT` is).
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

#[cfg(test)]
mod tests {
    //! Pin the transaction codes this module sends. They are the whole
    //! reason it exists: `android-15.0.0_r6` shifted them and nothing in the
    //! interface records that, so a regenerate from the wrong `.aidl` — or a
    //! reordered one — would be silent everywhere else. On a real
    //! `r6`+ service manager the wrong codes reach real methods, so the
    //! failure is a `BAD_PARCELABLE` that names nothing.
    use super::android::os::IServiceManager::transactions;
    use crate::FIRST_CALL_TRANSACTION;

    #[test]
    fn transaction_codes_are_the_r6_numbering() {
        // AOSP `android-15.0.0_r6`..`r36`, `libs/binder/aidl/android/os/
        // IServiceManager.aidl` — declaration order is the numbering.
        assert_eq!(transactions::r#getService, FIRST_CALL_TRANSACTION);
        assert_eq!(transactions::r#getService2, FIRST_CALL_TRANSACTION + 1);
        assert_eq!(transactions::r#checkService, FIRST_CALL_TRANSACTION + 2);
        assert_eq!(transactions::r#addService, FIRST_CALL_TRANSACTION + 3);
        assert_eq!(transactions::r#listServices, FIRST_CALL_TRANSACTION + 4);
        assert_eq!(
            transactions::r#registerForNotifications,
            FIRST_CALL_TRANSACTION + 5
        );
        assert_eq!(
            transactions::r#unregisterForNotifications,
            FIRST_CALL_TRANSACTION + 6
        );
        assert_eq!(transactions::r#isDeclared, FIRST_CALL_TRANSACTION + 7);
        assert_eq!(
            transactions::r#getDeclaredInstances,
            FIRST_CALL_TRANSACTION + 8
        );
        assert_eq!(transactions::r#updatableViaApex, FIRST_CALL_TRANSACTION + 9);
        assert_eq!(
            transactions::r#getUpdatableNames,
            FIRST_CALL_TRANSACTION + 10
        );
        assert_eq!(
            transactions::r#getConnectionInfo,
            FIRST_CALL_TRANSACTION + 11
        );
        assert_eq!(
            transactions::r#registerClientCallback,
            FIRST_CALL_TRANSACTION + 12
        );
        assert_eq!(
            transactions::r#tryUnregisterService,
            FIRST_CALL_TRANSACTION + 13
        );
        assert_eq!(
            transactions::r#getServiceDebugInfo,
            FIRST_CALL_TRANSACTION + 14
        );
    }

    /// The probe in [`hub::default`](crate::hub::default) sends code 14 and
    /// reads the answer as "which interface is this". That only decides
    /// anything while 14 is the last method here *and* one past the last
    /// method of the Android 14 interface — the two facts this asserts.
    #[cfg(feature = "android_14")]
    #[test]
    fn probe_code_14_separates_the_two_interfaces() {
        use crate::hub::android_14::android::os::IServiceManager::transactions as prev;

        assert_eq!(
            transactions::r#getServiceDebugInfo,
            FIRST_CALL_TRANSACTION + 14,
            "code 14 must be a method here, or the probe reads `UnknownTransaction` \
             on both interfaces"
        );
        assert_eq!(
            prev::r#getServiceDebugInfo,
            FIRST_CALL_TRANSACTION + 13,
            "code 14 must be past the end of the Android 14 interface, or the probe \
             gets an answer on both"
        );

        // Every method the two share, from `checkService` on, moved up
        // exactly one. This is what makes the wrong module fail *late* and
        // opaquely rather than not at all.
        assert_eq!(transactions::r#checkService, prev::r#checkService + 1);
        assert_eq!(transactions::r#addService, prev::r#addService + 1);
        assert_eq!(
            transactions::r#registerClientCallback,
            prev::r#registerClientCallback + 1
        );
        assert_eq!(
            transactions::r#tryUnregisterService,
            prev::r#tryUnregisterService + 1
        );
        // `getService` is the one that did not move — the reason this module
        // routes every lookup through it.
        assert_eq!(transactions::r#getService, prev::r#getService);
    }
}
