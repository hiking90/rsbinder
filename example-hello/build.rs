// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() {
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/hello/IHello.aidl"))
        .output(PathBuf::from("hello.rs"))
        .generate().unwrap();
}