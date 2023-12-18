// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

include!(concat!(env!("OUT_DIR"), "/hello.rs"));

pub use crate::hello::IHello::*;

pub const SERVICE_NAME: &str = "my.hello";