// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() {
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/v1/android/os/ConnectionInfo.aidl"))
        .source(PathBuf::from("aidl/v1/android/os/IClientCallback.aidl"))
        .source(PathBuf::from("aidl/v1/android/os/IServiceCallback.aidl"))
        .source(PathBuf::from("aidl/v1/android/os/IServiceManager.aidl"))
        .source(PathBuf::from("aidl/v1/android/os/PersistableBundle.aidl"))
        .source(PathBuf::from("aidl/v1/android/os/ServiceDebugInfo.aidl"))

        .output(PathBuf::from("service_manager_v1.rs"))

        .set_crate_support(true)

        .generate().unwrap();

    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/v2/android/os/ConnectionInfo.aidl"))
        .source(PathBuf::from("aidl/v2/android/os/IAccessor.aidl"))
        .source(PathBuf::from("aidl/v2/android/os/IClientCallback.aidl"))
        .source(PathBuf::from("aidl/v2/android/os/IServiceCallback.aidl"))
        .source(PathBuf::from("aidl/v2/android/os/IServiceManager.aidl"))
        .source(PathBuf::from("aidl/v2/android/os/PersistableBundle.aidl"))
        .source(PathBuf::from("aidl/v2/android/os/Service.aidl"))
        .source(PathBuf::from("aidl/v2/android/os/ServiceDebugInfo.aidl"))
        .source(PathBuf::from("aidl/v2/android/os/ServiceWithMetadata.aidl"))

        .output(PathBuf::from("service_manager_v2.rs"))

        .set_crate_support(true)

        .generate().unwrap();
}