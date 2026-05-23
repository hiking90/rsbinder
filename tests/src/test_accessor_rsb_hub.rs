// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-14 D.8.b — cross-process accessor discovery via `rsb_hub`.
//!
//! Exercises the end-to-end path that B.6 + B.7 enable on the kernel
//! binder side:
//!
//!   1. A *separate* server process (the
//!      `example-hello/src/bin/rpc_accessor_register_interop_server`
//!      binary, originally written for the D.9 STAGE3 emulator harness
//!      — reused verbatim here) registers an `IAccessor` binder with
//!      `rsb_hub` via `hub::add_service(instance, accessor_binder)`.
//!      rsb_hub's `addService` (B.6) inspects
//!      `service.descriptor() == "android.os.IAccessor"` and stamps
//!      `is_accessor = true`.
//!   2. This test process calls `hub::get_service(instance)`.
//!      rsb_hub's `getService2` (B.7) sees the flag and returns
//!      `Service::Accessor(Some(binder))` — distinct from the regular
//!      `ServiceWithMetadata` wrap.
//!   3. The consume-side accessor arm in
//!      [`rsbinder::hub::servicemanager_16::resolve_accessor_arm`]
//!      (2-13) transparently calls `IAccessor::addConnection` → adopts
//!      the returned fd → runs the 2-8 android-13+ handshake → returns
//!      the RPC root binder.
//!   4. This test transacts `TX_ECHO` and `TX_GIVE_MARKER` against the
//!      root, asserting full Parcel-body byte parity with the server's
//!      hardcoded constants.
//!
//! The test is marked `#[ignore]` because it requires two prerequisite
//! processes (the `rsb_hub` service manager and the accessor server
//! bin) to be running on the kernel binder. The orchestration script
//! `example-hello/cpp/run_d8b_register.sh` starts both, runs this
//! test with `cargo test ... -- --ignored`, and cleans up.

// `tests/` enables rsbinder via the cumulative `android_10_plus` feature
// (`tests/Cargo.toml`), so `hub::android_16` and the consume-side
// accessor arm are always available here. The own `rpc` feature of
// `tests/` (re-exporting `rsbinder/rpc`) is the only gate we need at
// the test level; the `target_os = "linux"` gate matches the kernel-
// binder + rsb_hub dependency.
#![cfg(all(target_os = "linux", feature = "rpc"))]
#![allow(dead_code)]

use env_logger::Env;
use rsbinder::*;

/// Service-manager instance name the server bin registers under. The
/// orchestration script passes the same string as `argv[1]` to the
/// server bin. Distinct from the D.9 STAGE3 emulator instance to
/// avoid collisions when both harnesses share a binder driver.
const INSTANCE: &str = "rsbinder.test.d8b.accessor";

/// RPC root interface descriptor — must match the
/// `rpc_accessor_register_interop_server` binary's `ROOT_DESC` constant
/// so the `writeInterfaceToken` rsbinder's `RpcProxy::build_request`
/// emits is the shape the server's `Remotable::on_transact` expects.
const ROOT_DESC: &str = "rsbinder.test.accessor.IInterop";

/// `TX_ECHO` writes a String request and reads `Status + String` reply.
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
/// `TX_GIVE_MARKER` takes no args and reads `Status + String` reply.
const TX_GIVE_MARKER: TransactionCode = FIRST_CALL_TRANSACTION + 1;
/// Server-side hardcoded marker — must match the server bin's `MARKER`
/// constant. Byte-equality on this proves the Parcel reply body
/// (Status header + `writeString16` encoding) survived the consume-side
/// accessor bridge intact.
const SERVER_MARKER: &str = "stage3-from-rsbinder";

fn init_test() {
    let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info")).try_init();
    ProcessState::init_default().expect("init_default");
    ProcessState::start_thread_pool();
}

#[test]
#[ignore = "requires rsb_hub + rpc_accessor_register_interop_server processes; run via example-hello/cpp/run_d8b_register.sh"]
fn d8b_cross_process_accessor_via_rsb_hub() -> Result<()> {
    init_test();

    // 1. Discover via rsb_hub.
    //    Under the hood: kernel-binder `getService2(INSTANCE)`
    //                    → rsb_hub returns `Service::Accessor(Some(_))` (B.7)
    //                    → consume-side `resolve_accessor_arm` bridges to RPC
    //                    → returns the RPC root binder.
    let root = hub::get_service(INSTANCE).unwrap_or_else(|| {
        panic!(
            "hub::get_service({INSTANCE:?}) returned None — \
             is the accessor server bin running and has it called \
             `hub::add_service` yet? rsb_hub also needs to be up."
        )
    });

    // The accessor arm wraps the consumed RPC connection in an
    // `RpcProxy`; downcasting is the canonical way to use the
    // `build_request`/`transact` helpers without going through a
    // generated AIDL stub. Same pattern the 2-13 D.8 client uses.
    let rp = (*root)
        .as_any()
        .downcast_ref::<rsbinder::rpc::RpcProxy>()
        .expect("root binder is not an RpcProxy — accessor arm did not bridge correctly");

    // 2. TX_GIVE_MARKER — no request body. The reply body is
    //    `Status::Ok` (`writeStatusHeader`) + `writeString16(MARKER)`.
    //    Asserts the consume-side decodes both correctly.
    let marker_reply = {
        let data = rp.build_request(ROOT_DESC)?;
        rp.transact(TX_GIVE_MARKER, &data, 0)?
            .expect("TX_GIVE_MARKER returned no reply parcel")
    };
    let mut reply = marker_reply;
    let st: Status = reply.read()?;
    assert!(st.is_ok(), "TX_GIVE_MARKER non-OK status: {st}");
    let marker: String = reply.read()?;
    assert_eq!(
        marker, SERVER_MARKER,
        "TX_GIVE_MARKER mismatch — server marker String body diverged across the accessor bridge"
    );

    // 3. TX_ECHO — String request, `Status::Ok` + String reply. Pulls
    //    the *request* Parcel body through the bridge too (server reads
    //    what we write), closing the round-trip wire-byte loop.
    let req = "hello-d8b";
    let echo_reply = {
        let mut data = rp.build_request(ROOT_DESC)?;
        data.write(&req.to_string())?;
        rp.transact(TX_ECHO, &data, 0)?
            .expect("TX_ECHO returned no reply parcel")
    };
    let mut reply = echo_reply;
    let st: Status = reply.read()?;
    assert!(st.is_ok(), "TX_ECHO non-OK status: {st}");
    let echoed: String = reply.read()?;
    assert_eq!(
        echoed, req,
        "TX_ECHO round-trip mismatch — server echo diverged from input"
    );

    Ok(())
}
