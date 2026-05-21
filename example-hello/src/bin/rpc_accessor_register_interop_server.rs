// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Subplan 2-14 D.9 STAGE3 — rsbinder *server* side of the IAccessor
//! **register**-side real-libbinder interop harness. The role-inverse
//! of 2-13 D.8: there libbinder served the IAccessor + RPC root and
//! rsbinder consumed; here rsbinder serves both and the real
//! libbinder C launcher consumes.
//!
//! Pairs with `cpp/rpc_accessor_register_interop_launcher` (real-
//! libbinder client) on an **android-16 emulator (API 36)**. See
//! `cpp/run_stage3_register.sh` for the automation around it.
//!
//! ```text
//! ANDROID_NDK_HOME=/opt/homebrew/share/android-ndk \
//!     cargo ndk -t arm64-v8a -p 36 build -p example-hello \
//!         --features rpc,android_16 \
//!         --bin rpc_accessor_register_interop_server
//! adb -s emulator-5556 push <bin> /data/local/tmp/rsacc_reg_srv
//! adb -s emulator-5556 shell /data/local/tmp/rsacc_reg_srv \
//!     rsbinder.test.acc.reg /data/local/tmp/rsacc-reg-rpc.sock 2
//! ```
//!
//! Flow:
//!   1. `ProcessState::init_default()` opens the kernel binder driver.
//!   2. `ProcessState::start_thread_pool()` spawns the binder workers
//!      so the IAccessor binder we register below can actually answer
//!      `addConnection()` calls from the libbinder client.
//!   3. `RpcServer::setup_unix_server(path)` binds + listens; the
//!      `LocalAccessor`'s addr-provider closure points back to this
//!      same UDS path, so when libbinder calls `addConnection()` the
//!      `LocalAccessor` does `connect(2)` to *our own* listener and
//!      hands the resulting client-side fd back to libbinder via PFD
//!      (AOSP `singleSocketConnection`, byte-symmetric).
//!   4. `set_android13plus(max_version)` opts into the android-13+ RPC
//!      profile; max=2 ⇒ android-16 v2 (matches real libbinder on the
//!      emulator).
//!   5. `set_root(BnInterop)` installs a tiny `TX_ECHO` / `TX_GIVE_MARKER`
//!      service — same shape as the 2-13 D.8 launcher's, swapped sides.
//!   6. `create_accessor(instance, addr_provider)` builds the
//!      `LocalAccessor` `BnAccessor` binder; `hub::add_service` publishes
//!      it to the kernel service manager as a regular service — no
//!      VINTF `<accessor>` entry needed (the stock emulator's /system is
//!      read-only). The libbinder client picks it up the same way the
//!      2-13 D.8 launcher's `AServiceManager_addService` did, then
//!      transacts `addConnection()` on it directly.
//!   7. `RpcServer::run_background()` serves the preconnected fd; the
//!      main thread joins the kernel-binder thread pool to keep
//!      servicing IAccessor calls.

use std::path::PathBuf;
use std::sync::Arc;

use rsbinder::hub::{
    self,
    android_16::{create_accessor, AccessorAddrProvider, AccessorSockAddr},
};
use rsbinder::rpc::RpcServer;
use rsbinder::*;

/// Must match the interface descriptor the libbinder C launcher
/// registers via `AIBinder_Class_define`, so its `writeInterfaceToken`
/// matches what rsbinder's RPC root expects on `on_transact`.
const ROOT_DESC: &str = "rsbinder.test.accessor.IInterop";
/// Transaction codes — the launcher's C side hardcodes the same.
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_GIVE_MARKER: TransactionCode = FIRST_CALL_TRANSACTION + 1;
/// Hard-coded server-side marker. The libbinder client asserts byte
/// equality after a full Parcel-body round trip — proving the AIDL
/// `Status::Ok` + `writeString16` reply encoding rsbinder emits is the
/// shape the real BpAccessor / NDK `AParcel_readString` decoder
/// expects.
const MARKER: &str = "stage3-from-rsbinder";

struct Interop;
impl Interface for Interop {}

impl Remotable for Interop {
    fn descriptor() -> &'static str {
        ROOT_DESC
    }

    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        eprintln!("[rsbinder-server] on_transact code={code:#x}");
        match code {
            TX_ECHO => {
                // AIDL-shaped wire: caller writes one String arg; reply
                // is `Status::Ok` + the echoed String. Mirrors the C++
                // launcher's `TX_ECHO` in 2-13 D.8 (`rpc_accessor_
                // interop_launcher.cpp`) bit-for-bit, just on the
                // opposite side of the bridge.
                let s: String = reader.read()?;
                eprintln!("[rsbinder-server] TX_ECHO arg={s:?} (from real libbinder client)");
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&s)
            }
            TX_GIVE_MARKER => {
                // No arg. Reply: `Status::Ok` + fixed marker String.
                eprintln!("[rsbinder-server] TX_GIVE_MARKER → {MARKER:?}");
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&MARKER.to_string())
            }
            _ => {
                eprintln!("[rsbinder-server] unknown txn {code:#x}");
                Err(StatusCode::UnknownTransaction)
            }
        }
    }

    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("rsbinder::rpc=info,rsbinder::hub=info"),
    )
    .init();

    let instance = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rsbinder.test.acc.reg".to_string());
    let sock_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "/data/local/tmp/rsacc-reg-rpc.sock".to_string());
    // argv[3] = max RPC_WIRE_PROTOCOL_VERSION offered (default 2 =
    // android-16 v2 object-table wire). The emulator's real libbinder
    // (API 36) has its own max=2, so negotiation picks min(2, 2) = 2.
    let max_version: u32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    eprintln!(
        "[rsbinder-server] STAGE3 register-side: instance={instance} sock={sock_path} v{max_version}"
    );

    // 1) Kernel binder driver — required for `hub::add_service` to
    //    register the IAccessor binder with the system service manager.
    ProcessState::init_default()?;
    // 2) Kernel-binder worker threads — without these, the registered
    //    IAccessor binder cannot answer transactions and the libbinder
    //    client hangs in `addConnection()` (mirrors the
    //    `ABinderProcess_startThreadPool()` call in the 2-13 D.8
    //    launcher).
    ProcessState::start_thread_pool();

    // 3) RPC server on a UDS — the listener that `LocalAccessor`
    //    `connect(2)`s into on every `addConnection()`. Both ends of
    //    the socket pair stay inside the kernel: the accepted side
    //    feeds the RPC accept loop, the connect side rides PFD to the
    //    remote client.
    let _ = std::fs::remove_file(&sock_path);
    let server: Arc<RpcServer> = RpcServer::setup_unix_server(&sock_path)?;
    server.set_android13plus(max_version);
    // 1 ⇒ libbinder opens exactly one outgoing connection per session
    // (rsbinder's single-connection model on the server arm; the
    // multi-connection thread-pool refinement is plan 2-12 territory).
    server.set_max_threads(1);
    server.set_root(Interface::as_binder(&Binder::new(Interop)));
    let _bg = server.run_background();

    // 4) IAccessor binder via `LocalAccessor` (AOSP `createAccessor`
    //    equivalent). The closure ignores the queried name — there's
    //    exactly one instance bound to this server. AOSP's
    //    `singleSocketConnection` does the same `connect(path)` here.
    let path_for_provider = PathBuf::from(&sock_path);
    let addr_provider: AccessorAddrProvider =
        Box::new(move |_name: &str| Ok(AccessorSockAddr::Unix(path_for_provider.clone())));
    let accessor_binder = create_accessor(&instance, addr_provider);
    eprintln!(
        "[rsbinder-server] LocalAccessor built; descriptor={:?}",
        accessor_binder.descriptor()
    );

    // 5) Publish to the kernel service manager as a regular service —
    //    the libbinder client looks it up via `AServiceManager_get*`
    //    and then transacts `addConnection()` on it directly. No VINTF
    //    `<accessor>` entry is needed (the stock emulator's read-only
    //    /system blocks it anyway — same constraint the 2-13 D.8
    //    launcher hits and works around).
    hub::add_service(&instance, accessor_binder)?;

    eprintln!("[rsbinder-server] addService({instance}) OK; READY (joining)");
    println!("[rsbinder-server] READY");

    // 6) Block on the kernel-binder thread pool so the IAccessor
    //    binder keeps servicing addConnection() calls. The RPC server
    //    runs on the background thread pool spawned by
    //    `run_background()`; the kernel-binder side runs here.
    Ok(ProcessState::join_thread_pool()?)
}
