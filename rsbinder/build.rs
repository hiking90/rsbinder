// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() {
    // Mirror the *runtime* crate's `async` feature in the codegen so
    // the emitted `IServiceManager` / `IAccessor` traits don't reference
    // `crate::BoxFuture` (which itself is `#[cfg(feature = "async")]`
    // gated in `rsbinder/src/lib.rs`) when the runtime build has
    // `async` disabled — e.g. a sync-only RPC profile such as
    // `--no-default-features --features rpc,rpc-tls,...`. Without
    // this, `cargo doc` / `cargo check` under that feature combo fails
    // with "cannot find type `BoxFuture` in the crate root", since
    // rsbinder-aidl's own `async` feature (a build-dep) is always on.
    // `CARGO_FEATURE_ASYNC` is set by cargo whenever the *current
    // package*'s `async` feature is active (Cargo Book §"Build
    // Scripts" → "Environment Variables Cargo Sets").
    let async_enabled = std::env::var_os("CARGO_FEATURE_ASYNC").is_some();
    let new_builder = || {
        rsbinder_aidl::Builder::new()
            .set_crate_support(true)
            .set_async_support(async_enabled)
    };

    new_builder()
        .source(PathBuf::from("aidl/11/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_11.rs"))
        .generate()
        .unwrap();

    new_builder()
        .source(PathBuf::from("aidl/12/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_12.rs"))
        .generate()
        .unwrap();

    new_builder()
        .source(PathBuf::from("aidl/13/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_13.rs"))
        .generate()
        .unwrap();

    new_builder()
        .source(PathBuf::from("aidl/14/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_14.rs"))
        .generate()
        .unwrap();

    new_builder()
        .source(PathBuf::from("aidl/16/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_16.rs"))
        .generate()
        .unwrap();

    // 2-13 A0.1: `IServiceManager.aidl` does not import `IAccessor`, so
    // the `Service.accessor` union arm surfaces as an unbound `IBinder`.
    // Compile `IAccessor` separately so the accessor-bridge resolve path
    // (`hub::android_16::resolve_accessor`) can call
    // `addConnection()`/`getInstanceName()` via the generated proxy.
    // `ParcelFileDescriptor` is already vendored in the rsbinder runtime.
    new_builder()
        .source(PathBuf::from("aidl/16/android/os/IAccessor.aidl"))
        .output(PathBuf::from("accessor_16.rs"))
        .generate()
        .unwrap();

    // Client-side stub for system_server's
    // PermissionManagerService. Single AIDL (interface is stable across
    // android-{11..16}), so it lives outside the versioned IServiceManager
    // trees under aidl/permission/.
    new_builder()
        .source(PathBuf::from(
            "aidl/permission/android/os/IPermissionController.aidl",
        ))
        .output(PathBuf::from("permission_controller.rs"))
        .generate()
        .unwrap();
}
