// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Include the code hello.rs generated from AIDL.
include!(concat!(env!("OUT_DIR"), "/hello.rs"));

// Set up to use the APIs provided in the code generated for Client and Service.
pub use crate::hello::IHello::*;

// Define the name of the service to be registered in the HUB(service manager).
pub const SERVICE_NAME: &str = "my.hello";

pub fn process_with_args() {
    std::env::args().for_each(|_arg| {
        #[cfg(target_os = "android")]
        if _arg.starts_with("--android-version=") {
            let version = _arg.split('=').collect::<Vec<&str>>()[1];
            set_android_version(version.parse().expect("Invalid version"));
        }
    });
}