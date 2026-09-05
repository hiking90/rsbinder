// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Behavioral RPC scenarios ported from AOSP `binderRpcTest.cpp` /
//! `binderRpcUniversalTests.cpp` that were not yet represented in
//! rsbinder's native RPC suite (see plans/5-aosp-test-porting.md §6).
//!
//! These are NOT a 1:1 port of the C++ gtest cases (those are soong/gtest
//! bound, and wire compat is proven via STAGE3 real-libbinder interop).
//! They re-create the *scenarios* against rsbinder's own `RpcServer` over a
//! Unix socket, so they run hermetically (no kernel binder, no emulator).
//!
//! Coverage map (AOSP name → test here):
//! - `SendLargeVector`              → `send_large_vector_round_trips`
//! - `UnknownTransaction`           → `unknown_transaction_returns_unknown`
//! - `RepeatBinder`                 → `repeat_binder_round_trips_as_the_same_local_binder`
//! - `RepeatBinderNull`             → `repeat_binder_null`
//! - `HoldBinder`/`getHeldBinder`   → `hold_and_get_binder`
//! - `alwaysGiveMeTheSameBinder` /
//!   `SameBinderEquality`           → `same_binder_returned_twice_is_equal`
//! - `OnewayCallDoesNotWait`        → `oneway_call_does_not_wait_for_handler`
//! - (rsbinder) a nested call that times out must not desync the
//!   connection → `late_reply_of_a_timed_out_nested_call_is_skipped`

#![cfg(feature = "rpc")]

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rsbinder::rpc::{RpcProxy, RpcServer, RpcSession};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, Result, SIBinder, Status, StatusCode, TransactionCode,
    FIRST_CALL_TRANSACTION,
};

const DESC: &str = "rsbinder.test.IRpcScenario";
const CB_DESC: &str = "rsbinder.test.IScenarioCallback";

const TX_BIG_ECHO: TransactionCode = FIRST_CALL_TRANSACTION; // Vec<u8> -> Vec<u8>
const TX_REPEAT_BINDER: TransactionCode = FIRST_CALL_TRANSACTION + 1; // @nullable IBinder -> @nullable IBinder
const TX_HOLD_BINDER: TransactionCode = FIRST_CALL_TRANSACTION + 2;
const TX_GET_HELD: TransactionCode = FIRST_CALL_TRANSACTION + 3;
const TX_SAME_BINDER: TransactionCode = FIRST_CALL_TRANSACTION + 4; // -> always the same IBinder
const TX_PING_DELAY: TransactionCode = FIRST_CALL_TRANSACTION + 5; // oneway: sleep then bump
const TX_CALL_CB: TransactionCode = FIRST_CALL_TRANSACTION + 6; // (IBinder cb, String) -> String via cb
const TX_SLOW: TransactionCode = FIRST_CALL_TRANSACTION + 7; // (i64 ms) -> (): sleeps
                                                             // Deliberately never handled by the server (tests UNKNOWN_TRANSACTION).
const TX_UNKNOWN: TransactionCode = FIRST_CALL_TRANSACTION + 100;

// Callback transaction the client's callback object answers.
const TX_CB_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;

// ---- server service -------------------------------------------------

struct ScenarioSvc {
    /// Binder held via `TX_HOLD_BINDER`, returned by `TX_GET_HELD`.
    held: Mutex<Option<SIBinder>>,
    /// A single stable binder that `TX_SAME_BINDER` always returns.
    same: SIBinder,
    /// Oneway delay handler entry signal + completion counter.
    delay_entered: Arc<AtomicBool>,
    delay_done: Arc<AtomicI64>,
}

impl Interface for ScenarioSvc {}

struct BnScenario(ScenarioSvc);
impl Remotable for BnScenario {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        let s = &self.0;
        match code {
            TX_BIG_ECHO => {
                let v: Vec<u8> = reader.read()?;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&v)
            }
            TX_REPEAT_BINDER => {
                // @nullable IBinder in, @nullable IBinder out.
                let b: Option<SIBinder> = reader.read()?;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&b)
            }
            TX_HOLD_BINDER => {
                let b: Option<SIBinder> = reader.read()?;
                *s.held.lock().unwrap() = b;
                reply.write(&Status::from(StatusCode::Ok))
            }
            TX_GET_HELD => {
                let b = s.held.lock().unwrap().clone();
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&b)
            }
            TX_SAME_BINDER => {
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&Some(s.same.clone()))
            }
            TX_PING_DELAY => {
                // oneway: no reply. Signal entry, sleep, then mark done.
                s.delay_entered.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(300));
                s.delay_done.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            TX_CALL_CB => {
                // Nested server → client call while this dispatch is open.
                let cb: SIBinder = reader.read()?;
                let s: String = reader.read()?;
                let out = cb_echo(&cb, &s)?;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&out)
            }
            TX_SLOW => {
                let ms: i64 = reader.read()?;
                std::thread::sleep(Duration::from_millis(ms as u64));
                reply.write(&Status::from(StatusCode::Ok))
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn make_service(delay_entered: Arc<AtomicBool>, delay_done: Arc<AtomicI64>) -> SIBinder {
    // The "same" binder is a distinct, stable callback-style object.
    let same = Interface::as_binder(&Binder::new(BnScenarioCallback(Mutex::new(0))));
    Interface::as_binder(&Binder::new(BnScenario(ScenarioSvc {
        held: Mutex::new(None),
        same,
        delay_entered,
        delay_done,
    })))
}

// ---- a simple client-side callback object ---------------------------

struct BnScenarioCallback(Mutex<i64>);
impl Interface for BnScenarioCallback {}
impl Remotable for BnScenarioCallback {
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
            TX_CB_ECHO => {
                let s: String = reader.read()?;
                *self.0.lock().unwrap() += 1;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&format!("cb:{s}"))
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

/// A callback whose handler calls *back into the server* (a nested call on
/// the connection it is being served on) and records the outcome.
struct BnNestingCallback {
    target: ScenarioProxy,
    slow_ms: i64,
    nested: Arc<Mutex<Vec<Result<()>>>>,
}
impl Interface for BnNestingCallback {}
impl Remotable for BnNestingCallback {
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
            TX_CB_ECHO => {
                let s: String = reader.read()?;
                let r = self.target.slow(self.slow_ms);
                self.nested.lock().unwrap().push(r);
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&format!("cb:{s}"))
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn rpc_of(b: &SIBinder) -> &RpcProxy {
    (**b).as_any().downcast_ref::<RpcProxy>().expect("RpcProxy")
}

/// `TX_CB_ECHO` on a (proxied) callback.
fn cb_echo(cb: &SIBinder, s: &str) -> Result<String> {
    let rp = rpc_of(cb);
    let mut d = rp.build_request(CB_DESC)?;
    d.write(&s)?;
    let mut r = rp
        .transact(TX_CB_ECHO, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    read_status(&mut r)?;
    r.read::<String>()
}

// ---- client typed proxy ---------------------------------------------

struct ScenarioProxy(SIBinder);
impl ScenarioProxy {
    fn rp(&self) -> &RpcProxy {
        rpc_of(&self.0)
    }
    fn call_cb(&self, cb: &SIBinder, s: &str) -> Result<String> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(cb)?;
        d.write(&s)?;
        let mut r = self
            .rp()
            .transact(TX_CALL_CB, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<String>()
    }
    fn slow(&self, ms: i64) -> Result<()> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(&ms)?;
        let mut r = self
            .rp()
            .transact(TX_SLOW, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)
    }
    fn big_echo(&self, v: &[u8]) -> Result<Vec<u8>> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(&v.to_vec())?;
        let mut r = self
            .rp()
            .transact(TX_BIG_ECHO, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<Vec<u8>>()
    }
    fn repeat_binder(&self, b: Option<SIBinder>) -> Result<Option<SIBinder>> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(&b)?;
        let mut r = self
            .rp()
            .transact(TX_REPEAT_BINDER, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<Option<SIBinder>>()
    }
    fn hold_binder(&self, b: Option<SIBinder>) -> Result<()> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(&b)?;
        let mut r = self
            .rp()
            .transact(TX_HOLD_BINDER, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)
    }
    fn get_held(&self) -> Result<Option<SIBinder>> {
        let d = self.rp().build_request(DESC)?;
        let mut r = self
            .rp()
            .transact(TX_GET_HELD, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<Option<SIBinder>>()
    }
    fn same_binder(&self) -> Result<Option<SIBinder>> {
        let d = self.rp().build_request(DESC)?;
        let mut r = self
            .rp()
            .transact(TX_SAME_BINDER, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<Option<SIBinder>>()
    }
    fn ping_delay(&self) -> Result<()> {
        let d = self.rp().build_request(DESC)?;
        self.rp()
            .transact(TX_PING_DELAY, &d, rsbinder::FLAG_ONEWAY)
            .map(|_| ())
    }
    fn raw_unknown(&self) -> Result<Option<Parcel>> {
        let d = self.rp().build_request(DESC)?;
        self.rp().transact(TX_UNKNOWN, &d, 0)
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

// ---- harness --------------------------------------------------------

fn tmp_sock(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rsb_rpc_scn_{}_{}_{}.sock",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    p
}

fn wait_for_sock(path: &std::path::Path) {
    for _ in 0..400 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("server socket {path:?} never appeared");
}

/// RAII teardown: shutdown + join + remove socket on any drop path.
struct ServeCleanup {
    server: Arc<RpcServer>,
    bg: Option<std::thread::JoinHandle<()>>,
    path: std::path::PathBuf,
}
impl Drop for ServeCleanup {
    fn drop(&mut self) {
        self.server.stop_accepting();
        if let Some(h) = self.bg.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Boot a server with a fresh `ScenarioSvc` root and return everything a
/// test needs (the proxy + the entry/done signals it may assert on).
struct Booted {
    _cu: ServeCleanup,
    proxy: ScenarioProxy,
    client: RpcSession,
    delay_entered: Arc<AtomicBool>,
}

fn boot(tag: &str) -> Booted {
    let path = tmp_sock(tag);
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    let delay_entered = Arc::new(AtomicBool::new(false));
    let delay_done = Arc::new(AtomicI64::new(0));
    server.set_root(make_service(
        Arc::clone(&delay_entered),
        Arc::clone(&delay_done),
    ));
    let bg = server.run_background();
    let cu = ServeCleanup {
        server: Arc::clone(&server),
        bg: Some(bg),
        path: path.clone(),
    };
    wait_for_sock(&path);
    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let proxy = ScenarioProxy(client.get_root().expect("get_root"));
    Booted {
        _cu: cu,
        proxy,
        client,
        delay_entered,
    }
}

// ---- tests ----------------------------------------------------------

/// AOSP `SendLargeVector` — a payload well past one socket buffer must
/// round-trip intact over the framed RPC transport.
#[test]
fn send_large_vector_round_trips() {
    let b = boot("big");
    // 256 KiB, value pattern so corruption/truncation is detectable.
    let v: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    let got = b.proxy.big_echo(&v).expect("big_echo");
    assert_eq!(got.len(), v.len(), "length mismatch on large vector");
    assert_eq!(got, v, "large vector corrupted in transit");
}

/// AOSP `UnknownTransaction` — an unhandled code returns
/// `UNKNOWN_TRANSACTION`, not a panic or a hang.
#[test]
fn unknown_transaction_returns_unknown() {
    let b = boot("unk");
    let err = b.proxy.raw_unknown().expect_err("unknown code must error");
    assert_eq!(err, StatusCode::UnknownTransaction);
}

/// AOSP `RepeatBinder` — a binder passed as an argument AND returned in the
/// same transaction comes back as the **same local object** (exercises the
/// arg-and-return binder wire path in one call). The server's argument
/// proxy goes out of scope before the reply is sent; the reply parcel pins
/// it (`write_binder`) so its `DEC_STRONG` follows the reply instead of
/// overtaking it — otherwise the client frees the node before the address
/// comes back and mints a fresh proxy for its own object.
#[test]
fn repeat_binder_round_trips_as_the_same_local_binder() {
    let b = boot("repeat");
    let cb: SIBinder = Interface::as_binder(&Binder::new(BnScenarioCallback(Mutex::new(0))));

    let echoed = b
        .proxy
        .repeat_binder(Some(cb.clone()))
        .expect("repeat_binder");
    assert_eq!(
        echoed.as_ref(),
        Some(&cb),
        "a binder that comes home in the same call must map back to the local stub"
    );
}

/// AOSP `RepeatBinderNull` — a null binder argument round-trips as null.
#[test]
fn repeat_binder_null() {
    let b = boot("repeatnull");
    let got = b.proxy.repeat_binder(None).expect("repeat null");
    assert!(got.is_none(), "null binder must round-trip as null");
}

/// AOSP `HoldBinder`/`getHeldBinder` — the server stores a client binder
/// across calls and hands it back; the retrieved proxy is still callable.
#[test]
fn hold_and_get_binder() {
    let b = boot("hold");
    let cb: SIBinder = Interface::as_binder(&Binder::new(BnScenarioCallback(Mutex::new(0))));

    assert!(b.proxy.get_held().expect("get_held empty").is_none());
    b.proxy.hold_binder(Some(cb.clone())).expect("hold");
    let held = b.proxy.get_held().expect("get_held").expect("held is some");

    assert_eq!(
        held, cb,
        "the held client binder must come back as the same local object"
    );
}

/// AOSP `alwaysGiveMeTheSameBinder` / `SameBinderEquality` — the server
/// returns one stable binder on every call; the client must observe the two
/// returned proxies as equal (the session de-duplicates by address).
#[test]
fn same_binder_returned_twice_is_equal() {
    let b = boot("same");
    let first = b.proxy.same_binder().expect("same 1").expect("non-null");
    let second = b.proxy.same_binder().expect("same 2").expect("non-null");
    assert_eq!(
        first, second,
        "the same server binder must compare equal across two fetches"
    );

    // A server-returned binder is callable in the normal client→server
    // direction (exercises the returned proxy end-to-end).
    let rp = (*first)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .expect("server binder is an RpcProxy on the client");
    let mut d = rp.build_request(CB_DESC).expect("build");
    d.write(&"x").expect("write");
    let mut r = rp
        .transact(TX_CB_ECHO, &d, 0)
        .expect("transact")
        .expect("reply");
    read_status(&mut r).expect("status");
    assert_eq!(r.read::<String>().expect("result"), "cb:x");
}

/// AOSP `OnewayCallDoesNotWait` — a oneway call returns to the caller before
/// the (slow) server handler has finished running.
#[test]
fn oneway_call_does_not_wait_for_handler() {
    let b = boot("oneway");
    let t0 = std::time::Instant::now();
    b.proxy.ping_delay().expect("oneway send");
    let elapsed = t0.elapsed();
    // The handler sleeps 300 ms; the oneway send must return well before that.
    assert!(
        elapsed < Duration::from_millis(200),
        "oneway call blocked on the handler ({elapsed:?})"
    );
    // The handler did start (entry observed) — sanity that it really ran.
    for _ in 0..400 {
        if b.delay_entered.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        b.delay_entered.load(Ordering::SeqCst),
        "oneway handler never entered"
    );
}

/// A nested call (client callback → server, on the connection the callback
/// is being served on) that gives up on its reply (`set_timeout`) must not
/// desync that connection: the late reply is skipped, the outer call still
/// gets *its* reply, and the connection stays usable. The nested call
/// cannot retire the slot — the outer frame owns it — so without the
/// skip the late reply would be taken for the outer call's reply.
#[test]
fn late_reply_of_a_timed_out_nested_call_is_skipped() {
    let b = boot("stale");
    // The outer reply follows the server's `slow_ms` sleep, against a deadline
    // re-armed when the nested call gives up: `timeout < slow_ms < 2 * timeout`,
    // with a wide margin on both sides for a loaded CI runner.
    b.client.set_timeout(Some(Duration::from_millis(1000)));
    let nested = Arc::new(Mutex::new(Vec::new()));
    let cb: SIBinder = Interface::as_binder(&Binder::new(BnNestingCallback {
        target: ScenarioProxy(b.proxy.0.clone()),
        slow_ms: 1500,
        nested: Arc::clone(&nested),
    }));

    let got = b
        .proxy
        .call_cb(&cb, "hello")
        .expect("outer call survives the nested call's timeout");
    assert_eq!(got, "cb:hello");
    assert_eq!(
        nested.lock().unwrap().as_slice(),
        &[Err(StatusCode::TimedOut)],
        "the nested call timed out exactly once"
    );
    assert_eq!(
        b.proxy
            .big_echo(b"again")
            .expect("connection still in sync"),
        b"again"
    );
}
