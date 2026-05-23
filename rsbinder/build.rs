// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() {
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/11/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_11.rs"))
        .set_crate_support(true)
        .generate()
        .unwrap();

    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/12/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_12.rs"))
        .set_crate_support(true)
        .generate()
        .unwrap();

    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/13/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_13.rs"))
        .set_crate_support(true)
        .generate()
        .unwrap();

    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/14/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_14.rs"))
        .set_crate_support(true)
        .generate()
        .unwrap();

    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/16/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_16.rs"))
        .set_crate_support(true)
        .generate()
        .unwrap();

    // 2-13 A0.1: `IServiceManager.aidl` does not import `IAccessor`, so
    // the `Service.accessor` union arm surfaces as an unbound `IBinder`.
    // Compile `IAccessor` separately so the accessor-bridge resolve path
    // (`hub::android_16::resolve_accessor`) can call
    // `addConnection()`/`getInstanceName()` via the generated proxy.
    // `ParcelFileDescriptor` is already vendored in the rsbinder runtime.
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/16/android/os/IAccessor.aidl"))
        .output(PathBuf::from("accessor_16.rs"))
        .set_crate_support(true)
        .generate()
        .unwrap();
}
