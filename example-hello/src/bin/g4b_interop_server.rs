// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! G4(b)-v0 live interop harness — the rsbinder side.
//!
//! Stands up an **android-13+ (v0) `RpcServer`** (the G4(a) opt-in
//! profile: `set_android13plus(0)`) on a Unix-domain socket and
//! publishes a trivial root object. A **real compiled android
//! libbinder** RPC client (`/system/lib64/libbinder_rpc_unstable.so`,
//! `RpcPreconnectedClient`) connects to it on the Android 13 emulator —
//! so this proves rsbinder's `Android13PlusCodec` + AOSP-faithful
//! framing + the versioned connection handshake interoperate with the
//! *genuine* AOSP RPC peer, not just hermetically (RPC_STATUS §"G4(b)").
//!
//! ```text
//! cargo ndk -t arm64-v8a build -p example-hello --features rpc \
//!     --bin g4b_interop_server
//! adb push <bin> /data/local/tmp/ && \
//!   adb shell /data/local/tmp/g4b_interop_server /data/local/tmp/g4b.sock
//! ```
//!
//! Argv[1] = socket path (default `/data/local/tmp/g4b.sock`).

use rsbinder::rpc::RpcServer;
use rsbinder::*;

/// Must match the descriptor the android client's `AIBinder_Class`
/// uses, so libbinder's `writeInterfaceToken` matches what rsbinder's
/// RPC server adapter expects (`consume_rpc_interface_token`).
const IFACE: &str = "rsbinder.g4b.IInterop";
/// `FIRST_CALL_TRANSACTION` — echo(String) -> String.
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;

struct Interop;
impl Interface for Interop {}

impl Remotable for Interop {
    fn descriptor() -> &'static str {
        IFACE
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
                // Harness wire is deliberately minimal — exactly one
                // String arg in, one String back (no AIDL `Status`
                // header) — so STAGE3 isolates the *parcel-body* +
                // interface-token interop vs the real peer, not the
                // AIDL Status convention.
                let s: String = reader.read()?;
                eprintln!("[rsbinder-server] echo({s:?}) from real android libbinder");
                reply.write(&s)?;
                Ok(())
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
        env_logger::Env::default().default_filter_or("rsbinder::rpc=debug"),
    )
    .init();

    let sock = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/data/local/tmp/g4b.sock".to_string());
    // argv[2] = max RPC_WIRE_PROTOCOL_VERSION offered: 0 = android-13
    // (v0, G4(b) STAGE1), 1 = android-14/15 (v1, subplan 2-8 D2),
    // 2 = android-16 (v2 object table, subplan 2-8 D3). libbinder
    // negotiates min(its_own_max, this); on the android-16 emulator
    // libbinder's own max is 2, so this selects the negotiated wire.
    // Default 0 keeps the original G4(b)-v0 harness behaviour.
    let max_version: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let _ = std::fs::remove_file(&sock);

    let server = RpcServer::setup_unix_server(&sock)?;
    // G4(a)/subplan-2-8 opt-in: speak the android-13+ versioned RPC
    // wire at the requested max version (v0/v1/v2).
    server.set_android13plus(max_version);
    // 1 ⇒ libbinder opens exactly one outgoing connection. rsbinder's
    // model is one-connection-per-session; android-13's
    // multiple-connections-per-session thread pool is a documented
    // future refinement (server.rs module doc) out of this v0 smoke's
    // scope, so cap the negotiation at a single connection.
    server.set_max_threads(1);
    server.set_root(Interface::as_binder(&Binder::new(Interop)));

    println!("[rsbinder-server] READY android13plus(v{max_version}) on {sock}");
    // Block in the accept loop until killed.
    server.run()?;
    Ok(())
}
