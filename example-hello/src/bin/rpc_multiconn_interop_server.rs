// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! rsbinder side of the
//! **multi-connection-per-session** real-libbinder interop gate. Pairs
//! with `cpp/rpc_multiconn_interop_launcher.cpp` (real-libbinder
//! client wired with `ARpcSession_setMaxOutgoingConnections(2) +
//! setMaxIncomingThreads(2)`) on an **android-16 emulator (API 36)**.
//! See `cpp/run_rpc_multiconn_interop.sh` for the automation around it.
//!
//! ```text
//! ANDROID_NDK_HOME=/opt/homebrew/share/android-ndk \
//!     cargo ndk -t arm64-v8a -p 36 build -p example-hello \
//!         --features rpc --bin rpc_multiconn_interop_server
//! adb -s emulator-5556 push <bin> /data/local/tmp/rsmc_srv
//! adb -s emulator-5556 shell /data/local/tmp/rsmc_srv \
//!     /data/local/tmp/rsmc.sock 2
//! ```
//!
//! What this harness verifies (*hermetic rsbinder↔rsbinder is
//! byte-symmetric, so reorder/async-numbering defects only surface
//! against the real peer*):
//!
//!   (a) **Concurrent twoway across N=2 outgoing slots**: the launcher
//!       fires two `TX_SLOW_ECHO(80ms)` calls in parallel; both must
//!       finish in <~250ms wall (sequential would be ~160ms+ on a
//!       single slot — the launcher asserts the parallel budget so a
//!       silent serialization on one slot fails the gate).
//!
//!   (b) **Oneway in-order receipt**:
//!       20 oneway `TX_ONEWAY(i)` calls i=0..19, then one twoway
//!       `TX_GET_LOG()` reading the recorded `Vec<i32>`. Must equal
//!       `[0..20)` byte-exact. Catches a missing reorder buffer or
//!       per-node `asyncNumber`.
//!
//!   (c) **Cross-slot nested callback** (pool-traversing): the launcher
//!       registers a callback `AIBinder` and invokes
//!       `TX_INVOKE_CALLBACK(cb, "ping")` twice in parallel on slots
//!       0 and 1; the server transacts on the callback while still
//!       inside the original twoway (server→client nested call on the
//!       *same* slot via DRIVING `(sess, slot)` re-entry pin). Reply
//!       must round-trip the callback's response (`cb-echo:ping`).
//!
//! The launcher is the only side that decides "PASS" — this server
//! just exposes the transactions and lets the genuine peer drive them.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use rsbinder::rpc::{RpcProxy, RpcServer};
use rsbinder::*;

/// Must match the descriptor the C launcher's AIBinder class uses for
/// the *server root*. The genuine peer's `writeInterfaceToken` and
/// rsbinder's `consume_rpc_interface_token` agree on it bit-for-bit
/// (STAGE3 RPC token = bare `writeString16(descriptor)`).
const ROOT_DESC: &str = "rsbinder.test.IMultiConn";
/// Must match the descriptor the C launcher's *callback* AIBinder uses.
const CALLBACK_DESC: &str = "rsbinder.test.IMultiConnCallback";

const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_SLOW_ECHO: TransactionCode = FIRST_CALL_TRANSACTION + 1;
const TX_ONEWAY: TransactionCode = FIRST_CALL_TRANSACTION + 2;
const TX_GET_LOG: TransactionCode = FIRST_CALL_TRANSACTION + 3;
const TX_INVOKE_CALLBACK: TransactionCode = FIRST_CALL_TRANSACTION + 4;
/// The callback's `on_transact` code on the C launcher side.
const TX_CALLBACK_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;

struct MultiConn {
    /// Oneway log — every `TX_ONEWAY(i)` appends `i` here, in receipt
    /// order. The founding-slot pin means *all*
    /// top-level oneway calls land on the same incoming slot, so order
    /// is preserved by single-slot in-order dispatch.
    oneway_log: Mutex<Vec<i32>>,
    /// Concurrent slow-echo entry counter — used by the launcher's
    /// parallel-on-N-slots gate (the launcher asserts this hits ≥2
    /// before either slow-echo returns; this server merely exposes the
    /// observable via `TX_GET_LOG`'s negative-index probe shouldn't be
    /// needed — overlap is decided by wall-clock at the launcher).
    in_flight: AtomicI32,
}

impl Interface for MultiConn {}
impl Remotable for MultiConn {
    fn descriptor() -> &'static str {
        ROOT_DESC
    }

    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            TX_ECHO => {
                let s: String = reader.read()?;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&s)
            }
            TX_SLOW_ECHO => {
                // `(s: String, ms: i32) -> String` — sleeps `ms` then
                // echoes. The launcher uses ms≈80 across two parallel
                // calls on two slots; sequential = ~160 ms, parallel
                // = ~80 ms. Asserts parallel budget at the launcher.
                let s: String = reader.read()?;
                let ms: i32 = reader.read()?;
                let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                eprintln!("[rsbinder-server] TX_SLOW_ECHO enter (in_flight={n}) ms={ms}");
                std::thread::sleep(Duration::from_millis(ms.max(0) as u64));
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&s)
            }
            TX_ONEWAY => {
                // `(i: i32)` oneway — no reply. Append to the in-order
                // log; sliced out by TX_GET_LOG.
                let i: i32 = reader.read()?;
                self.oneway_log.lock().expect("oneway_log poisoned").push(i);
                Ok(())
            }
            TX_GET_LOG => {
                let log = self.oneway_log.lock().expect("oneway_log poisoned").clone();
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&log)
            }
            TX_INVOKE_CALLBACK => {
                // `(cb: SIBinder, s: String) -> String` — call back into
                // the client-supplied callback `cb.echo(s)` and return
                // its reply. The nested call rides the *same* slot via
                // the DRIVING `(sess, slot)` re-entry pin; cross-slot
                // routing would deadlock (slot already owned by this
                // dispatch).
                let cb: SIBinder = reader.read()?;
                let s: String = reader.read()?;
                let rp = (*cb)
                    .as_any()
                    .downcast_ref::<RpcProxy>()
                    .ok_or(StatusCode::BadType)?;
                let mut d = rp.build_request(CALLBACK_DESC)?;
                d.write(&s)?;
                let mut r = rp
                    .transact(TX_CALLBACK_ECHO, &d, 0)?
                    .ok_or(StatusCode::UnexpectedNull)?;
                let st: Status = r.read()?;
                if !st.is_ok() {
                    return Err(StatusCode::from(st));
                }
                let cb_reply: String = r.read()?;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&cb_reply)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("rsbinder::rpc=info"),
    )
    .init();

    let sock = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/data/local/tmp/rsmc.sock".to_string());
    // argv[2] = max incoming slots per session. Baseline = 2
    // (founding + 1 attached). The launcher's
    // `ARpcSession_setMaxOutgoingConnections(2)` matches.
    let max_threads: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    eprintln!("[rsbinder-server] AC-12.6 STAGE3 multi-conn: sock={sock} max_threads={max_threads}");

    let _ = std::fs::remove_file(&sock);
    let server = RpcServer::setup_unix_server(&sock)?;
    // android-16 v2 wire (matches real libbinder on the emulator).
    server.set_android13plus(2);
    // **The gate**: opt into N=2 incoming slots per session (founding
    // + 1 attached). Default 1 would silently reject the launcher's
    // second connection (the per-session cap), so this is the
    // load-bearing setting that wires the multi-conn code path.
    server.set_max_threads(max_threads);
    server.set_root(Interface::as_binder(&Binder::new(MultiConn {
        oneway_log: Mutex::new(Vec::new()),
        in_flight: AtomicI32::new(0),
    })));

    println!("[rsbinder-server] READY v2 max_threads={max_threads} on {sock}");
    // Block in the accept loop; the launcher drives every transact.
    server.run()?;
    Ok(())
}
