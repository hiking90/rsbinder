// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! rsbinder *server* side of the IAccessor
//! **register**-side real-libbinder interop harness. The role-inverse
//! of the consume-side harness: there libbinder served the IAccessor +
//! RPC root and rsbinder consumed; here rsbinder serves both and the
//! real libbinder C launcher consumes.
//!
//! Pairs with `cpp/rpc_accessor_register_interop_launcher` (real-
//! libbinder client) on an **android-16 emulator (API 36)**. See
//! `cpp/run_rpc_accessor_register_interop.sh` for the automation around it.
//!
//! ```text
//! ANDROID_NDK_HOME=/opt/homebrew/share/android-ndk \
//!     cargo ndk -t arm64-v8a --platform 36 build -p example-hello \
//!         --features rpc,android_16 \
//!         --bin rpc_accessor_register_interop_server
//! adb -s emulator-5556 push <bin> /data/local/tmp/rsacc_reg_srv
//! adb -s emulator-5556 shell /data/local/tmp/rsacc_reg_srv \
//!     rsbinder.test.acc.reg /data/local/tmp/rsacc-reg-rpc.sock 2 [REGISTER_NAME]
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
//!      service — same shape as the consume-side launcher's, swapped sides.
//!   6. `create_accessor(instance, addr_provider)` builds the
//!      `LocalAccessor` `BnAccessor` binder; `hub::add_service` publishes
//!      it to the kernel service manager as a regular service, under the
//!      instance name or under `argv[4]`. Without a VINTF `<accessor>`
//!      entry (the stock emulator's /system is read-only) the libbinder
//!      client picks it up the same way the consume-side launcher's
//!      `AServiceManager_addService` did, then transacts `addConnection()`
//!      on it directly. With one, or with an `rsb_hub` declaration, the
//!      service manager hands it to clients of the instance.
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

/// Must match the descriptor the libbinder C launcher passes to `AIBinder_Class_define`.
const ROOT_DESC: &str = "rsbinder.test.accessor.IInterop";
/// Transaction codes — the launcher's C side hardcodes the same.
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_GIVE_MARKER: TransactionCode = FIRST_CALL_TRANSACTION + 1;
/// The libbinder client checks these bytes, so a wrong reply encoding fails the run.
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
                // Same wire as `TX_ECHO` in `rpc_accessor_interop_launcher.cpp`.
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
    // argv[3]: max RPC wire version offered; 2 is also the max of libbinder on API 36.
    let max_version: u32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    // argv[4]: the name the accessor registers under (default: the instance).
    let register_name = std::env::args().nth(4).unwrap_or_else(|| instance.clone());

    eprintln!(
        "[rsbinder-server] STAGE3 register-side: instance={instance} \
         registered as {register_name} sock={sock_path} v{max_version}"
    );

    ProcessState::init_default()?;
    // Without workers the client hangs in `addConnection()` on the accessor.
    ProcessState::start_thread_pool();

    // `LocalAccessor` connect(2)s to this listener and hands the client the connected fd.
    let _ = std::fs::remove_file(&sock_path);
    let server: Arc<RpcServer> = RpcServer::setup_unix_server(&sock_path)?;
    server.set_android13plus(max_version);
    server.set_root(Interface::as_binder(&Binder::new(Interop)))?;
    let _bg = server.run_background();

    // One instance per server, so the provider ignores the queried name.
    let path_for_provider = PathBuf::from(&sock_path);
    let addr_provider: AccessorAddrProvider =
        Box::new(move |_name: &str| Ok(AccessorSockAddr::Unix(path_for_provider.clone())));
    let accessor_binder = create_accessor(&instance, addr_provider);
    eprintln!(
        "[rsbinder-server] LocalAccessor built; descriptor={:?}",
        accessor_binder.descriptor()
    );

    // Publish under register_name; the module doc says when a service manager vends it.
    hub::add_service(&register_name, accessor_binder)?;

    eprintln!("[rsbinder-server] addService({register_name}) OK; READY (joining)");
    println!("[rsbinder-server] READY");

    // The RPC server runs on `run_background()`'s threads; this one serves the accessor.
    Ok(ProcessState::join_thread_pool()?)
}
