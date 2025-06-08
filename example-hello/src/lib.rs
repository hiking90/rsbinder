// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Include the code hello.rs generated from AIDL.
include!(concat!(env!("OUT_DIR"), "/hello.rs"));

// Set up to use the APIs provided in the code generated for Client and Service.
pub use crate::hello::IHello::*;

// Define the name of the service to be registered in the HUB(service manager).
pub const SERVICE_NAME: &str = "my.hello";
