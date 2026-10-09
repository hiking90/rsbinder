// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! rsbinder server half of the r34 wire STAGE3 gate (plan 2-25 Phase 7,
//! gate A): a real android-12 libbinder client
//! (`cpp/rpc_r34_interop.cpp client`) against an rsbinder `RpcServer` on the
//! default r34 profile, `set_max_threads(3)`, on an SDK 31 emulator. Run it
//! through `cpp/run_rpc_r34_interop.sh`.
//!
//! ```text
//! rpc_r34_interop_server <sock> <noroot_sock> <max_threads>
//! ```
//!
//! `<sock>` serves the root below. `<noroot_sock>` is a second server with
//! no root object, for the client's null `GET_ROOT` check. The client
//! decides PASS; this side only answers and reports its counters through
//! `TX_STATS`.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rsbinder::rpc::{RpcProxy, RpcServer};
use rsbinder::*;

// Must match `cpp/rpc_r34_interop.cpp`.
const ROOT_DESC: &str = "rsbinder.test.IR34Interop";
const CALLBACK_DESC: &str = "rsbinder.test.IR34Callback";
const CHILD_DESC: &str = "rsbinder.test.IR34Child";

const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_SLOW_ECHO: TransactionCode = FIRST_CALL_TRANSACTION + 1;
const TX_ONEWAY: TransactionCode = FIRST_CALL_TRANSACTION + 2;
const TX_GET_LOG: TransactionCode = FIRST_CALL_TRANSACTION + 3;
const TX_INVOKE_CALLBACK: TransactionCode = FIRST_CALL_TRANSACTION + 4;
const TX_NULL_BINDER: TransactionCode = FIRST_CALL_TRANSACTION + 5;
const TX_HOLD: TransactionCode = FIRST_CALL_TRANSACTION + 6;
const TX_RELEASE: TransactionCode = FIRST_CALL_TRANSACTION + 7;
const TX_ECHO_BINDER: TransactionCode = FIRST_CALL_TRANSACTION + 8;
const TX_GET_CHILD: TransactionCode = FIRST_CALL_TRANSACTION + 9;
const TX_STATS: TransactionCode = FIRST_CALL_TRANSACTION + 10;
const TX_BYTES: TransactionCode = FIRST_CALL_TRANSACTION + 11;
const TX_ONEWAY_BINDER: TransactionCode = FIRST_CALL_TRANSACTION + 12;
const TX_CB_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_CHILD_PING: TransactionCode = FIRST_CALL_TRANSACTION;

struct Child;

impl Interface for Child {}
impl Remotable for Child {
    fn descriptor() -> &'static str {
        CHILD_DESC
    }

    fn on_transact(&self, code: TransactionCode, _: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        match code {
            TX_CHILD_PING => {
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&7i32)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
        Ok(())
    }
}

struct R34Interop {
    server: Weak<RpcServer>,
    oneway_log: Mutex<Vec<i32>>,
    held: Mutex<Vec<SIBinder>>,
    child: Mutex<Option<SIBinder>>,
    in_flight: AtomicI32,
}

impl R34Interop {
    fn stats(&self) -> [i32; 4] {
        let Some(server) = self.server.upgrade() else {
            return [-1; 4];
        };
        [
            server.live_session_node_count(),
            server.session_registered_count(),
            server.attached_count(),
            server.rejected_unknown_id_count(),
        ]
        .map(|n| i32::try_from(n).unwrap_or(i32::MAX))
    }
}

fn ok(reply: &mut Parcel) -> Result<()> {
    reply.write(&Status::from(StatusCode::Ok))
}

impl Interface for R34Interop {}
impl Remotable for R34Interop {
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
                ok(reply)?;
                reply.write(&s)
            }
            TX_SLOW_ECHO => {
                let s: String = reader.read()?;
                let ms: i32 = reader.read()?;
                let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                if ms >= 100 {
                    eprintln!("[rsbinder-server] TX_SLOW_ECHO {s} in_flight={n} ms={ms}");
                }
                std::thread::sleep(Duration::from_millis(ms.max(0) as u64));
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                ok(reply)?;
                reply.write(&s)
            }
            TX_ONEWAY => {
                let i: i32 = reader.read()?;
                self.oneway_log.lock().expect("oneway_log poisoned").push(i);
                Ok(())
            }
            TX_GET_LOG => {
                let log = self.oneway_log.lock().expect("oneway_log poisoned").clone();
                ok(reply)?;
                reply.write(&log)
            }
            TX_INVOKE_CALLBACK => {
                let cb: SIBinder = reader.read()?;
                let s: String = reader.read()?;
                let rp = (*cb)
                    .as_any()
                    .downcast_ref::<RpcProxy>()
                    .ok_or(StatusCode::BadType)?;
                let mut d = rp.build_request(CALLBACK_DESC)?;
                d.write(&s)?;
                let mut r = rp
                    .transact(TX_CB_ECHO, &d, 0)?
                    .ok_or(StatusCode::UnexpectedNull)?;
                let st: Status = r.read()?;
                if !st.is_ok() {
                    return Err(StatusCode::from(st));
                }
                let echoed: String = r.read()?;
                ok(reply)?;
                reply.write(&echoed)
            }
            TX_NULL_BINDER => {
                let b: Option<SIBinder> = reader.read()?;
                let x: i32 = reader.read()?;
                ok(reply)?;
                reply.write(&i32::from(b.is_none()))?;
                reply.write(&Option::<SIBinder>::None)?;
                reply.write(&(x + 1))
            }
            TX_HOLD => {
                let b: SIBinder = reader.read()?;
                self.held.lock().expect("held poisoned").push(b);
                ok(reply)
            }
            TX_RELEASE => {
                let held = std::mem::take(&mut *self.held.lock().expect("held poisoned"));
                let n = held.len() as i32;
                drop(held);
                ok(reply)?;
                reply.write(&n)
            }
            TX_ECHO_BINDER => {
                let b: SIBinder = reader.read()?;
                ok(reply)?;
                reply.write(&b)
            }
            TX_GET_CHILD => {
                let child = self
                    .child
                    .lock()
                    .expect("child poisoned")
                    .get_or_insert_with(|| Interface::as_binder(&Binder::new(Child)))
                    .clone();
                ok(reply)?;
                reply.write(&child)
            }
            TX_STATS => {
                ok(reply)?;
                for v in self.stats() {
                    reply.write(&v)?;
                }
                Ok(())
            }
            TX_BYTES => {
                let reply_len: i32 = reader.read()?;
                let payload: Vec<u8> = reader.read()?;
                ok(reply)?;
                reply.write(&(payload.len() as i32))?;
                reply.write(&vec![0x33u8; reply_len.max(0) as usize])
            }
            TX_ONEWAY_BINDER => {
                // Dropping the proxy here sends its DEC_STRONG back.
                let _b: SIBinder = reader.read()?;
                Ok(())
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
        Ok(())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("rsbinder::rpc=info"),
    )
    .format_timestamp_millis()
    .init();

    let mut args = std::env::args().skip(1);
    let sock = args
        .next()
        .unwrap_or_else(|| "/data/local/tmp/rsr34.sock".to_string());
    let noroot_sock = args
        .next()
        .unwrap_or_else(|| "/data/local/tmp/rsr34_noroot.sock".to_string());
    let max_threads: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);

    let _ = std::fs::remove_file(&noroot_sock);
    let noroot = RpcServer::setup_unix_server(&noroot_sock)?;
    std::thread::spawn(move || {
        if let Err(e) = noroot.run() {
            eprintln!("[rsbinder-server] rootless server ended: {e:?}");
        }
    });

    let _ = std::fs::remove_file(&sock);
    let server = RpcServer::setup_unix_server(&sock)?;
    server.set_max_threads(max_threads);
    server.set_root(Interface::as_binder(&Binder::new(R34Interop {
        server: Arc::downgrade(&server),
        oneway_log: Mutex::new(Vec::new()),
        held: Mutex::new(Vec::new()),
        child: Mutex::new(None),
        in_flight: AtomicI32::new(0),
    })))?;

    println!(
        "[rsbinder-server] READY r34 max_threads={max_threads} on {sock} (rootless: {noroot_sock})"
    );
    server.run()?;
    Ok(())
}
