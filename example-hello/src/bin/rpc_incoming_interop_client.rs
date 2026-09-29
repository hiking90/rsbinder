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
//!    `RpcClientConfig::incoming_connections(1)` performs is what
//!    libbinder's `RpcServer::establishConnection` expects
//!    (`addOutgoingConnection(init=true)`);
//! 2. libbinder's `ExclusiveConnection::find` from a non-handler thread
//!    picks that connection and the rsbinder incoming thread answers the
//!    twoway echo and receives the oneway notify;
//! 3. a **nested call made from inside that oneway handler** reaches the
//!    server. The connection the oneway arrived on is not one libbinder
//!    reads outside a reply wait, so the nested call has to leave on the
//!    outgoing (founding) connection instead — AOSP gates this with
//!    `RpcConnection::allowNested`, and pinning it to the incoming
//!    connection blocks until the session dies;
//! 4. `RpcSession::close_session` ends the incoming thread cleanly;
//! 5. a reply deadline that expires ends the session on **both** ends
//!    (plan 2-24 D2): the call returns `TimedOut`, the next one
//!    `DeadObject`, this end's death recipient fires, and the server's
//!    death link on a binder of ours fires as libbinder shuts its session
//!    down (`RpcSession::shutdownAndWait` → `sendObituaries`).
//!
//! The argument picks the transport: a socket path, or `tcp:<host>:<port>`
//! for inet (the `tcp-debug` feature). Over inet every connection comes
//! from one `RpcClientConfig` connect function, as AOSP `setupClient`
//! opens them over `setupInetClient` — the case plan 10-7 Phase 0 opened.
//!
//! Exit code 0 = PASS; non-zero = the failing step.
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rsbinder::rpc::{RpcClientConfig, RpcProxy, RpcSession};
use rsbinder::*;

const ROOT_DESC: &str = "rsbinder.test.IIncomingInterop";
const CB_DESC: &str = "rsbinder.test.IIncomingInteropCallback";

const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_SCHEDULE_CALLBACK: TransactionCode = FIRST_CALL_TRANSACTION + 5;
const TX_GET_SCHED: TransactionCode = FIRST_CALL_TRANSACTION + 6;
const TX_WATCH: TransactionCode = FIRST_CALL_TRANSACTION + 7;
const TX_SLOW: TransactionCode = FIRST_CALL_TRANSACTION + 8;
const TX_GET_DIED: TransactionCode = FIRST_CALL_TRANSACTION + 9;
const TX_CALLBACK_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_CALLBACK_NOTIFY: TransactionCode = FIRST_CALL_TRANSACTION + 1;

struct Cb {
    echo_seen: Arc<AtomicI32>,
    notify_seen: Arc<AtomicI32>,
    /// The server root, so the oneway handler can call back into the
    /// session it was dispatched from.
    root: SIBinder,
    /// What that nested call returned, shared with `main`. `None` while
    /// it is still running — the shape of the regression this checks for.
    nested: Arc<Mutex<Option<std::result::Result<String, StatusCode>>>>,
}

impl Cb {
    /// A twoway call to the server, issued from a callback handler.
    fn call_server(&self, arg: &str) -> Result<String> {
        let rp = (*self.root)
            .as_any()
            .downcast_ref::<RpcProxy>()
            .ok_or(StatusCode::BadType)?;
        let mut d = rp.build_request(ROOT_DESC)?;
        d.write(&arg)?;
        let mut r = rp
            .transact(TX_ECHO, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read()
    }
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
                eprintln!("[rsbinder-cb] notify (oneway); calling the server back");
                // The nested call must NOT ride the connection this
                // oneway arrived on: libbinder reads that one only inside
                // its own reply wait, and it is not waiting for anything.
                let nested = self.call_server("from-oneway");
                eprintln!("[rsbinder-cb] nested call from the oneway handler: {nested:?}");
                *self.nested.lock().expect("nested poisoned") = Some(nested);
                // Bumped last, so a nested call that never returns leaves
                // the main thread's wait to time out with a diagnosis.
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

/// A binder of ours for the server to watch; it answers nothing.
struct Watched;
impl Interface for Watched {}
impl Remotable for Watched {
    fn descriptor() -> &'static str {
        CB_DESC
    }
    fn on_transact(&self, _: TransactionCode, _: &mut Parcel, _: &mut Parcel) -> Result<()> {
        Err(StatusCode::UnknownTransaction)
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

struct Died(Arc<AtomicBool>);
impl DeathRecipient for Died {
    fn binder_died(&self, _who: &WIBinder) {
        self.0.store(true, Ordering::SeqCst);
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

#[cfg(feature = "tcp-debug")]
fn connect_inet(addr: &str) -> Result<RpcSession> {
    use rsbinder::rpc::RpcClientConfig;
    let addr: std::net::SocketAddr = addr.parse().map_err(|_| StatusCode::BadValue)?;
    RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::tcp_debug(addr, 2)
            .outgoing_connections(2)
            .incoming_connections(1),
    )
}

#[cfg(not(feature = "tcp-debug"))]
fn connect_inet(_addr: &str) -> Result<RpcSession> {
    eprintln!("[rsbinder-client] tcp: needs the `tcp-debug` feature");
    Err(StatusCode::InvalidOperation)
}

// Two outgoing slots: the callback handler calls the server back while `main` holds one.
fn connect(sock: &str) -> Result<RpcSession> {
    match sock.strip_prefix("tcp:") {
        Some(addr) => connect_inet(addr),
        None => RpcSession::setup_client_android13plus_with_config(
            RpcClientConfig::unix(std::path::Path::new(sock), 2)
                .outgoing_connections(2)
                .incoming_connections(1),
        ),
    }
}

fn fail(code: i32, what: String) -> ! {
    eprintln!("[rsbinder-client] FAIL: {what}");
    std::process::exit(code)
}

/// (5) A reply deadline that expires ends the session here and in libbinder (plan 2-24 D2).
fn reply_deadline_ends_both_sessions(
    sock: &str,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let session = connect(sock)?;
    let root = session.get_root()?;
    let rp = (*root)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .ok_or("root is not an RpcProxy")?;
    let died = Arc::new(AtomicBool::new(false));
    let recipient: Arc<dyn DeathRecipient> = Arc::new(Died(Arc::clone(&died)));
    root.link_to_death(Arc::downgrade(&recipient))?;

    // The server links to a binder of ours: its obituary says the server's session ended too.
    let watched: SIBinder = Interface::as_binder(&Binder::new(Watched));
    let mut d = rp.build_request(ROOT_DESC)?;
    d.write(&watched)?;
    let mut r = rp.transact(TX_WATCH, &d, 0)?.ok_or("watch: no reply")?;
    read_status(&mut r)?;
    let linked: i32 = r.read()?;
    if linked != 0 {
        fail(
            16,
            format!("the server could not link to our binder: {linked}"),
        );
    }

    session.set_timeout(Some(Duration::from_secs(1)));
    let mut d = rp.build_request(ROOT_DESC)?;
    d.write(&3000i32)?;
    let started = Instant::now();
    match rp.transact(TX_SLOW, &d, 0) {
        Err(StatusCode::TimedOut) => {}
        other => fail(
            17,
            format!(
                "a 3 s handler under a 1 s deadline: {:?}",
                other.map(|_| ())
            ),
        ),
    }
    eprintln!(
        "[rsbinder-client] (5) TX_SLOW timed out after {:?}",
        started.elapsed()
    );

    let mut d = rp.build_request(ROOT_DESC)?;
    d.write(&"after")?;
    match rp.transact(TX_ECHO, &d, 0) {
        Err(StatusCode::DeadObject) => {}
        other => fail(
            18,
            format!("a call after the deadline: {:?}", other.map(|_| ())),
        ),
    }
    if !died.load(Ordering::SeqCst) {
        fail(19, "this end's death recipient did not fire".into());
    }

    // Asked on a session of its own: the ended one can no longer carry the question.
    let probe = connect(sock)?;
    let proot = probe.get_root()?;
    let prp = (*proot)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .ok_or("root is not an RpcProxy")?;
    let mut server_died = 0;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        let d = prp.build_request(ROOT_DESC)?;
        let mut r = prp
            .transact(TX_GET_DIED, &d, 0)?
            .ok_or("get_died: no reply")?;
        read_status(&mut r)?;
        server_died = r.read()?;
        if server_died == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    probe.close_session();
    session.close_session();
    if server_died != 1 {
        fail(
            20,
            "libbinder's session outlived the deadline: no obituary for our binder".into(),
        );
    }
    eprintln!("[rsbinder-client] (5) both sessions ended; libbinder sent the obituary");
    Ok(())
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

    // Every wire wait below is unbounded by default, and the attach at
    // `setup_*` blocks reading the server's connection-init before any
    // session exists to set a deadline on. A gate that hangs reports
    // nothing, so bound the whole run from outside.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(60));
        eprintln!("[rsbinder-client] FAIL: watchdog — gate still running after 60s");
        std::process::exit(15);
    });

    let session = connect(&sock)?;
    // Bound every reply wait from here on (the default is to block forever).
    session.set_timeout(Some(Duration::from_secs(10)));
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
    let nested_result: Arc<Mutex<Option<std::result::Result<String, StatusCode>>>> =
        Arc::new(Mutex::new(None));
    let cb: SIBinder = Interface::as_binder(&Binder::new(Cb {
        echo_seen: Arc::clone(&echo_seen),
        notify_seen: Arc::clone(&notify_seen),
        root: root.clone(),
        nested: Arc::clone(&nested_result),
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
    // (3) The nested call the oneway handler made must have reached the
    // server. Pinned to the incoming connection it would still be
    // blocked here, and `notify_seen` above would never have risen.
    match nested_result.lock().expect("nested poisoned").as_ref() {
        Some(Ok(got)) if got == "from-oneway" => {
            eprintln!("[rsbinder-client] (3) nested call from the oneway handler round-tripped")
        }
        other => {
            eprintln!("[rsbinder-client] FAIL: nested call from oneway handler: {other:?}");
            std::process::exit(14);
        }
    }
    eprintln!(
        "[rsbinder-client] (e) callback outside a handler round-tripped after {:?}",
        t0.elapsed()
    );

    // Teardown: the incoming thread is joined here. `__incoming_thread_
    // count` cannot witness that — `close_session` empties the handle list
    // before joining any of it — so check the join counter and that the
    // thread really finished.
    session.close_session();
    if session.__incoming_thread_joined_count() != 1 || session.__incoming_thread_live_count() != 0
    {
        eprintln!(
            "[rsbinder-client] FAIL: incoming thread not joined (joined={}, live={})",
            session.__incoming_thread_joined_count(),
            session.__incoming_thread_live_count()
        );
        std::process::exit(13);
    }
    reply_deadline_ends_both_sessions(&sock)?;
    println!("PASS — plan 2-20 (e): rsbinder incoming connection ↔ real libbinder RpcServer");
    Ok(())
}
