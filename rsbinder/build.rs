// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Generates the AIDL bindings the runtime crate vendors.
//!
//! The codegen's async support mirrors the runtime crate's own `async`
//! feature (`CARGO_FEATURE_ASYNC`, set by cargo whenever the current
//! package's `async` feature is active). The emitted `IServiceManager` /
//! `IAccessor` traits would otherwise reference `crate::BoxFuture`, which
//! `rsbinder/src/lib.rs` gates on `#[cfg(feature = "async")]`: a sync-only
//! profile such as `--no-default-features --features rpc,rpc-tls,...` then
//! fails `cargo doc` / `cargo check` with "cannot find type `BoxFuture` in
//! the crate root", since rsbinder-aidl's own `async` feature (a build-dep)
//! is always on.
//!
//! `IAccessor` is compiled on its own (plan 2-13): `IServiceManager.aidl`
//! does not import it, so the `Service.accessor` union arm surfaces as an
//! unbound `IBinder`, and the accessor-bridge resolve path
//! (`hub::android_16::resolve_accessor`) needs the generated proxy to call
//! `addConnection()` / `getInstanceName()`. `ParcelFileDescriptor` is already
//! part of the runtime crate.

use std::path::PathBuf;

fn main() {
    // Codegen `async` follows the runtime crate's feature (see the module doc).
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

    // 15.0.0_r6+ (`getService2`): `hub::servicemanager_15`; r1–r5 use 14's; `hub::default` probes.
    new_builder()
        .source(PathBuf::from("aidl/15/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_15.rs"))
        .generate()
        .unwrap();

    new_builder()
        .source(PathBuf::from("aidl/16/android/os/IServiceManager.aidl"))
        .output(PathBuf::from("service_manager_16.rs"))
        .generate()
        .unwrap();

    // Not imported by `IServiceManager.aidl`, so compiled on its own (see the module doc).
    new_builder()
        .source(PathBuf::from("aidl/16/android/os/IAccessor.aidl"))
        .output(PathBuf::from("accessor_16.rs"))
        .generate()
        .unwrap();

    // Client stub for PermissionManagerService; stable across android-{11..16}, so unversioned.
    new_builder()
        .source(PathBuf::from(
            "aidl/permission/android/os/IPermissionController.aidl",
        ))
        .output(PathBuf::from("permission_controller.rs"))
        .generate()
        .unwrap();

    // Plan 10-5: unchanged since 2012, so unversioned; rsbinder implements both sides of it.
    new_builder()
        .source(PathBuf::from(
            "aidl/cancel/android/os/ICancellationSignal.aidl",
        ))
        .output(PathBuf::from("cancellation_signal.rs"))
        .generate()
        .unwrap();

    // AOSP FMQ AIDL (plans/12-fmq.md): rsbinder-aidl maps user imports to it, so built once here.
    new_builder()
        .source(PathBuf::from(
            "aidl/fmq/android/hardware/common/NativeHandle.aidl",
        ))
        .source(PathBuf::from(
            "aidl/fmq/android/hardware/common/fmq/GrantorDescriptor.aidl",
        ))
        .source(PathBuf::from(
            "aidl/fmq/android/hardware/common/fmq/MQDescriptor.aidl",
        ))
        .source(PathBuf::from(
            "aidl/fmq/android/hardware/common/fmq/SynchronizedReadWrite.aidl",
        ))
        .source(PathBuf::from(
            "aidl/fmq/android/hardware/common/fmq/UnsynchronizedWrite.aidl",
        ))
        .output(PathBuf::from("fmq.rs"))
        .generate()
        .unwrap();

    // rsbinder's own streaming AIDL (AOSP has none): plans/10-7b-streaming-over-fmq.md §2.1.
    new_builder()
        .source(PathBuf::from(
            "aidl/stream/rsbinder/stream/StreamEndpoint.aidl",
        ))
        .source(PathBuf::from(
            "aidl/stream/rsbinder/stream/IStreamSink.aidl",
        ))
        .source(PathBuf::from(
            "aidl/stream/rsbinder/stream/IStreamSource.aidl",
        ))
        .output(PathBuf::from("stream.rs"))
        .generate()
        .unwrap();
}
