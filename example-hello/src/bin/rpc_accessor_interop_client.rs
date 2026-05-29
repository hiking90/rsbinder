// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! rsbinder side of the IAccessor bridge
//! real-libbinder interop harness.
//!
//! Pairs with `cpp/rpc_accessor_interop_launcher` (real-libbinder side)
//! on an **android-16 emulator (API 36)**.
//!
//! ```text
//! ANDROID_NDK_HOME=/opt/homebrew/share/android-ndk \
//!     cargo ndk -t arm64-v8a -p 36 build -p example-hello \
//!         --features rpc,android_16 --bin rpc_accessor_interop_client
//! adb -s emulator-5556 push <bin> /data/local/tmp/rsacc_client
//! adb -s emulator-5556 shell /data/local/tmp/rsacc_client \
//!     rsbinder.test.acc
//! ```
//!
//! Flow:
//!   1. `ProcessState::init_default()` opens kernel binder driver.
//!   2. `hub::default()` resolves the android-16 servicemanager.
//!   3. `android_16::get_service(name)` returns the
//!      `ServiceWithMetadata` arm carrying the libbinder
//!      `ABinderRpc_Accessor` binder (registered as a regular service —
//!      see launcher; the `Service::accessor` arm needs a VINTF entry
//!      which the stock emulator's read-only /system blocks).
//!   4. `accessor_16::resolve_accessor(name, accessor_binder)` drives
//!      the bridge **end-to-end**: BpAccessor wire (real libbinder) →
//!      `addConnection()` → fd adopt → v2 handshake (real libbinder
//!      RPC server peer) → `get_root()` → real RPC root.
//!   5. Full transact: `TX_ECHO` (round-trip arg) + `TX_GIVE_MARKER`
//!      (server-side string, no arg). The marker is a fixed string the
//!      C++ launcher hard-codes, so an end-to-end PASS proves the
//!      Parcel body bytes match against the genuine peer (the same
//!      shape the v2 STAGE3 interop used).
//!
//! Exit code 0 = STAGE3 PASS; non-zero = bug.

use rsbinder::*;

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("rsbinder::rpc=info,rsbinder::hub=info"),
    )
    .init();

    let instance = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rsbinder.test.acc".to_string());
    eprintln!("[rsbinder-client] STAGE3 instance={instance}");

    // Step 1: open the kernel binder driver. Required for any kernel-
    // binder traffic (Bp* proxies, servicemanager).
    ProcessState::init_default()?;

    // Step 2 + 3: ask the kernel servicemanager for the service. With
    // no VINTF `<accessor>` entry for this name (the stock emulator
    // doesn't allow VINTF write), servicemanager returns the regular
    // `serviceWithMetadata` arm whose `.service` IS the IAccessor
    // binder (the launcher registers `ABinderRpc_Accessor_asBinder` via
    // `AServiceManager_addService`). The bridge under test
    // (`accessor_16::resolve_accessor`) is independent of which arm
    // delivered the IAccessor — its job is to turn an IAccessor proxy
    // into an RPC root.
    let sm = hub::default()?;
    let swm = match &*sm {
        hub::ServiceManager::Android16(inner) => hub::android_16::get_service(inner, &instance)
            .ok_or_else(|| format!("get_service({instance}) returned None"))?,
        // `ServiceManager` only has non-`Android16` variants on
        // `target_os = "android"` with the older `android_N` features
        // enabled; this arm catches "wrong AVD" at runtime there.
        #[allow(unreachable_patterns)]
        _ => return Err("servicemanager is not Android16 — wrong AVD?".into()),
    };
    let accessor_binder = swm
        .service
        .ok_or("ServiceWithMetadata.service is None — the kernel servicemanager has no binder for this name")?;
    eprintln!(
        "[rsbinder-client] obtained IAccessor binder; descriptor={:?}",
        (*accessor_binder).descriptor()
    );

    // Step 4: drive the bridge end-to-end. `resolve_accessor` does:
    //   BpAccessor::getInstanceName() → equality check vs `instance`,
    //   BpAccessor::addConnection() → ParcelFileDescriptor,
    //   RpcSession::from_preconnected_fd(fd, max=2) → v2 handshake,
    //   session.get_root().
    let swm2 = hub::android_16::resolve_accessor(&instance, accessor_binder)
        .ok_or("resolve_accessor returned None — bridge failed (check launcher log + dmesg)")?;
    assert!(
        !swm2.r#isLazyService,
        "bridge yielded isLazyService=true (AOSP setSessionSpecificRoot is never lazy)"
    );
    let root = swm2
        .service
        .ok_or("bridge ServiceWithMetadata.service is None — get_root() yielded null")?;
    eprintln!("[rsbinder-client] RPC root acquired via bridge");

    // Step 5a: TX_ECHO("hello-stage3") — exercises Parcel body bytes
    // (rsbinder writes, real-libbinder reads, real-libbinder writes
    // reply, rsbinder reads). Asserts byte-faithfulness of the v2 wire
    // through the bridge.
    let req_str = "hello-stage3";
    let echoed: String = {
        let rp = (*root)
            .as_any()
            .downcast_ref::<rsbinder::rpc::RpcProxy>()
            .ok_or("root is not an RpcProxy (bridge wrap broke)")?;
        let mut data = rp.build_request("rsbinder.test.accessor.IInterop")?;
        data.write(&req_str.to_string())?;
        let mut reply = rp
            .transact(FIRST_CALL_TRANSACTION, &data, 0)?
            .ok_or("TX_ECHO no reply")?;
        let st: Status = reply.read()?;
        if !st.is_ok() {
            return Err(format!("TX_ECHO non-OK status: {st}").into());
        }
        reply.read::<String>()?
    };
    eprintln!("[rsbinder-client] TX_ECHO reply = {echoed:?}");
    assert_eq!(
        echoed, req_str,
        "TX_ECHO round-trip mismatch: real libbinder peer reply diverged from input"
    );

    // Step 5b: TX_GIVE_MARKER — no arg, fixed server-side string. A
    // matching reply proves the reply Parcel body is byte-correct
    // (Status header + writeString16 / read) against the real peer,
    // even with zero-byte request body.
    const EXPECTED_MARKER: &str = "stage3-from-real-libbinder";
    let marker: String = {
        let rp = (*root)
            .as_any()
            .downcast_ref::<rsbinder::rpc::RpcProxy>()
            .ok_or("root is not an RpcProxy")?;
        let data = rp.build_request("rsbinder.test.accessor.IInterop")?;
        let mut reply = rp
            .transact(FIRST_CALL_TRANSACTION + 1, &data, 0)?
            .ok_or("TX_GIVE_MARKER no reply")?;
        let st: Status = reply.read()?;
        if !st.is_ok() {
            return Err(format!("TX_GIVE_MARKER non-OK status: {st}").into());
        }
        reply.read::<String>()?
    };
    eprintln!("[rsbinder-client] TX_GIVE_MARKER reply = {marker:?}");
    assert_eq!(
        marker, EXPECTED_MARKER,
        "TX_GIVE_MARKER mismatch: bridge dropped or mangled reply bytes"
    );

    println!("STAGE3 PASS — real libbinder IAccessor + RPC server full transact");
    eprintln!(
        "[rsbinder-client] dropping root + session (DEC_STRONG + peer-side serve loop exits)"
    );
    drop(root);
    Ok(())
}
