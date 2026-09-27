// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `StreamEndpoint.aidl` exists twice: `rsbinder/aidl/stream/` is what
//! rsbinder's build script compiles and what a C++ peer is given, and
//! `rsbinder-aidl/aidl/` is what `include_str!` embeds so that an
//! `import rsbinder.stream.StreamEndpoint;` resolves without the file.
//! The two must not drift apart.

use std::path::Path;

#[test]
fn stream_endpoint_aidl_copies_are_identical() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let embedded = manifest.join("aidl/rsbinder/stream/StreamEndpoint.aidl");
    let compiled = manifest.join("../rsbinder/aidl/stream/rsbinder/stream/StreamEndpoint.aidl");
    let read = |p: &Path| std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    assert!(
        read(&embedded) == read(&compiled),
        "{} and {} differ; edit both the same way",
        embedded.display(),
        compiled.display()
    );
}
