// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Cross-process accessor discovery via `rsb_hub`.
//!
//! Exercises the end-to-end path on the kernel binder side:
//!
//!   1. A *separate* server process (the
//!      `example-hello/src/bin/rpc_accessor_register_interop_server`
//!      binary, reused verbatim here) builds an `IAccessor` for the
//!      instance and registers it with `rsb_hub` under another name,
//!      `hub::add_service(ACCESSOR, accessor_binder)`. rsb_hub makes no
//!      call on it.
//!   2. This test process calls `hub::try_get_service(INSTANCE)`. The
//!      hub's configuration (`tests/policy/d8b-accessor.toml`) declares
//!      `INSTANCE` with `accessor = ACCESSOR`, as AOSP's VINTF
//!      `<accessor>`, so `getService2` returns
//!      `Service::Accessor(Some(binder))` — distinct from the regular
//!      `ServiceWithMetadata` wrap.
//!   3. The consume-side accessor arm in
//!      `rsbinder::hub::servicemanager_16::resolve_accessor_arm`
//!      transparently calls `IAccessor::addConnection` → adopts
//!      the returned fd → runs the android-13+ handshake → returns
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

// `android_10_plus` (tests/Cargo.toml) always brings `hub::android_16`; only rpc and rsb_hub gate.
#![cfg(all(target_os = "linux", feature = "rpc"))]
#![allow(dead_code)]

use env_logger::Env;
use rsbinder::*;

/// The server bin's `argv[1]`; distinct from the STAGE3 emulator instance on a shared driver.
const INSTANCE: &str = "rsbinder.test.d8b.IInterop/default";

/// The server bin's `argv[4]`, the name `d8b-accessor.toml` declares as `INSTANCE`'s accessor.
const ACCESSOR: &str = "android.os.IAccessor/d8b/default";

/// Equals the server bin's `ROOT_DESC`, so its `on_transact` accepts our interface token.
const ROOT_DESC: &str = "rsbinder.test.accessor.IInterop";

/// `TX_ECHO` writes a String request and reads `Status + String` reply.
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
/// `TX_GIVE_MARKER` takes no args and reads `Status + String` reply.
const TX_GIVE_MARKER: TransactionCode = FIRST_CALL_TRANSACTION + 1;
/// Equals the server bin's `MARKER`; byte equality shows the reply body crossed the bridge intact.
const SERVER_MARKER: &str = "stage3-from-rsbinder";

fn init_test() {
    let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info")).try_init();
    ProcessState::init_default().expect("init_default");
    ProcessState::start_thread_pool();
}

/// The hub hands an undeclared accessor over as is; an accessor arm fails the instance check.
#[test]
#[ignore = "requires rsb_hub + rpc_accessor_register_interop_server processes; run via example-hello/cpp/run_d8b_register.sh"]
fn d8b_an_undeclared_accessor_is_an_ordinary_service() -> Result<()> {
    init_test();

    let found = hub::try_get_service(ACCESSOR)
        .expect("service manager")
        .unwrap_or_else(|| {
            panic!("{ACCESSOR:?}: None: not registered, or vended as an accessor arm the instance check refused")
        });
    assert!(
        found.as_proxy().is_some(),
        "{ACCESSOR:?} is not a kernel proxy"
    );
    Ok(())
}

#[test]
#[ignore = "requires rsb_hub + rpc_accessor_register_interop_server processes; run via example-hello/cpp/run_d8b_register.sh"]
fn d8b_cross_process_accessor_via_rsb_hub() -> Result<()> {
    init_test();

    // 1. Discover via rsb_hub (module doc steps 2-3).
    let root = hub::try_get_service(INSTANCE)
        .expect("service manager")
        .unwrap_or_else(|| {
            panic!(
                "hub::try_get_service({INSTANCE:?}) returned None — \
                 is the accessor server bin running and has it called \
                 `hub::add_service` yet? rsb_hub also needs to be up."
            )
        });

    // The accessor arm yields an `RpcProxy`; downcasting reaches `prepare_request` without a stub.
    let rp = (*root)
        .as_any()
        .downcast_ref::<rsbinder::rpc::RpcProxy>()
        .expect("root binder is not an RpcProxy — accessor arm did not bridge correctly");

    // 2. TX_GIVE_MARKER: no request body; the reply is `Status::Ok` + `writeString16(MARKER)`.
    let marker_reply = {
        let data = rp.prepare_request(ROOT_DESC)?;
        rp.submit_transact(TX_GIVE_MARKER, &data, 0)?
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

    // 3. TX_ECHO: the request body crosses the bridge too, and comes back in the reply.
    let req = "hello-d8b";
    let echo_reply = {
        let mut data = rp.prepare_request(ROOT_DESC)?;
        data.write(&req.to_string())?;
        rp.submit_transact(TX_ECHO, &data, 0)?
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
