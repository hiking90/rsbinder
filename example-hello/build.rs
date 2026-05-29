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
}
