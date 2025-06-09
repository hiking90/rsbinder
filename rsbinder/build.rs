// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() {
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/11/android/os/IClientCallback.aidl"))
        .source(PathBuf::from("aidl/11/android/os/IServiceCallback.aidl"))
        .source(PathBuf::from("aidl/11/android/os/IServiceManager.aidl"))

        .output(PathBuf::from("service_manager_11.rs"))
        .set_crate_support(true)

        .generate().unwrap();

        rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/12/android/os/IClientCallback.aidl"))
        .source(PathBuf::from("aidl/12/android/os/IServiceCallback.aidl"))
        .source(PathBuf::from("aidl/12/android/os/IServiceManager.aidl"))
        .source(PathBuf::from("aidl/12/android/os/ServiceDebugInfo.aidl"))

        .output(PathBuf::from("service_manager_12.rs"))
        .set_crate_support(true)

        .generate().unwrap();

    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/13/android/os/ConnectionInfo.aidl"))
        .source(PathBuf::from("aidl/13/android/os/IClientCallback.aidl"))
        .source(PathBuf::from("aidl/13/android/os/IServiceCallback.aidl"))
        .source(PathBuf::from("aidl/13/android/os/IServiceManager.aidl"))
        .source(PathBuf::from("aidl/13/android/os/ServiceDebugInfo.aidl"))

        .output(PathBuf::from("service_manager_13.rs"))
        .set_crate_support(true)

        .generate().unwrap();

    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/14/android/os/ConnectionInfo.aidl"))
        .source(PathBuf::from("aidl/14/android/os/IClientCallback.aidl"))
        .source(PathBuf::from("aidl/14/android/os/IServiceCallback.aidl"))
        .source(PathBuf::from("aidl/14/android/os/IServiceManager.aidl"))
        .source(PathBuf::from("aidl/14/android/os/ServiceDebugInfo.aidl"))

        .output(PathBuf::from("service_manager_14.rs"))
        .set_crate_support(true)

        .generate().unwrap();

        rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/16/android/os/ConnectionInfo.aidl"))
        .source(PathBuf::from("aidl/16/android/os/IAccessor.aidl"))
        .source(PathBuf::from("aidl/16/android/os/IClientCallback.aidl"))
        .source(PathBuf::from("aidl/16/android/os/IServiceCallback.aidl"))
        .source(PathBuf::from("aidl/16/android/os/IServiceManager.aidl"))
        .source(PathBuf::from("aidl/16/android/os/PersistableBundle.aidl"))
        .source(PathBuf::from("aidl/16/android/os/Service.aidl"))
        .source(PathBuf::from("aidl/16/android/os/ServiceDebugInfo.aidl"))
        .source(PathBuf::from("aidl/16/android/os/ServiceWithMetadata.aidl"))

        .output(PathBuf::from("service_manager_16.rs"))
        .set_crate_support(true)

        .generate().unwrap();
}