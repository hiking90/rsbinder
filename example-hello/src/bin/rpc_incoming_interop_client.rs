// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-20 STAGE3 gate (e) — rsbinder **client** with one incoming
//! (callback) connection against a real-libbinder **RPC server**
//! (`cpp/rpc_incoming_interop_launcher.cpp`) that drives the callback
//! from a plain `std::thread`, i.e. outside any handler.
//!
//! What the gate proves, on the real wire:
//!
//! 1. the INCOMING-bit attach + `"cci"` read that
//!    `RpcUnixClientConfig::incoming_connections(1)` performs is what
//!    libbinder's `RpcServer::establishConnection` expects
//!    (`addOutgoingConnection(init=true)`);
//! 2. libbinder's `ExclusiveConnection::find` from a non-handler thread
//!    picks that connection and the rsbinder incoming thread answers the
//!    twoway echo and receives the oneway notify;
//! 3. `RpcSession::shutdown` ends the incoming thread cleanly.
//!
//! Exit code 0 = PASS; non-zero = the failing step.
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rsbinder::rpc::{RpcProxy, RpcSession, RpcUnixClientConfig};
use rsbinder::*;

const ROOT_DESC: &str = "rsbinder.test.IIncomingInterop";
const CB_DESC: &str = "rsbinder.test.IIncomingInteropCallback";

const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_SCHEDULE_CALLBACK: TransactionCode = FIRST_CALL_TRANSACTION + 5;
const TX_GET_SCHED: TransactionCode = FIRST_CALL_TRANSACTION + 6;
const TX_CALLBACK_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_CALLBACK_NOTIFY: TransactionCode = FIRST_CALL_TRANSACTION + 1;

struct Cb {
    echo_seen: Arc<AtomicI32>,
    notify_seen: Arc<AtomicI32>,
}
impl Interface for Cb {}
impl Remotable for Cb {
    fn descriptor() -> &'static str {
        CB_DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            TX_CALLBACK_ECHO => {
                let s: String = reader.read()?;
                eprintln!(
                    "[rsbinder-cb] echo({s}) on {:?}",
                    std::thread::current().name()
                );
                self.echo_seen.fetch_add(1, Ordering::SeqCst);
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&format!("cb-echo:{s}"))
            }
            TX_CALLBACK_NOTIFY => {
                eprintln!("[rsbinder-cb] notify (oneway)");
                self.notify_seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn read_status(reply: &mut Parcel) -> Result<()> {
    let st: Status = reply.read()?;
    if st.is_ok() {
        Ok(())
    } else {
        Err(StatusCode::from(st))
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("rsbinder::rpc=info"),
    )
    .init();
    let sock = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/data/local/tmp/rsinc.sock".to_string());
    eprintln!("[rsbinder-client] plan 2-20 gate (e): sock={sock}");

    let session = RpcSession::setup_unix_client_android13plus_with_config(
        RpcUnixClientConfig::path(std::path::Path::new(&sock), 2).incoming_connections(1),
    )?;
    eprintln!(
        "[rsbinder-client] session up: wire v{:?}, slots={}, incoming threads={}",
        session.wire_protocol_version(),
        session.__slot_count(),
        session.__incoming_thread_count()
    );
    let root = session.get_root()?;
    let rp = (*root)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .ok_or("root is not an RpcProxy")?;

    // Sanity.
    {
        let mut d = rp.build_request(ROOT_DESC)?;
        d.write(&"sanity")?;
        let mut r = rp.transact(TX_ECHO, &d, 0)?.ok_or("echo: no reply")?;
        read_status(&mut r)?;
        let got: String = r.read()?;
        if got != "sanity" {
            eprintln!("[rsbinder-client] sanity echo mismatch: {got:?}");
            std::process::exit(10);
        }
    }
    eprintln!("[rsbinder-client] sanity OK");

    // Schedule.
    let echo_seen = Arc::new(AtomicI32::new(0));
    let notify_seen = Arc::new(AtomicI32::new(0));
    let cb: SIBinder = Interface::as_binder(&Binder::new(Cb {
        echo_seen: Arc::clone(&echo_seen),
        notify_seen: Arc::clone(&notify_seen),
    }));
    {
        let mut d = rp.build_request(ROOT_DESC)?;
        d.write(&cb)?;
        d.write(&"later")?;
        let mut r = rp
            .transact(TX_SCHEDULE_CALLBACK, &d, 0)?
            .ok_or("schedule: no reply")?;
        read_status(&mut r)?;
    }
    eprintln!("[rsbinder-client] scheduled; waiting for the server's out-of-handler callback");
    let t0 = Instant::now();
    while (echo_seen.load(Ordering::SeqCst) < 1 || notify_seen.load(Ordering::SeqCst) < 1)
        && t0.elapsed() < Duration::from_secs(3)
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    if echo_seen.load(Ordering::SeqCst) < 1 || notify_seen.load(Ordering::SeqCst) < 1 {
        eprintln!(
            "[rsbinder-client] FAIL: after {:?} echo_seen={} notify_seen={}",
            t0.elapsed(),
            echo_seen.load(Ordering::SeqCst),
            notify_seen.load(Ordering::SeqCst)
        );
        std::process::exit(11);
    }
    // The server's own view of the outcome.
    let mut outcome = String::from("pending");
    for _ in 0..100 {
        let d = rp.build_request(ROOT_DESC)?;
        let mut r = rp
            .transact(TX_GET_SCHED, &d, 0)?
            .ok_or("get_sched: no reply")?;
        read_status(&mut r)?;
        outcome = r.read()?;
        if outcome != "pending" {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if outcome != "ok:cb-echo:later" {
        eprintln!("[rsbinder-client] FAIL: server outcome {outcome:?}");
        std::process::exit(12);
    }
    eprintln!(
        "[rsbinder-client] (e) callback outside a handler round-tripped after {:?}",
        t0.elapsed()
    );

    // Teardown: the incoming thread is joined here.
    drop(root);
    session.shutdown();
    if session.__incoming_thread_count() != 0 {
        eprintln!("[rsbinder-client] FAIL: incoming thread not joined");
        std::process::exit(13);
    }
    println!("PASS — plan 2-20 (e): rsbinder incoming connection ↔ real libbinder RpcServer");
    Ok(())
}
