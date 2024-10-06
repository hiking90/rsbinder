// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() {
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/android/os/ConnectionInfo.aidl"))
        .source(PathBuf::from("aidl/android/os/IClientCallback.aidl"))
        .source(PathBuf::from("aidl/android/os/IServiceCallback.aidl"))
        .source(PathBuf::from("aidl/android/os/IServiceManager.aidl"))
        .source(PathBuf::from("aidl/android/os/PersistableBundle.aidl"))
        .source(PathBuf::from("aidl/android/os/ServiceDebugInfo.aidl"))

        .output(PathBuf::from("service_manager.rs"))

        .set_crate_support(true)

        .generate().unwrap();
}