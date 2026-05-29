// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() {
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from(
            "aidl/android/aidl/fixedsizearray/FixedSizeArrayExample.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/nested/INestedService.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/nested/ParcelableWithNested.aidl",
        ))
        .source(PathBuf::from("aidl/android/aidl/tests/ITestService.aidl"))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/extension/ExtendableParcelable.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/extension/MyExt.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/extension/MyExt2.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/extension/MyExtLike.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/unions/EnumUnion.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/nonvintf/NonVintfExtendableParcelable.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/nonvintf/NonVintfParcelable.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/vintf/VintfExtendableParcelable.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/vintf/VintfParcelable.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/unstable/UnstableExtendableParcelable.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/unstable/UnstableParcelable.aidl",
        ))
        .source(PathBuf::from("aidl/android/aidl/tests/BackendType.aidl"))
        .source(PathBuf::from("aidl/android/aidl/tests/ByteEnum.aidl"))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/ConstantExpressionEnum.aidl",
        ))
        .source(PathBuf::from("aidl/android/aidl/tests/INamedCallback.aidl"))
        .source(PathBuf::from("aidl/android/aidl/tests/INewName.aidl"))
        .source(PathBuf::from("aidl/android/aidl/tests/IOldName.aidl"))
        .source(PathBuf::from("aidl/android/aidl/tests/IntEnum.aidl"))
        .source(PathBuf::from("aidl/android/aidl/tests/LongEnum.aidl"))
        .source(PathBuf::from("aidl/android/aidl/tests/RecursiveList.aidl"))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/StructuredParcelable.aidl",
        ))
        .source(PathBuf::from("aidl/android/aidl/tests/Union.aidl"))
        .source(PathBuf::from("aidl/android/aidl/tests/ICircular.aidl"))
        .source(PathBuf::from(
            "aidl/android/aidl/tests/CircularParcelable.aidl",
        ))
        .source(PathBuf::from(
            "aidl/android/aidl/versioned/tests/BazUnion.aidl",
        ))
        .source(PathBuf::from("aidl/android/aidl/versioned/tests/Foo.aidl"))
        .source(PathBuf::from(
            "aidl/android/aidl/versioned/tests/IFooInterface.aidl",
        ))
        // Stable-AIDL meta methods: version + frozen-API hash.
        // The hash literal matches AOSP's `aidl_api/aidl-test-versioned-interface/1/.hash`
        // (second/legacy line). Generator echoes both values verbatim — see
        // `getInterfaceVersion()`/`getInterfaceHash()` in the generated `BpFooInterface`.
        .version(1)
        .hash("9e7be1859820c59d9d55dd133e71a3687b5d2e5b")
        .source(PathBuf::from("aidl/android/aidl/tests/sm/IFoo.aidl"))
        .output(PathBuf::from("test_aidl.rs"))
        .generate()
        .unwrap();

    // Test-only fixture for the generated-stub RPC e2e
    // (`tests/rpc_generated_stub.rs`). This integration-test crate —
    // not the production `rsbinder` crate — is the home for codegen
    // that combines `rsbinder-aidl` output with the `rsbinder` runtime.
    // Gated on this crate's `rpc` feature (off by default; Cargo sets
    // CARGO_FEATURE_RPC for build scripts when it is active), so the
    // kernel-binder Android CI build (`cargo test -p tests`) generates
    // nothing. No crate_support → generated code uses `rsbinder::`
    // paths so it `include!`s into the integration test.
    if std::env::var_os("CARGO_FEATURE_RPC").is_some() {
        rsbinder_aidl::Builder::new()
            .source(PathBuf::from("aidl/rpc_smoke/IRpcSmoke.aidl"))
            .output(PathBuf::from("rpc_smoke.rs"))
            .generate()
            .unwrap();
    }
}
