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
        .source(PathBuf::from(
            "aidl/android/aidl/tests/SimpleParcelable.aidl",
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

    // V1-frozen IFooInterface fixture: lets the test service register a V1 Bn
    // against a V2 client Bp. See plans/5-aosp-test-porting.md §4.
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from(
            "aidl_v1/android/aidl/versioned/tests/IFooInterface.aidl",
        ))
        .source(PathBuf::from(
            "aidl_v1/android/aidl/versioned/tests/BazUnion.aidl",
        ))
        .source(PathBuf::from(
            "aidl_v1/android/aidl/versioned/tests/Foo.aidl",
        ))
        .version(1)
        .hash("9e7be1859820c59d9d55dd133e71a3687b5d2e5b")
        .output(PathBuf::from("foo_v1.rs"))
        .generate()
        .unwrap();

    // Trunk-stable cross-version fixtures: V2 drives the client, V1-frozen the
    // service, exercising a V2↔V1 round trip. See plans/5-aosp-test-porting.md §5.
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from(
            "aidl/android/aidl/test/trunk/ITrunkStableTest.aidl",
        ))
        .version(2)
        .hash("notfrozen")
        .output(PathBuf::from("trunk_v2.rs"))
        .generate()
        .unwrap();

    rsbinder_aidl::Builder::new()
        .source(PathBuf::from(
            "aidl_v1/android/aidl/test/trunk/ITrunkStableTest.aidl",
        ))
        .version(1)
        .hash("88311b9118fb6fe9eff4a2ca19121de0587f6d5f")
        .output(PathBuf::from("trunk_v1.rs"))
        .generate()
        .unwrap();

    // Mesh integration fixture (`mesh_node` bin + `tests/mesh_rpc.rs` /
    // `tests/mesh_kernel.rs`). One AIDL package served over both kernel
    // binder and RPC. Generation is transport-agnostic, so it is emitted
    // unconditionally (the kernel-only `cargo test -p tests` build uses
    // the same module for its kernel mesh node).
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/mesh/mesh/NodeKind.aidl"))
        .source(PathBuf::from("aidl/mesh/mesh/MeshValue.aidl"))
        .source(PathBuf::from("aidl/mesh/mesh/MeshMessage.aidl"))
        .source(PathBuf::from("aidl/mesh/mesh/IMeshObserver.aidl"))
        .source(PathBuf::from("aidl/mesh/mesh/IMeshNode.aidl"))
        .output(PathBuf::from("mesh.rs"))
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

        // Plan 2-16 Phase A: an `@EnforcePermission` interface served over
        // RPC, used by `tests/rpc_enforce_permission_deny.rs` to prove
        // every guarded method denies (the RPC root-bypass close).
        rsbinder_aidl::Builder::new()
            .source(PathBuf::from("aidl/rpc_perm/IRpcPermGuard.aidl"))
            .output(PathBuf::from("rpc_perm_guard.rs"))
            .generate()
            .unwrap();

        // Plan 2-16 Phase B: a service that reports the calling identity it
        // observes inside its handler, for `tests/rpc_calling_identity.rs`.
        rsbinder_aidl::Builder::new()
            .source(PathBuf::from("aidl/rpc_perm/IRpcCaller.aidl"))
            .output(PathBuf::from("rpc_caller.rs"))
            .generate()
            .unwrap();
    }
}
