// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Pin the transaction codes of Android 15's two service-manager
//! numberings (see `hub::android_15` for why a regenerate from the wrong
//! `.aidl` fails only on-device, as opaque `BAD_PARCELABLE`s); the
//! generated bindings are re-included here so the pins run on every host
//! `cargo test` — the real modules are behind `target_os = "android"`
//! plus a feature no automated build turns on.

mod sm_14 {
    include!(concat!(env!("OUT_DIR"), "/service_manager_14.rs"));
}
mod sm_15 {
    include!(concat!(env!("OUT_DIR"), "/service_manager_15.rs"));
}

use crate::FIRST_CALL_TRANSACTION;
use sm_14::android::os::IServiceManager::transactions as prev;
use sm_15::android::os::IServiceManager::transactions;

#[test]
fn transaction_codes_are_the_r6_numbering() {
    // `android-15.0.0_r6`..`r36` `IServiceManager.aidl`: declaration order is the numbering.
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

/// The `hub::default` probe (code 14) needs r6+'s last method and one past Android 14's last.
#[test]
fn probe_code_14_separates_the_two_interfaces() {
    // The constant the probe sends, not a literal, so a moved probe code turns this red.
    let probe = FIRST_CALL_TRANSACTION + super::ANDROID_15_PROBE_CODE;
    assert_eq!(
        transactions::r#getServiceDebugInfo,
        probe,
        "the probe must be a method on r6+, or it reads `UnknownTransaction` \
         on both interfaces"
    );
    assert_eq!(
        prev::r#getServiceDebugInfo,
        probe - 1,
        "the probe must be one past the end of the Android 14 interface, or it \
         gets an answer on both"
    );

    // Shared methods from `checkService` on moved up one: why the wrong module fails late.
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
    // `getService` did not move: why the `android_15` module routes every lookup through it.
    assert_eq!(transactions::r#getService, prev::r#getService);
}
