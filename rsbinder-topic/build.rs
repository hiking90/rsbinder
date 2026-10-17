// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Generates the subscription's AIDL (`aidl/rsbinder/topic/`).
//!
//! With async support, as `rsbinder`'s `async` feature asks of generated code: `Cargo.toml` turns
//! that feature on whatever this crate's own features are, since this script cannot see what
//! other crates turn on. The `MQDescriptor` import resolves to `rsbinder::fmq::MQDescriptor`,
//! which `rsbinder-aidl` knows.
//!
//! A codegen failure prints the `miette` report and exits 1, as `rsbinder`'s `build.rs` does.

use std::path::PathBuf;

fn main() {
    let sources = [
        "aidl/rsbinder/topic/SubscribeRequest.aidl",
        "aidl/rsbinder/topic/TopicEndpoint.aidl",
        "aidl/rsbinder/topic/Pool.aidl",
        "aidl/rsbinder/topic/ISubscription.aidl",
        "aidl/rsbinder/topic/ITopic.aidl",
    ];
    let builder = sources.iter().fold(
        rsbinder_aidl::Builder::new().set_async_support(true),
        |builder, source| builder.source(PathBuf::from(source)),
    );
    if let Err(err) = builder.output(PathBuf::from("topic.rs")).generate() {
        eprintln!("generating topic.rs: {:?}", miette::Report::new(err));
        std::process::exit(1);
    }
}
