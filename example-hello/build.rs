// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() {
    let aidl = |src: &str, out: &str| {
        rsbinder_aidl::Builder::new()
            .source(PathBuf::from(src))
            .output(PathBuf::from(out))
            .generate()
            .unwrap();
    };

    aidl("aidl/hello/IHello.aidl", "hello.rs");
    // Calling-identity interop fixture interface.
    aidl(
        "aidl/calling_identity/ICallingIdentity.aidl",
        "calling_identity.rs",
    );
    // @EnforcePermission codegen E2E fixture.
    aidl("aidl/permcheck/IPermCheck.aidl", "permcheck.rs");
    // `TF_UPDATE_TXN` dedup fixture.
    aidl("aidl/update_txn/IUpdateTxnDedup.aidl", "update_txn.rs");
    // RT inheritance fixture.
    aidl("aidl/rt_inherit/IRtCheck.aidl", "rt_inherit.rs");
    // Plan 2-16 handler-side authorization example interface.
    aidl("aidl/authz/IAuthz.aidl", "authz.rs");
    // Shared-memory example interface (`bin/shm_{service,client}`).
    aidl("aidl/shm/IShm.aidl", "shm.rs");
    // Argument shapes whose generated Rust changed with the 2026-08 AIDL
    // review. One source of truth with `tests/aidl/shapes/`: the tests crate
    // round-trips them rsbinder-to-rsbinder, `cpp/codegen_shapes_interop.cpp`
    // cross-checks the same wire against real libbinder.
    aidl(
        "../tests/aidl/shapes/ICodegenShapes.aidl",
        "codegen_shapes.rs",
    );
}
