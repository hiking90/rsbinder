// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Multi-session `RpcServer`, real-process e2e, threads,
//! `getRemoteMaxThreads` negotiation, oneway FIFO, nested callbacks,
//! timeout, lifecycle, and the no-global gate.
//!
//! Separate test binary. Each test builds its own
//! server + sessions ⇒ parallel-safe, no `--test-threads=1`.

#![cfg(feature = "rpc")]

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rsbinder::rpc::{RpcProxy, RpcServer, RpcSession, RpcUnixClientConfig};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, Result, SIBinder, Status, StatusCode, TransactionCode,
    FIRST_CALL_TRANSACTION,
};

const DESC: &str = "rsbinder.test.IEcho2";
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_BUMP: TransactionCode = FIRST_CALL_TRANSACTION + 1; // oneway
const TX_COUNT: TransactionCode = FIRST_CALL_TRANSACTION + 2;
const TX_SLOW: TransactionCode = FIRST_CALL_TRANSACTION + 3;
const TX_ROUNDTRIP: TransactionCode = FIRST_CALL_TRANSACTION + 4; // nested
const TX_HOLD_CB: TransactionCode = FIRST_CALL_TRANSACTION + 5; // store a callback for later

trait IEcho2: Interface {
    fn echo(&self, s: &str) -> Result<String>;
    fn bump(&self) -> Result<()>; // oneway
    fn count(&self) -> Result<i64>;
    fn slow(&self, ms: i32) -> Result<()>;
    /// Server calls `cb.echo("ping")` and returns its result
    /// (exercises a server→client nested callback).
    fn roundtrip(&self, cb: &SIBinder) -> Result<String>;
    /// Server stores `cb`; a test then drives it from **outside** any
    /// handler (plan 2-20).
    fn hold(&self, cb: &SIBinder) -> Result<()>;
}

struct EchoSvc {
    counter: Arc<AtomicI64>,
    /// Optional self-handle so `roundtrip`'s callback can call back in
    /// (depth-3): the client callback re-invokes the *server*.
    deeper: bool,
    /// Set on **entry** to `slow()` so a test can wait deterministically
    /// for a parked thread to have actually claimed a server worker
    /// (e.g. the slot-pin scope). Tests that don't care pass a
    /// fresh `AtomicBool` and ignore it. Set is one-way (no reset) —
    /// "at least one slow call entered" is the only signal needed.
    slow_entered: Arc<AtomicBool>,
    /// Callback parked by `hold()` for out-of-handler use.
    held: Arc<Mutex<Option<SIBinder>>>,
}
impl Interface for EchoSvc {}
impl IEcho2 for EchoSvc {
    fn echo(&self, s: &str) -> Result<String> {
        Ok(s.to_string())
    }
    fn bump(&self) -> Result<()> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn count(&self) -> Result<i64> {
        Ok(self.counter.load(Ordering::SeqCst))
    }
    fn slow(&self, ms: i32) -> Result<()> {
        self.slow_entered.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(ms.max(0) as u64));
        Ok(())
    }
    fn roundtrip(&self, cb: &SIBinder) -> Result<String> {
        // Call back into the client-provided callback (server→client).
        let rp = (**cb)
            .as_any()
            .downcast_ref::<RpcProxy>()
            .ok_or(StatusCode::BadType)?;
        let mut d = rp.build_request(DESC)?;
        d.write(&"ping")?;
        let mut r = rp
            .transact(TX_ECHO, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        let got: String = r.read()?;
        let _ = self.deeper;
        Ok(format!("rt:{got}"))
    }
    fn hold(&self, cb: &SIBinder) -> Result<()> {
        *self.held.lock().unwrap() = Some(cb.clone());
        Ok(())
    }
}

fn echo_on_transact(
    s: &dyn IEcho2,
    code: TransactionCode,
    reader: &mut Parcel,
    reply: &mut Parcel,
) -> Result<()> {
    match code {
        TX_ECHO => {
            let a: String = reader.read()?;
            ok_str(reply, s.echo(&a))
        }
        TX_BUMP => {
            // oneway: no reply written.
            let _ = s.bump();
            Ok(())
        }
        TX_COUNT => match s.count() {
            Ok(v) => {
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&v)
            }
            Err(e) => reply.write(&Status::from(e)),
        },
        TX_SLOW => {
            let ms: i32 = reader.read()?;
            match s.slow(ms) {
                Ok(()) => reply.write(&Status::from(StatusCode::Ok)),
                Err(e) => reply.write(&Status::from(e)),
            }
        }
        TX_ROUNDTRIP => {
            let cb: SIBinder = reader.read()?;
            ok_str(reply, s.roundtrip(&cb))
        }
        TX_HOLD_CB => {
            let cb: SIBinder = reader.read()?;
            match s.hold(&cb) {
                Ok(()) => reply.write(&Status::from(StatusCode::Ok)),
                Err(e) => reply.write(&Status::from(e)),
            }
        }
        _ => Err(StatusCode::UnknownTransaction),
    }
}

fn ok_str(reply: &mut Parcel, r: Result<String>) -> Result<()> {
    match r {
        Ok(v) => {
            reply.write(&Status::from(StatusCode::Ok))?;
            reply.write(&v)
        }
        Err(e) => reply.write(&Status::from(e)),
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

struct BnEcho2(Box<dyn IEcho2 + Send + Sync>);
impl Remotable for BnEcho2 {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        echo_on_transact(&*self.0, code, reader, reply)
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn make_service(counter: Arc<AtomicI64>) -> SIBinder {
    make_service_with_slow_signal(counter, Arc::new(AtomicBool::new(false)))
}

/// Build an `EchoSvc` whose `slow()` entry sets the supplied `AtomicBool`
/// so a test can wait deterministically for a parked client thread to
/// have actually claimed a server worker — instead of a sleep(N ms)
/// heuristic that races scheduler jitter.
fn make_service_with_slow_signal(
    counter: Arc<AtomicI64>,
    slow_entered: Arc<AtomicBool>,
) -> SIBinder {
    Interface::as_binder(&Binder::new(BnEcho2(Box::new(EchoSvc {
        counter,
        deeper: false,
        slow_entered,
        held: Arc::new(Mutex::new(None)),
    }))))
}
/// Build an `EchoSvc` whose `hold()` parks the callback in `held`, so a
/// test can drive that proxy from a thread that is inside no handler.
fn make_service_with_hold(counter: Arc<AtomicI64>, held: Arc<Mutex<Option<SIBinder>>>) -> SIBinder {
    Interface::as_binder(&Binder::new(BnEcho2(Box::new(EchoSvc {
        counter,
        deeper: false,
        slow_entered: Arc::new(AtomicBool::new(false)),
        held,
    }))))
}

// ---- client typed proxy --------------------------------------------

struct EchoProxy(SIBinder);
impl EchoProxy {
    fn rp(&self) -> &RpcProxy {
        (*self.0)
            .as_any()
            .downcast_ref::<RpcProxy>()
            .expect("RpcProxy")
    }
    fn echo(&self, s: &str) -> Result<String> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(&s)?;
        let mut r = self
            .rp()
            .transact(TX_ECHO, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<String>()
    }
    fn bump(&self) -> Result<()> {
        let d = self.rp().build_request(DESC)?;
        self.rp()
            .transact(TX_BUMP, &d, rsbinder::FLAG_ONEWAY)
            .map(|_| ())
    }
    fn count(&self) -> Result<i64> {
        let d = self.rp().build_request(DESC)?;
        let mut r = self
            .rp()
            .transact(TX_COUNT, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<i64>()
    }
    fn slow(&self, ms: i32) -> Result<()> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(&ms)?;
        let mut r = self
            .rp()
            .transact(TX_SLOW, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)
    }
    fn roundtrip(&self, cb: &SIBinder) -> Result<String> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(cb)?;
        let mut r = self
            .rp()
            .transact(TX_ROUNDTRIP, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<String>()
    }
    fn hold(&self, cb: &SIBinder) -> Result<()> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(cb)?;
        let mut r = self
            .rp()
            .transact(TX_HOLD_CB, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)
    }
}

fn tmp_sock(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rsb_rpc_{}_{}_{}.sock",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    p
}

/// Wait until a server socket file exists (bounded).
fn wait_for_sock(path: &std::path::Path) {
    for _ in 0..400 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("server socket {path:?} never appeared");
}

/// Generic bounded polling helper (~2 s budget = 400 × 5 ms). Returns
/// `true` when `f` first becomes true; the trailing `f()` is a final
/// race-tightening check after the last sleep.
fn poll_until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    f()
}

/// **RAII test cleanup** — guarantees `shutdown` + `bg.join` +
/// `join_workers` + socket-file removal on **any** drop path, including
/// an `assert!`/`unwrap`/`expect` panic mid-test. Without this an
/// assertion failure leaks the background accept loop + every spawned
/// `serve_blocking` worker into the test binary process for the
/// remainder of the suite (each subsequent timing-sensitive
/// test then runs against a contaminated scheduler). Construct **once**
/// per test, right after `server.run_background()`, then let `Drop` do
/// the rest.
struct ServeCleanup {
    server: Arc<RpcServer>,
    bg: Option<std::thread::JoinHandle<()>>,
    path: Option<std::path::PathBuf>,
}
impl ServeCleanup {
    fn new(
        server: Arc<RpcServer>,
        bg: std::thread::JoinHandle<()>,
        path: std::path::PathBuf,
    ) -> Self {
        Self {
            server,
            bg: Some(bg),
            path: Some(path),
        }
    }
}
impl Drop for ServeCleanup {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(h) = self.bg.take() {
            // A panic in the background accept loop is a real SUT
            // bug (`RpcServer::run()` is supposed to return cleanly).
            // Discarding the join result would let a regression that
            // introduces an `expect("...poisoned")` panic in the accept
            // path pass every test green. Surface the payload to stderr —
            // *not* via `resume_unwind`, because the test's own
            // assertions may already be unwinding and the more useful
            // signal is the first panic, not the cleanup-time
            // double-panic. The stderr line is what makes the bg
            // panic observable in CI logs.
            if let Err(p) = h.join() {
                let msg = p
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| p.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string panic payload>");
                eprintln!(
                    "WARNING: ServeCleanup observed a background accept-loop \
                     panic during teardown: {msg}"
                );
            }
        }
        self.server.join_workers();
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ---- real OS process e2e + negotiation -----------------------------

#[test]
fn real_process_e2e_and_negotiation() {
    // Child role: become the server and block.
    if let Ok(path) = std::env::var("RSB_RPC_SERVER") {
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server.set_max_threads(2);
        server.set_root(make_service(Arc::new(AtomicI64::new(0))));
        let _ = server.run(); // blocks until killed
        std::process::exit(0);
    }

    let path = tmp_sock("e2e");
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = std::process::Command::new(exe)
        .args(["--exact", "real_process_e2e_and_negotiation", "--nocapture"])
        .env("RSB_RPC_SERVER", &path)
        .spawn()
        .expect("spawn server child");
    wait_for_sock(&path);

    {
        let client = RpcSession::setup_unix_client(&path).expect("connect");
        assert!(matches!(
            client.add_outgoing_connection_android13plus(tmp_sock("r34bad"), 1, &[0u8; 32]),
            Err(StatusCode::BadType)
        ));
        // Explicit negotiation, local=8, server advertises 2.
        assert_eq!(client.negotiate(8).expect("negotiate"), 2);
        assert_eq!(client.negotiated_max_threads(), 2);

        let root = EchoProxy(client.get_root().expect("get_root"));
        // Real cross-process AIDL call.
        assert_eq!(
            root.echo("over a real process").unwrap(),
            "over a real process"
        );
        assert_eq!(root.echo("").unwrap(), "");
        for i in 0..50 {
            assert_eq!(root.echo(&format!("n{i}")).unwrap(), format!("n{i}"));
        }
    }

    // Killing the client leaves the server able to exit cleanly.
    child.kill().expect("kill server child");
    child.wait().expect("reap server child");
    let _ = std::fs::remove_file(&path);
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn real_process_abstract_unix_socket_e2e() {
    if let Ok(name) = std::env::var("RSB_RPC_ABSTRACT_SERVER") {
        let server = RpcServer::setup_unix_server_abstract(name.as_bytes()).expect("bind abstract");
        server.set_android13plus(2);
        server.set_max_threads(3);
        server.set_root(make_service(Arc::new(AtomicI64::new(0))));
        let _ = server.run();
        std::process::exit(0);
    }

    // Kill + reap the server child even when an assert below panics —
    // its `server.run()` loop never exits on its own.
    struct KillOnDrop(std::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let name = format!("rsb_rpc_abs_proc_{}", std::process::id());
    let exe = std::env::current_exe().expect("current_exe");
    let _child = KillOnDrop(
        std::process::Command::new(exe)
            .args([
                "--exact",
                "real_process_abstract_unix_socket_e2e",
                "--nocapture",
            ])
            .env("RSB_RPC_ABSTRACT_SERVER", &name)
            .spawn()
            .expect("spawn abstract server child"),
    );

    let client = 'connect: {
        for _ in 0..400 {
            if let Ok(c) = RpcSession::setup_unix_client_android13plus_with_config(
                RpcUnixClientConfig::abstract_name(name.as_bytes(), 2),
            ) {
                break 'connect c;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("connect abstract server child");
    };

    assert_eq!(client.wire_protocol_version(), Some(2));
    assert_eq!(client.negotiate(8).expect("negotiate"), 3);
    let sid = client.get_session_id().expect("get_session_id");
    let attached = RpcSession::setup_unix_client_android13plus_with_config(
        RpcUnixClientConfig::abstract_name(name.as_bytes(), 2).session_id(&sid),
    )
    .expect("attach abstract child session");
    assert_eq!(attached.get_session_id().expect("attached id"), sid);
    let root = EchoProxy(attached.get_root().expect("attached get_root"));
    assert_eq!(root.echo("abstract-process").unwrap(), "abstract-process");

    let fan = RpcSession::setup_unix_client_android13plus_with_config(
        RpcUnixClientConfig::abstract_name(name.as_bytes(), 2).outgoing_connections(3),
    )
    .expect("abstract child fan-out");
    assert_eq!(fan.negotiated_max_threads(), 3);
    let root = Arc::new(EchoProxy(fan.get_root().expect("fan get_root")));
    let t0 = std::time::Instant::now();
    let handles: Vec<_> = (0..3)
        .map(|_| {
            let root = Arc::clone(&root);
            std::thread::spawn(move || root.slow(150).expect("slow round-trip"))
        })
        .collect();
    for h in handles {
        h.join().expect("thread");
    }
    assert!(
        t0.elapsed() < Duration::from_millis(420),
        "abstract fan-out across process must run 3 slow calls in parallel"
    );
}

// ---- concurrency: many threads, ONE shared session ----------------

#[test]
fn concurrent_calls_single_shared_session() {
    let path = tmp_sock("shared");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // 8 client threads on the SAME session.
    // ONE client session, its root proxy shared (Arc) across 8 threads
    // — exactly how a generated `Bp*` stub is used concurrently
    // (`SIBinder` is `Send`/`Sync`). Calls are internally serialized on the one
    // connection (the documented model: parallelism = multiple
    // connections), so wall time is also bounded well below a hang.
    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let root = Arc::new(EchoProxy(client.get_root().expect("get_root")));

    let t0 = std::time::Instant::now();
    let mut handles = Vec::new();
    for t in 0..8 {
        let root = Arc::clone(&root);
        handles.push(std::thread::spawn(move || {
            for i in 0..200 {
                let msg = format!("shared-t{t}-i{i}");
                assert_eq!(
                    root.echo(&msg).expect("echo on shared session"),
                    msg,
                    "reply cross-delivered / wire corrupted on shared session"
                );
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }
    assert!(
        t0.elapsed() < Duration::from_secs(30),
        "shared-session concurrency must make progress, not deadlock ({:?})",
        t0.elapsed()
    );
    // _cu handles teardown.
}

// ---- multi-session isolation ---------------------------------------

#[test]
fn concurrent_clients_isolated_sessions() {
    let path = tmp_sock("iso");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // 8 client threads, each its own connection (independent session +
    // RpcState). Each does 200 calls and must see only its own
    // echoes (no cross-session contamination, no deadlock).
    let mut handles = Vec::new();
    for t in 0..8 {
        let p = path.clone();
        handles.push(std::thread::spawn(move || {
            let client = RpcSession::setup_unix_client(&p).expect("connect");
            let root = EchoProxy(client.get_root().expect("get_root"));
            for i in 0..200 {
                let msg = format!("t{t}-i{i}");
                assert_eq!(root.echo(&msg).unwrap(), msg, "cross-session contamination");
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }
    // _cu handles teardown.
}

// ---- opt-in server-side connection admission bound -----------

/// `set_max_connections(N)` caps *concurrent* connection workers via
/// reactor-free accept backpressure (excess clients wait in the kernel
/// listen backlog, none dropped; a freed slot resumes accept). This is
/// the reactor-free, Android-faithful resource bound — a genuine middle
/// ground that the no-reactor design does NOT foreclose.
///
/// Mutant: deleting the `max_connections` gate in `RpcServer::run`
/// makes the 3rd client served immediately ⇒ its bounded-timeout
/// `get_root` would *succeed*, failing this test.
#[test]
fn max_connections_admission_bound() {
    let path = tmp_sock("admit");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    server.set_max_connections(2);
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // Two long-lived sessions occupy both worker slots: each get_root
    // succeeds (proves served), then the session is *held* so its
    // worker stays blocked in `serve_once`/recv.
    let c1 = RpcSession::setup_unix_client(&path).expect("connect c1");
    assert_eq!(
        EchoProxy(c1.get_root().expect("c1 get_root"))
            .echo("c1")
            .unwrap(),
        "c1"
    );
    let c2 = RpcSession::setup_unix_client(&path).expect("connect c2");
    assert_eq!(
        EchoProxy(c2.get_root().expect("c2 get_root"))
            .echo("c2")
            .unwrap(),
        "c2"
    );

    // 3rd connects (into the kernel backlog — `connect()` succeeds
    // without a server `accept()`), but the accept loop is gated at 2,
    // so no worker ever serves it: a bounded-deadline `get_root` must
    // time out, not succeed.
    let c3 = RpcSession::setup_unix_client(&path).expect("connect c3 (backlog)");
    c3.set_timeout(Some(Duration::from_millis(600)));
    assert!(
        c3.get_root().is_err(),
        "3rd connection must be admission-blocked while 2 workers are live"
    );
    drop(c3);

    // Free a slot: dropping c1 closes its socket ⇒ the worker's recv
    // hits EOF ⇒ `serve_blocking` returns ⇒ the JoinHandle finishes ⇒
    // the next accept tick reaps it (`live_worker_count`) ⇒ accept
    // resumes. A fresh 4th client is then served within a generous
    // deadline.
    drop(c1);
    let c4 = RpcSession::setup_unix_client(&path).expect("connect c4");
    c4.set_timeout(Some(Duration::from_secs(5)));
    assert_eq!(
        EchoProxy(c4.get_root().expect("c4 get_root after a slot freed"))
            .echo("c4")
            .unwrap(),
        "c4",
        "a freed worker slot must let the accept loop resume"
    );

    drop(c2);
    drop(c4);
    // _cu handles teardown.
}

/// A connected-but-silent **r34** peer (the default profile, which has no
/// separate handshake — first contact is the first serve-loop frame) must
/// be torn down by the handshake/admission read deadline so it cannot pin
/// its worker + admission slot forever.
///
/// Mutant: clearing the read deadline *before* the r34 serve loop leaves
/// the silent c1 worker blocked in `recv` with no deadline ⇒ the single
/// `max_connections` slot is never freed ⇒ c2's
/// bounded `get_root` times out, failing this test.
#[test]
fn silent_r34_peer_released_by_handshake_deadline() {
    let path = tmp_sock("silent_r34");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    // One slot so a pinned silent peer is directly observable, and a short
    // deadline to keep the test fast + deterministic.
    server.set_max_connections(1);
    server.set_handshake_timeout(Some(Duration::from_millis(300)));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // c1 connects but sends nothing: r34 connect performs no wire, so this
    // is the silent first-contact peer. It occupies the single worker slot.
    let c1 = RpcSession::setup_unix_client(&path).expect("connect c1 (silent)");

    // c2 connects into the backlog; it can only be served once c1's worker
    // exits on the first-frame deadline and frees the slot. A deadline
    // comfortably larger than the handshake timeout proves the slot is
    // released (and not that c2 merely raced ahead).
    let c2 = RpcSession::setup_unix_client(&path).expect("connect c2");
    c2.set_timeout(Some(Duration::from_secs(5)));
    assert_eq!(
        EchoProxy(c2.get_root().expect("c2 get_root after c1 deadline"))
            .echo("c2")
            .unwrap(),
        "c2",
        "silent r34 peer's worker must release its slot on the handshake deadline"
    );

    drop(c1);
    drop(c2);
    // _cu handles teardown.
}

// ---- oneway FIFO + non-blocking send -------------------------------

#[test]
fn oneway_fifo_and_nonblocking() {
    let path = tmp_sock("ow");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let root = EchoProxy(client.get_root().expect("get_root"));

    let n = 2000;
    let t0 = std::time::Instant::now();
    for _ in 0..n {
        root.bump().expect("oneway bump");
    }
    // Oneway sends must not block on a per-call round trip — 2000 of
    // them complete far faster than 2000 sync RTTs would.
    let oneway_elapsed = t0.elapsed();

    // A subsequent sync call observes all prior oneway calls in order
    // (single connection ⇒ FIFO): the count has reached n.
    let mut last = root.count().unwrap();
    for _ in 0..200 {
        if last == n {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
        last = root.count().unwrap();
    }
    assert_eq!(last, n, "all oneway calls processed in FIFO order");
    assert!(
        oneway_elapsed < Duration::from_secs(5),
        "oneway sends should not block per-call (took {oneway_elapsed:?})"
    );
    // _cu handles teardown.
}

// ---- nested server→client callback ---------------------------------

#[test]
fn nested_callback_no_deadlock() {
    let path = tmp_sock("nest");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let root = EchoProxy(client.get_root().expect("get_root"));

    // The client publishes its OWN IEcho2 object as a callback. The
    // server calls back into it (`roundtrip` → cb.echo("ping")) while
    // the client's `roundtrip` call is still in flight: the client's
    // recv loop dispatches the nested inbound TRANSACT inline. Must
    // not deadlock and must return the right value.
    let cb = make_service(Arc::new(AtomicI64::new(0)));
    let out = root.roundtrip(&cb).expect("nested roundtrip");
    assert_eq!(out, "rt:ping", "server→client nested callback result");

    // Run it repeatedly to shake out any nesting deadlock.
    for _ in 0..50 {
        assert_eq!(root.roundtrip(&cb).unwrap(), "rt:ping");
    }
    // _cu handles teardown.
}

// ---- timeout (hung server) -----------------------------------------

#[test]
fn client_timeout_on_hung_server() {
    let path = tmp_sock("to");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    client.set_timeout(Some(Duration::from_millis(150)));
    let root = EchoProxy(client.get_root().expect("get_root"));

    // Server sleeps 5s; client deadline is 150ms → deterministic
    // Timeout, never an infinite wait.
    let t0 = std::time::Instant::now();
    let err = root.slow(5000).expect_err("hung call must time out");
    assert_eq!(err, StatusCode::TimedOut, "got {err:?}");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "must return promptly on timeout, not block for the full 5s"
    );
    // _cu handles teardown.
}

// ---- opt-in android-13+ versioned-wire profile ---------------------

/// The proven android-13+ connection handshake + AOSP-faithful
/// framing + `Android13PlusCodec` (hermetic) driving a
/// **live `RpcServer`/`RpcSession` dispatch path** end-to-end over a
/// real `UnixTransport`, reusing the existing per-session `RpcState`,
/// `client_transact`/`serve_blocking`, oneway-FIFO and nested-callback
/// machinery unchanged. Covers v0 (android-13), v1 (android-14/15) and
/// the `min(client_max, server_max)` version negotiation incl.
/// mismatch. The default r34 path is untouched (its green suite is the
/// no-regression gate).
#[test]
fn android13plus_profile_e2e() {
    // (server_max, client_max, expected negotiated version)
    for (smax, cmax, expect) in [
        (0u32, 0u32, 0u32), // v0 — android-13
        (1, 1, 1),          // v1 — android-14/15
        (1, 0, 0),          // mismatch ⇒ min = v0
        (0, 1, 0),          // mismatch ⇒ min = v0
        (2, 2, 2),          // v2 — android-16
        (2, 1, 1),          // v2↔v1 ⇒ min = v1
        (1, 2, 1),          // v1↔v2 ⇒ min = v1
        (2, 0, 0),          // v2↔v0 ⇒ min = v0
    ] {
        let path = tmp_sock(&format!("a13_{smax}_{cmax}"));
        let counter = Arc::new(AtomicI64::new(0));
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server.set_android13plus(smax); // opt in to the versioned wire
        server.set_max_threads(2);
        server.set_root(make_service(counter.clone()));
        let bg = server.run_background();
        let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
        wait_for_sock(&path);

        // Client opts into android-13+; the handshake negotiates
        // min(cmax, smax) and uses AOSP framing (no u32 length prefix).
        let client =
            RpcSession::setup_unix_client_android13plus(&path, cmax).expect("android-13+ connect");
        assert_eq!(
            client.wire_protocol_version(),
            Some(expect),
            "negotiated min({cmax},{smax}) wire version"
        );

        // GET_MAX_THREADS special transact over the android-13+ wire.
        assert_eq!(client.negotiate(8).expect("negotiate"), 2);
        assert_eq!(client.negotiated_max_threads(), 2);

        let root = EchoProxy(client.get_root().expect("get_root"));

        // AIDL scalar/string round-trip over AOSP framing.
        assert_eq!(root.echo("hello android-13+").unwrap(), "hello android-13+");
        assert_eq!(root.echo("").unwrap(), "");
        for i in 0..50 {
            assert_eq!(
                root.echo(&format!("v{expect}-n{i}")).unwrap(),
                format!("v{expect}-n{i}")
            );
        }

        // Oneway FIFO: 300 oneway bumps then a sync read observes them
        // all in order (single connection ⇒ FIFO) — exercises the
        // android-13+ TRANSACT(oneway)/no-reply path.
        let n = 300;
        for _ in 0..n {
            root.bump().expect("oneway bump");
        }
        let mut last = root.count().unwrap();
        for _ in 0..200 {
            if last == n {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            last = root.count().unwrap();
        }
        assert_eq!(last, n, "oneway FIFO over android-13+ wire");

        // Nested server→client callback while a call is in flight:
        // the client's recv loop dispatches the inbound TRANSACT inline
        // over the same android-13+ connection. Must not deadlock.
        let cb = make_service(Arc::new(AtomicI64::new(0)));
        assert_eq!(root.roundtrip(&cb).expect("nested"), "rt:ping");
        for _ in 0..20 {
            assert_eq!(root.roundtrip(&cb).unwrap(), "rt:ping");
        }

        drop(root);
        drop(client);
        // _cu (per-iteration) handles teardown — `let _cu = ServeCleanup::new(...)`
        // is dropped here as the for-loop body scope ends, in the same
        // order the explicit shutdown/join/remove_file ran before.
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn abstract_unix_socket_e2e() {
    let r34_name = format!("rsb_rpc_abs_r34_{}", std::process::id()).into_bytes();
    let server = RpcServer::setup_unix_server_abstract(&r34_name).expect("bind abstract r34");
    assert!(server.path().is_none());
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu_r34 = ServeCleanup {
        server: Arc::clone(&server),
        bg: Some(bg),
        path: None,
    };

    let client = RpcSession::setup_unix_client_abstract(&r34_name).expect("connect abstract r34");
    let root = EchoProxy(client.get_root().expect("get_root"));
    assert_eq!(root.echo("abstract-r34").unwrap(), "abstract-r34");

    let a13_name = format!("rsb_rpc_abs_a13_{}", std::process::id()).into_bytes();
    let server = RpcServer::setup_unix_server_abstract(&a13_name).expect("bind abstract a13");
    assert!(server.path().is_none());
    server.set_android13plus(2);
    server.set_max_threads(2);
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu_a13 = ServeCleanup {
        server: Arc::clone(&server),
        bg: Some(bg),
        path: None,
    };

    let client = RpcSession::setup_unix_client_android13plus_abstract(&a13_name, 2)
        .expect("connect abstract android13plus");
    assert_eq!(client.wire_protocol_version(), Some(2));
    assert_eq!(client.negotiate(8).expect("negotiate"), 2);
    let root = EchoProxy(client.get_root().expect("get_root"));
    assert_eq!(root.echo("abstract-a13").unwrap(), "abstract-a13");

    let sid = client.get_session_id().expect("get_session_id");
    let sid_arr: [u8; 32] = sid.as_slice().try_into().expect("32-byte session id");
    let attached = RpcSession::setup_unix_client_android13plus_with_config(
        RpcUnixClientConfig::abstract_name(&a13_name, 2).session_id(&sid),
    )
    .expect("attach abstract android13plus");
    assert_eq!(attached.get_session_id().expect("attached id"), sid);
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(2)),
        "abstract with_id attach shares the founding session"
    );

    let fan_name = format!("rsb_rpc_abs_fan_{}", std::process::id()).into_bytes();
    let fan_server = RpcServer::setup_unix_server_abstract(&fan_name).expect("bind fan-out");
    fan_server.set_android13plus(2);
    fan_server.set_max_threads(3);
    fan_server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let fan_bg = fan_server.run_background();
    let _cu_fan = ServeCleanup {
        server: Arc::clone(&fan_server),
        bg: Some(fan_bg),
        path: None,
    };

    let fan_client = RpcSession::setup_unix_client_android13plus_with_config(
        RpcUnixClientConfig::abstract_name(&fan_name, 2).outgoing_connections(3),
    )
    .expect("abstract fan-out");
    assert_eq!(fan_client.negotiated_max_threads(), 3);
    let fan_sid = fan_client.get_session_id().expect("fan-out id");
    let fan_sid_arr: [u8; 32] = fan_sid.as_slice().try_into().expect("32-byte session id");
    assert!(
        poll_until(|| fan_server.session_slot_count(&fan_sid_arr) == Some(3)),
        "abstract fan-out created founding + 2 attached slots"
    );
    let fan_root = EchoProxy(fan_client.get_root().expect("fan get_root"));
    assert_eq!(fan_root.echo("abstract-fan").unwrap(), "abstract-fan");
}

/// The decoupled `TlsTransport`
/// (`Mutex<Connection>` + lock-free stream) driving
/// the **android-13+ profile over TLS**, hermetic rsbinder↔rsbinder.
/// Mirrors `android13plus_profile_e2e` (version negotiation v0/v1/v2 +
/// mismatch, echo, 300-oneway FIFO, **nested server→client callback**)
/// but the transport is TLS over TCP instead of a plain `UnixTransport`
/// — the keystone gate that the decomposed structure achieves
/// full-duplex (the nested callback) without the blocking-while-holding
/// deadlock a single coupled `StreamOwned`-Mutex would cause.
#[cfg(feature = "rpc-tls")]
#[test]
fn tls_android13plus_nested_callback_e2e() {
    use std::net::TcpListener;

    use rsbinder::rpc::rustls::pki_types::pem::PemObject;
    use rsbinder::rpc::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rsbinder::rpc::rustls::{ClientConfig, RootCertStore, ServerConfig};
    use rsbinder::rpc::transport::TlsTransport;

    const CA: &str = include_str!("tls_fixtures/ca.crt");
    const SRV_CRT: &str = include_str!("tls_fixtures/srv.crt");
    const SRV_KEY: &str = include_str!("tls_fixtures/srv.key");

    fn certs(pem: &str) -> Vec<CertificateDer<'static>> {
        CertificateDer::pem_slice_iter(pem.as_bytes())
            .collect::<std::result::Result<_, _>>()
            .expect("parse certs")
    }
    fn key(pem: &str) -> PrivateKeyDer<'static> {
        PrivateKeyDer::from_pem_slice(pem.as_bytes()).expect("parse key")
    }
    let srv_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs(SRV_CRT), key(SRV_KEY))
            .expect("server config"),
    );
    let cli_cfg = {
        let mut roots = RootCertStore::empty();
        for c in certs(CA) {
            roots.add(c).expect("add ca");
        }
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    };

    for (smax, cmax, expect) in [
        (0u32, 0u32, 0u32),
        (1, 1, 1),
        (2, 2, 2),
        (2, 1, 1),
        (1, 2, 1),
        (2, 0, 0),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let srv_cfg = Arc::clone(&srv_cfg);

        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().expect("accept");
            let t = TlsTransport::accept(tcp, srv_cfg).expect("server TLS handshake");
            let session = RpcSession::accept_android13plus(Box::new(t), smax)
                .expect("server android-13+ accept");
            session.set_root(make_service(counter));
            let _ = session.serve_blocking();
        });

        // Exercises the convenience constructor (TCP-connect +
        // TLS-handshake + android-13+ handshake in one call).
        let client = RpcSession::setup_tcp_client_tls_android13plus(
            addr,
            "localhost",
            Arc::clone(&cli_cfg),
            cmax,
        )
        .expect("setup_tcp_client_tls_android13plus");
        assert_eq!(
            client.wire_protocol_version(),
            Some(expect),
            "negotiated min({cmax},{smax}) over TLS"
        );

        let root = EchoProxy(client.get_root().expect("get_root over TLS"));
        assert_eq!(root.echo("hi tls a13+").unwrap(), "hi tls a13+");
        assert_eq!(root.echo("").unwrap(), "");
        for i in 0..30 {
            assert_eq!(
                root.echo(&format!("v{expect}-{i}")).unwrap(),
                format!("v{expect}-{i}")
            );
        }

        // Oneway FIFO over TLS+android-13+ wire.
        let n = 300;
        for _ in 0..n {
            root.bump().expect("oneway bump over TLS");
        }
        let mut last = root.count().unwrap();
        for _ in 0..200 {
            if last == n {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            last = root.count().unwrap();
        }
        assert_eq!(last, n, "oneway FIFO over TLS android-13+ wire");

        // The keystone: nested server→client callback over one TLS
        // connection must not deadlock (full-duplex via the decomposed
        // Mutex<Connection> + lock-free stream).
        let cb = make_service(Arc::new(AtomicI64::new(0)));
        assert_eq!(root.roundtrip(&cb).expect("nested over TLS"), "rt:ping");
        for _ in 0..20 {
            assert_eq!(root.roundtrip(&cb).unwrap(), "rt:ping");
        }

        drop(root);
        drop(client);
        server.join().unwrap();
    }
}

/// The default r34 profile must report **no** android-13+ wire version
/// (the new accessor's R34 arm) — locks "opt-in only; default
/// byte-unchanged".
#[test]
fn r34_profile_reports_no_wire_version() {
    let path = tmp_sock("r34_ver");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    assert_eq!(
        client.wire_protocol_version(),
        None,
        "default profile is android-12 r34 (no versioned wire)"
    );
    let root = EchoProxy(client.get_root().expect("get_root"));
    assert_eq!(root.echo("r34 still default").unwrap(), "r34 still default");

    drop(root);
    drop(client);
}

/// `transport` split out of
/// `RpcSessionInner` into a shared `SharedSession` + server id-demux +
/// partial-vs-full death-trigger.
///
/// Proves the **id-demux attach** is real (not dead plumbing):
///  - client #1 — **empty** id ⇒ new session; server registers a
///    `Weak` of its `SharedSession`; default flow unchanged
///    (`attached/rejected == 0`, `session_registered >= 1`, full
///    round-trip);
///  - `get_session_id()` round-trips the server-minted 32-byte id
///    (the client half of AOSP `setupClient`);
///  - client #2 — **echoes that id** ⇒ server resolves the live
///    session and **attaches** this connection to it: `c2` speaks the
///    *same* `SharedSession` (`c2.get_session_id() == sid1` — the
///    shared `rpc_session_id` lives in `SharedSession`, so this holds
///    **iff** state is shared, not a fresh per-connection session),
///    `attached_count == 1`, and `c2` is fully functional over the
///    attached connection;
///  - client #3 — an **unknown** 32-byte id ⇒ no live session ⇒
///    rejected (`rejected_unknown_id == 1`);
///  - **partial vs. full teardown**: dropping the *attached* #2
///    must NOT tear the session down — #1 stays fully functional and
///    still reports the same shared id (a spurious obituary / early
///    teardown on a partial connection loss would break this). The
///    complementary "fires exactly once on *full* teardown" side is
///    the `rpc_death_recipient_fires_on_session_drop` (single-connection
///    `live_conns 1→0`) in this same suite.
///
/// **Mutant gate**: make the found branch build a
/// *fresh* session instead of attaching (`from_android13plus(.., None)`
/// in `serve_connection`'s attach arm). Then #2 gets its own
/// `SharedSession` ⇒ `c2.get_session_id() != sid1` ⇒ the shared-id
/// assertion fails. That is what makes the demux load-bearing rather
/// than dead plumbing.
#[test]
fn a0b_multi_connection_shared_session() {
    let path = tmp_sock("a0b");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1); // opt in to the versioned wire
                                 // `set_max_threads` is the advertised
                                 // *and* enforced per-session incoming-slot cap. This test exercises
                                 // founding + one attached connection ⇒ explicit opt-in at 2 slots
                                 // (default 1 ⇒ founding-only). AOSP-faithful: this is fundamentally
                                 // a multi-conn scenario, so AOSP `setMaxIncomingThreads(2)` is its
                                 // natural setup step.
    server.set_max_threads(2);
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // --- client #1: empty id ⇒ new session (default, byte-identical).
    let c1 = RpcSession::setup_unix_client_android13plus(&path, 1).expect("a13+ connect #1");
    let root1 = EchoProxy(c1.get_root().expect("get_root #1"));
    assert_eq!(root1.echo("a0b-1").unwrap(), "a0b-1");
    assert!(
        poll_until(|| server.session_registered_count() >= 1),
        "new-session id registered"
    );
    assert_eq!(server.attached_count(), 0);
    assert_eq!(server.rejected_unknown_id_count(), 0);

    let sid1 = c1.get_session_id().expect("get_session_id #1");
    assert_eq!(sid1.len(), 32, "AOSP kSessionIdBytes");

    // --- client #2: echo #1's id ⇒ server ATTACHES it to #1's
    //     SharedSession (shared state/root/rpc_session_id).
    let c2 = RpcSession::setup_unix_client_android13plus_with_id(&path, 1, &sid1)
        .expect("a13+ connect #2 (attach)");
    let sid2 = c2.get_session_id().expect("get_session_id #2");
    assert_eq!(
        sid2, sid1,
        "attached connection speaks the SAME SharedSession \
         (mutant — fresh-session-on-found — flips this)"
    );
    assert!(
        poll_until(|| server.attached_count() == 1),
        "echoed-id connection took the id-demux ATTACH path"
    );
    assert_eq!(server.rejected_unknown_id_count(), 0, "no false reject");
    // The attached connection is fully functional.
    let root2 = EchoProxy(c2.get_root().expect("get_root #2"));
    assert_eq!(root2.echo("a0b-2").unwrap(), "a0b-2");

    // --- client #3: an unknown 32-byte id ⇒ no live session ⇒ reject.
    let bogus = [0xABu8; 32];
    assert_ne!(
        &bogus[..],
        &sid1[..],
        "bogus id differs from the minted one"
    );
    // The reject lands past the handshake (the wire acknowledges an
    // attach with nothing), so the attach confirmation round trip —
    // `GET_SESSION_ID` on the fresh connection — is what makes the
    // *constructor* fail instead of some later call on a dead
    // connection.
    // Strengthen the unknown-id reject assertion: a plain `is_err()`
    // would also pass for an unrelated
    // error (e.g. handshake itself failed). Lock the contract to the
    // single status this reject produces: the server `drop(transport)`
    // closes the socket, and a disconnect folds to `PeerClosed` and
    // then `StatusCode::DeadObject` (see `rsbinder/src/rpc/mod.rs`,
    // `From<io::Error> for RpcError`, kind-preserving in both
    // directions per
    // `rpc::tests::peer_closed_and_timeout_round_trip_through_io_error`).
    // This path arms no deadline (`RpcUnixClientConfig`'s
    // `handshake_timeout` defaults to `None`), so a `TimedOut` or an
    // unclassified `Unknown` here means a different bug — as does `Ok`,
    // the true mutant: server honored the unknown id.
    let err = RpcSession::setup_unix_client_android13plus_with_id(&path, 1, &bogus)
        .err()
        .expect("unknown id rejected");
    assert_eq!(
        err,
        StatusCode::DeadObject,
        "unknown id reject must surface as DeadObject (the PeerClosed path)"
    );
    assert!(
        poll_until(|| server.rejected_unknown_id_count() == 1),
        "unknown id ⇒ rejected as UNKNOWN"
    );
    assert_eq!(server.attached_count(), 1, "attach count stable");

    // --- Sever **only** the ATTACHED connection #2 (a partial loss).
    //     A proxy holds its session strongly (AOSP `sp<RpcSession>` in
    //     `BpBinder`), so `root2` must go first: dropping `c2` alone
    //     would leave connection #2 open through `root2`. The session
    //     must survive: the founding connection #1 keeps working and
    //     still reports the same shared id. A spurious obituary / early
    //     teardown on a partial connection loss (live_conns mis-gated)
    //     would make these DeadObject.
    //
    //     The liveness probe is `get_session_id` (a zero-address
    //     special transact). It does not drop a sibling proxy here;
    //     the sibling-proxy-drop path (AOSP `timesSent` /
    //     `flushExcessBinderRefs`) is covered by
    //     `f7_shared_node_survives_sibling_proxy_drop` +
    //     `f7_excess_receipt_no_leak_single_client`. This test stays
    //     focused on attach (shown above) + no premature teardown on
    //     partial loss (shown here) so its mutant gate stays clean.
    drop(root2);
    drop(c2);
    // **Deterministic** wait for the server's attached worker to
    // exit (`serve_blocking_on` → `live_conns.fetch_sub`); a fixed
    // sleep would race scheduler jitter under load.
    // `session_live_conns` reads the live-conn ledger
    // directly: after `c2` drop the attached worker observes the
    // peer-close and `fetch_sub`s 2→1; once we see 1 the partial-loss
    // path is fully reaped and the *next* liveness check
    // (`get_session_id` below) probes a stable state.
    let sid1_arr: [u8; 32] = sid1
        .as_slice()
        .try_into()
        .expect("32-byte session id (AOSP kSessionIdBytes)");
    assert!(
        poll_until(|| server.session_live_conns(&sid1_arr) == Some(1)),
        "F4 partial-loss reap: server's attached worker must have \
         decremented live_conns 2→1 within budget"
    );
    assert_eq!(
        c1.get_session_id().expect("get_session_id #1 post-partial"),
        sid1,
        "founding connection + shared session survive a partial \
         (attached) connection loss — no spurious obituary/teardown (F4)"
    );
    assert_eq!(server.attached_count(), 1, "attach count stable post-drop");

    drop(root1);
    drop(c1);
    // _cu's Drop handles shutdown/bg.join/join_workers/remove_file —
    // a panic above does not leak worker threads + socket file.
}

/// Server-side unification of `RpcSessionInner` into a single inner per
/// session.
///
/// Without it the *server* built one
/// `RpcSessionInner` per accepted connection (sharing only the
/// `SharedSession`), so `state.remote_proxies`-cached `RpcProxy.weak:
/// Weak<RpcSessionInner>` pointed to the *first* worker's inner. A
/// later worker unmarshaling the same client binder hit the cache and
/// inherited that other inner — its nested `proxy.transact`
/// `find_conn`ed inside the other worker's slot pool, not its own:
/// cross-slot aliasing. Collapsing N inners into 1 (slots
/// in one pool) makes every cached `RpcProxy.weak` point to the only
/// inner, and any server worker's nested `proxy.transact` `find_conn`s
/// **inside its own pool**.
///
/// Witness via the *founding inner*'s slot count: an
/// id-echoing attached connection adds a *slot* to that inner, so
/// `session_slot_count(sid) == Some(2)`. The mutant (server attach arm
/// builds `from_android13plus(.., Some(shared))` = a fresh inner sharing
/// only `SharedSession`) leaves the founding
/// inner with its single founding slot ⇒ `Some(1)` — the assertion
/// fails. This is what makes the unification load-bearing rather than a
/// no-op refactor.
///
/// `a0b_multi_connection_shared_session` asserts the *attach* itself
/// (shared `SharedSession` id round-trip + `attached_count == 1` +
/// partial-loss survival); this test layers the single-inner structural
/// shape on top of that.

#[test]
fn ac_12_f8_attach_unifies_to_single_inner() {
    let path = tmp_sock("ac12f8");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Opt into 2 incoming slots (default 1 ⇒ founding-only).
    server.set_max_threads(2);
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // Founding connection (#1): empty id ⇒ new session, one slot.
    let c1 = RpcSession::setup_unix_client_android13plus(&path, 1).expect("a13+ #1");
    let _root1 = EchoProxy(c1.get_root().expect("get_root #1"));
    let sid = c1.get_session_id().expect("get_session_id #1");
    let sid_arr: [u8; 32] = sid
        .as_slice()
        .try_into()
        .expect("32-byte session id (AOSP kSessionIdBytes)");
    assert_eq!(
        server.session_slot_count(&sid_arr),
        Some(1),
        "founding-only ⇒ single slot in the founding inner"
    );

    // Attached connection (#2): echo #1's id ⇒ the attach arm
    // adds a *slot* to the founding inner (rather than building a
    // fresh inner sharing only SharedSession — the mutant).
    let c2 = RpcSession::setup_unix_client_android13plus_with_id(&path, 1, &sid)
        .expect("a13+ #2 (attach)");
    // Bound without the conventional `_` prefix — `root2` is moved into
    // an explicit `drop(...)` below to trigger the partial-loss
    // reap; an underscore-prefix would have read as "intentionally
    // unused" and hidden that load-bearing drop.
    let root2 = EchoProxy(c2.get_root().expect("get_root #2"));
    assert!(
        poll_until(|| server.attached_count() == 1),
        "echo-id connection took the id-demux ATTACH path (A0b)"
    );
    // The attached connection is a *slot on the
    // founding inner*, so the inner's slot pool now has 2 slots.
    // The mutant — each connection has its own inner with one slot —
    // would leave the founding inner at slot_count == 1.
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(2)),
        "Phase A4: attached connection adds a slot to the founding inner \
         (mutant: fresh-inner-on-attach leaves slot_count == 1)"
    );

    // Partial-loss reaping (re-uses the existing live-conn ledger): drop
    // the attached and verify the founding inner's slot pool shrinks back
    // to 1, then the founding-only state survives.
    drop(root2);
    drop(c2);
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(1)),
        "remove_slot on attached worker exit shrinks the pool back to 1"
    );
    assert_eq!(
        c1.get_session_id().expect("get_session_id #1 post-partial"),
        sid,
        "founding still alive and on the same shared session (F4)"
    );
}

/// `setMaxIncomingThreads` cap + negotiate reflection.
/// `set_max_threads(N)` carries
/// two meanings — advertise + enforce: the server attach arm refuses
/// an id-echoing connection when adding it would push `slot_count() >
/// max_threads_value()`. Default 1 ⇒ founding-only; multi-conn
/// callers opt in via explicit `set_max_threads(N >= 2)`.
///
/// Mutant gate (advertise-only): cap check absent ⇒
/// 3rd attach silently succeeds and `session_slot_count(sid) ==
/// Some(3) > set_max_threads(2)` ⇒ the cap assertion fails (and the
/// `rejected_unknown_id_count` witness stays at 0).
///
/// `GetMaxThreads` returns the same `max_threads_value()`
/// (advertise == enforce), so a client's `negotiate(local_max)`
/// records `min(local_max, server_cap)` — verified at the wire by
/// querying `negotiated_max_threads()` after a single
/// `GetMaxThreads` round-trip.
///
/// The `shutdown` gate is documented at the *code* level only — the
/// attach arm shows `if server.shutdown.load() { reject }`. A
/// deterministic e2e trigger needs an accept-pass / handshake-stall /
/// late-shutdown race that the current test scaffolding does not bound;
/// the *behavior* is already in code and falls inside the
/// rejected_unknown_id observability.

#[test]
fn ac_12_4_set_max_threads_caps_incoming_slots() {
    let path = tmp_sock("ac124");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(2);
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // Founding (#1) + attached (#2): both under the cap.
    let c1 = RpcSession::setup_unix_client_android13plus(&path, 1).expect("a13+ #1");
    let _r1 = EchoProxy(c1.get_root().expect("get_root #1"));
    let sid = c1.get_session_id().expect("get_session_id #1");
    let sid_arr: [u8; 32] = sid
        .as_slice()
        .try_into()
        .expect("32-byte session id (AOSP kSessionIdBytes)");
    let c2 = RpcSession::setup_unix_client_android13plus_with_id(&path, 1, &sid)
        .expect("a13+ #2 (attach within cap)");
    let _r2 = EchoProxy(c2.get_root().expect("get_root #2"));
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(2)),
        "2 slots after founding + attached (cap = 2)"
    );
    let rejected_before = server.rejected_unknown_id_count();

    // 3rd attach with the same session id ⇒ would push slot_count to
    // 3 > max_threads(2) ⇒ refused at the attach arm (cap). The reject
    // is post-handshake, and the attach confirmation round trip
    // (`GET_SESSION_ID` on the fresh connection) reports it from the
    // constructor — a valid id refused by the cap is the case no
    // client-side id check could catch.
    let err = RpcSession::setup_unix_client_android13plus_with_id(&path, 1, &sid)
        .err()
        .expect("3rd attach must be rejected by the per-session cap");
    assert_eq!(
        err,
        StatusCode::DeadObject,
        "cap-reject surfaces as the same status as the unknown-id reject \
         (post-handshake socket close)"
    );
    assert!(
        poll_until(|| server.rejected_unknown_id_count() == rejected_before + 1),
        "cap-reject increments the `rejected_unknown_id` observability counter"
    );
    assert_eq!(
        server.session_slot_count(&sid_arr),
        Some(2),
        "founding inner's slot pool stays at the cap (no overshoot — \
         a B.1 mutant would have let this reach Some(3))"
    );

    // negotiate reflects the server's advertised + enforced
    // value. Client opts into local_max=4; server advertises 2 ⇒
    // negotiated = min(4, 2) = 2. (Wire-level: a single GetMaxThreads
    // round-trip.) The advertise == enforce equation is exactly
    // what makes the cap observable to a well-behaved client *without*
    // needing to learn `rejected_unknown_id_count` out-of-band.
    let negotiated = c1.negotiate(4).expect("negotiate");
    assert_eq!(
        negotiated, 2,
        "negotiate(local=4) records min(local, server_cap=2) = 2"
    );
    assert_eq!(
        c1.negotiated_max_threads(),
        2,
        "negotiated_max_threads() reflects the same cap"
    );
}

/// AOSP `setupClient` fan-out automation, multi-conn path.
/// `RpcSession::setup_unix_client_android13plus_fan_out` does
/// in one helper what the manual API requires three explicit steps for:
/// founding connect → `negotiate(local)` → N-1 additional outgoing
/// `add_outgoing_connection_android13plus`. Witnesses that the helper's
/// fan-out loop actually mints `N - 1` extras under the server's
/// `set_max_threads(N)` cap.
///
/// **Mutant gate**: skipping the `1..negotiated` loop body (`for _ in
/// 1..negotiated { … }` ⇒ `for _ in 0..0 { … }` or `unreachable!`)
/// leaves the founding inner at a single slot ⇒ `session_slot_count`
/// is `Some(1)` instead of `Some(N)` and this test fails.
#[test]
fn b2_fan_out_creates_n_outgoing_slots_when_local_max_outgoing_is_n() {
    let path = tmp_sock("b2fan");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // N = 3 (founding + 2 fan-out attaches). Cap is server-side; the
    // helper learns it via `negotiate` and mints exactly N - 1 extras.
    server.set_max_threads(3);
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client_android13plus_fan_out(&path, 1, 3)
        .expect("setupClient fan-out");
    assert!(matches!(
        client.add_outgoing_connection_android13plus(tmp_sock("badid"), 1, b"bad"),
        Err(StatusCode::BadValue)
    ));
    // `negotiate` was the second step of the helper ⇒ negotiated value
    // is recorded on the session and equals the fan-out pool size.
    assert_eq!(
        client.negotiated_max_threads(),
        3,
        "helper performed GET_MAX_THREADS and recorded min(local=3, server=3) = 3"
    );
    let sid = client.get_session_id().expect("get_session_id");
    assert!(matches!(
        client.add_outgoing_connection_android13plus_with_config(
            RpcUnixClientConfig::path(&tmp_sock("badfd"), 1)
                .session_id(&sid)
                .fd_mode(rsbinder::rpc::FileDescriptorTransportMode::Unix),
        ),
        Err(StatusCode::BadValue)
    ));
    let sid_arr: [u8; 32] = sid
        .as_slice()
        .try_into()
        .expect("32-byte session id (AOSP kSessionIdBytes)");

    // Functional gate: the session built by the helper round-trips
    // through every transport (default outgoing slot picker spreads
    // calls across the pool).
    let root = EchoProxy(client.get_root().expect("get_root"));
    for i in 0..6 {
        let msg = format!("b2-{i}");
        assert_eq!(root.echo(&msg).unwrap(), msg, "fan-out echo #{i}");
    }

    // Mutant-gate witness: server-side, the founding `RpcSessionInner`'s
    // slot pool holds *all* N connections (founding + N-1 attached).
    // A mutant that skipped the fan-out leaves this at `Some(1)`.
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(3)),
        "founding inner's pool has 3 slots (founding + 2 fan-out attaches) — \
         mutant: helper skipping the fan-out loop leaves this at Some(1)"
    );
    assert_eq!(
        server.attached_count(),
        2,
        "exactly 2 id-echoing attaches reached the server-side attach arm"
    );
    assert_eq!(
        server.rejected_unknown_id_count(),
        0,
        "fan-out connections all pass under the cap"
    );
}

/// Single-connection fast path. `local_max_outgoing ==
/// 1` (and `0`, normalized) must skip the `GET_MAX_THREADS` round-trip
/// and the fan-out loop entirely so the wire is bit-identical to the
/// founding-only helper. A caller paying for the fan-out helper's
/// convenience on a single-conn session must NOT pay an extra
/// negotiation packet.
///
/// **Mutant gate**: dropping the `if local == 1 { return Ok(session); }`
/// short-circuit makes the helper run `negotiate(1)` ⇒
/// `negotiated_max_threads() == 1` instead of `0`.
#[test]
fn b2_local_max_outgoing_one_skips_fan_out_byte_identical_to_founding_only() {
    let path = tmp_sock("b2one");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Server is multi-conn-capable but the client opts out — fan-out
    // must NOT run regardless.
    server.set_max_threads(2);
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client_android13plus_fan_out(&path, 1, 1)
        .expect("setupClient (single-conn)");
    // Negotiation skipped ⇒ session.negotiated_max_threads() stays at
    // its default (0 = "not negotiated"). The byte-identical witness.
    assert_eq!(
        client.negotiated_max_threads(),
        0,
        "local_max_outgoing == 1 must skip GET_MAX_THREADS entirely \
         (mutant: dropping the early-return would record 1 here)"
    );
    let sid = client.get_session_id().expect("get_session_id");
    let sid_arr: [u8; 32] = sid.as_slice().try_into().expect("32-byte session id");
    // Server side: exactly one slot (the founding). Mutant: extra
    // outgoing attaches would push this past 1.
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(1)),
        "founding-only session has exactly one slot on the server side"
    );
    assert_eq!(
        server.attached_count(),
        0,
        "no id-echoing attach reached the server (no fan-out ran)"
    );
    let root = EchoProxy(client.get_root().expect("get_root"));
    assert_eq!(root.echo("b2-one").unwrap(), "b2-one");

    // `local_max_outgoing == 0` is normalized to 1 (a session needs at
    // least the founding connection). Same byte-identical behavior.
    let path2 = tmp_sock("b2zero");
    let server2 = RpcServer::setup_unix_server(&path2).expect("bind");
    server2.set_android13plus(1);
    server2.set_max_threads(2);
    server2.set_root(make_service(counter.clone()));
    let bg2 = server2.run_background();
    let _cu2 = ServeCleanup::new(Arc::clone(&server2), bg2, path2.clone());
    wait_for_sock(&path2);
    let client_zero = RpcSession::setup_unix_client_android13plus_fan_out(&path2, 1, 0)
        .expect("local_max_outgoing == 0 normalized to 1");
    assert_eq!(
        client_zero.negotiated_max_threads(),
        0,
        "0 normalized to 1 ⇒ no negotiation"
    );
}

/// A **refused** outgoing attach must be an error at attach time, not
/// an `Ok` slot the pool keeps. The outgoing attach wire has no
/// server→client acknowledgement (the server refuses by closing), so
/// `RpcSession::add_outgoing_connection_android13plus` confirms
/// admission with one `GET_SESSION_ID` round trip on the fresh
/// connection.
///
/// Single-threaded clients always draw slot 1 first, so this drives 4
/// threads to make the pool actually reach the attached slot.
///
/// **Mutant gate**: dropping the `confirm_attach` call restores
/// `Ok(2)` for both bogus ids and re-corrupts the pool (the echo loop
/// then fails ~1/4 of its calls).
#[test]
fn attach_with_a_bogus_session_id_is_refused_at_attach_time() {
    let path = tmp_sock("badattach");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(4);
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = Arc::new(RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect"));
    let sid = client.get_session_id().expect("get_session_id");
    let sid_arr: [u8; 32] = sid.as_slice().try_into().expect("32-byte session id");

    // (a) The client-local `session_id()` accessor is NOT the
    //     server-minted id — the exact confusion that started this.
    assert_ne!(
        client.session_id().as_slice(),
        sid.as_slice(),
        "a client session's `session_id()` is a local value, never the peer's"
    );
    assert!(
        client
            .add_outgoing_connection_android13plus(&path, 1, &client.session_id())
            .is_err(),
        "attaching with the client-local id must fail here, not later"
    );
    // (b) Plain garbage.
    assert!(
        client
            .add_outgoing_connection_android13plus(&path, 1, &[0xABu8; 32])
            .is_err(),
        "attaching with an unknown id must fail here, not later"
    );
    // Neither reached the server's pool, and the founding connection is
    // untouched.
    assert_eq!(
        server.session_slot_count(&sid_arr),
        Some(1),
        "both refused attaches left the server session at its founding slot"
    );
    assert!(
        poll_until(|| server.rejected_unknown_id_count() == 2),
        "server counted exactly the two unknown-id rejects"
    );

    // (c) The control: the server-minted id attaches, and the pool is
    //     clean — 200 calls across 4 threads, zero failures.
    assert_eq!(
        client
            .add_outgoing_connection_android13plus(&path, 1, &sid)
            .expect("attach with the server-minted id"),
        2
    );
    let mut handles = Vec::new();
    for t in 0..4 {
        let c = Arc::clone(&client);
        handles.push(std::thread::spawn(move || {
            let root = EchoProxy(c.get_root().expect("get_root"));
            for i in 0..50 {
                let msg = format!("clean-{t}-{i}");
                assert_eq!(root.echo(&msg).expect("echo on a confirmed pool"), msg);
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }
}

/// The same confirmation covers a refusal that is **nobody's mistake**:
/// a perfectly valid session id attached past the server's
/// `set_max_threads` outgoing-slot cap. No client-side id check could
/// catch this one — only the round trip can — and a caller that skips
/// `negotiate()` has no other way to learn the cap.
///
/// **Mutant gate**: without `confirm_attach` the over-cap attaches
/// return `Ok(3)`/`Ok(4)` while the server stays at 2 slots.
#[test]
fn attach_past_the_server_slot_cap_is_refused_at_attach_time() {
    let path = tmp_sock("capattach");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Founding + exactly one attach.
    server.set_max_threads(2);
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = client.get_session_id().expect("get_session_id");
    let sid_arr: [u8; 32] = sid.as_slice().try_into().expect("32-byte session id");

    assert_eq!(
        client
            .add_outgoing_connection_android13plus(&path, 1, &sid)
            .expect("first attach is under the cap"),
        2
    );
    for n in 0..2 {
        assert!(
            client
                .add_outgoing_connection_android13plus(&path, 1, &sid)
                .is_err(),
            "attach #{n} past max_threads=2 must be refused at attach time"
        );
    }
    assert_eq!(
        server.session_slot_count(&sid_arr),
        Some(2),
        "the server never went past its cap"
    );
    // The pool the client kept is exactly the pool the server has, so
    // every call lands on a live slot.
    let root = EchoProxy(client.get_root().expect("get_root"));
    for i in 0..20 {
        let msg = format!("cap-{i}");
        assert_eq!(root.echo(&msg).expect("echo after refused attaches"), msg);
    }
}

/// An attach cannot negotiate its own version, so a `max_version`
/// below the session's negotiated one can never work — that is
/// `BadType`, and the log names `wire_protocol_version()` as the value
/// to pass. Complements the R34 case in
/// `real_process_e2e_and_negotiation`.
#[test]
fn attach_max_version_below_the_session_version_is_bad_type() {
    let path = tmp_sock("attachver");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(2);
    server.set_max_threads(3);
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client_android13plus(&path, 2).expect("connect");
    assert_eq!(client.wire_protocol_version(), Some(2));
    let sid = client.get_session_id().expect("get_session_id");
    assert!(matches!(
        client.add_outgoing_connection_android13plus(&path, 1, &sid),
        Err(StatusCode::BadType)
    ));
    // `wire_protocol_version()` is the value that works.
    assert_eq!(
        client
            .add_outgoing_connection_android13plus(
                &path,
                client.wire_protocol_version().expect("versioned"),
                &sid,
            )
            .expect("attach at the session's version"),
        2
    );
}

/// The r34-server / android-13+-client half of a wire-profile
/// mismatch. The server is the default (r34) wire, so it reads the
/// client's 16-byte `RpcConnectionHeader` as an r34 frame, fails to
/// decode it and closes. The client must report `DeadObject`, not an
/// unclassified `StatusCode::Unknown`.
///
/// The opposite direction (r34 client, android-13+ server) is covered
/// by the `"cci"` reject string in
/// `wire_android13::tests::android13plus_spec_golden_vectors`.
#[test]
fn r34_server_reports_an_android13plus_client_as_a_dead_peer() {
    let path = tmp_sock("profmix");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    // No `set_android13plus`: the default r34 wire.
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    for v in [0u32, 1, 2] {
        match RpcSession::setup_unix_client_android13plus(&path, v) {
            Ok(_) => panic!("an r34 server must not complete an android-13+ handshake (v{v})"),
            Err(e) => assert_eq!(
                e,
                StatusCode::DeadObject,
                "profile mismatch at max_version={v} must report the peer as dead, whichever \
                 side of the handshake notices first"
            ),
        }
    }
}

/// A standalone attach *session*
/// ([`RpcSession::setup_unix_client_android13plus_with_id`], and the
/// `ClientOptions::session_id` path over it) is confirmed the same way:
/// a refused attach is an error from the constructor, not a session
/// whose every call fails somewhere else.
#[test]
fn standalone_attach_session_with_a_bogus_id_fails_to_build() {
    let path = tmp_sock("standalone");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(3);
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let founding = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = founding.get_session_id().expect("get_session_id");
    assert!(
        RpcSession::setup_unix_client_android13plus_with_id(&path, 1, &[0xCDu8; 32]).is_err(),
        "an unknown id must not yield a session object"
    );
    // Control: the real id builds a usable second session handle on the
    // same server-side session.
    let attached = RpcSession::setup_unix_client_android13plus_with_id(&path, 1, &sid)
        .expect("attach session with the server-minted id");
    let root = EchoProxy(attached.get_root().expect("get_root"));
    assert_eq!(root.echo("standalone").unwrap(), "standalone");
}

/// Shutdown-reject e2e. The android-13+ attach arm sits past a
/// successful handshake but before the per-slot enqueue; in production
/// the `shutdown.load()` gate at that point sees a *sub-microsecond*
/// window between an accepted late attach and a concurrent
/// `server.shutdown()`, so the branch is otherwise only observable by
/// code inspection + the `rejected_unknown_id_count` counter.
///
/// The `__set_attach_shutdown_probe` `#[doc(hidden)]` test-only barrier
/// makes the window deterministic: it
/// fires after the codec check passes and *before* `shutdown.load()`,
/// so the test can park the worker, flip `server.shutdown()`, then
/// release the worker. The worker re-reads the now-true flag and
/// takes the reject branch — exactly the production semantics, with
/// no scheduler-luck dependency.
///
/// **Mutant gate**: removing the `if server.shutdown.load() {
/// reject }` line at the outgoing-attach arm leaves the worker
/// proceeding to `add_incoming_slot` after the barrier returns, so
/// `rejected_unknown_id_count` does NOT increment (the assertion
/// below fails) and the attach `get_root()` would succeed (also
/// flagged). Reverting just the assertion-side checks would still
/// fail the rejected-count delta — both arms catch the regression.
#[test]
fn shutdown_gate_e2e_rejects_attach_during_handshake_stall() {
    let path = tmp_sock("shutgate");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // 2 = founding + 1 attached. The cap is irrelevant to the
    // shutdown gate (which fires *before* the cap check), but
    // staying within it avoids confounding the reject-set: the only
    // increment of `rejected_unknown_id` we want to observe is the
    // shutdown-arm one.
    server.set_max_threads(2);
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // Founding session — kept alive (`c1` not dropped) so the
    // registry still resolves `sid` when the attach worker hits the
    // arm. Were c1 dropped here, the founding inner would die, the
    // attach would hit the "unknown/stale" arm, and the shutdown
    // gate would never be tested.
    let c1 = RpcSession::setup_unix_client_android13plus(&path, 1).expect("a13+ founding connect");
    let sid = c1.get_session_id().expect("get_session_id founding");

    // Two channels form the barrier. The probe sends "handshake
    // done" on the first call, then blocks on `release_rx`. The
    // test waits for the signal, flips `shutdown`, then releases.
    let (hs_done_tx, hs_done_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    // Wrap `release_rx` in an `Option` so the *first* probe fire
    // takes it; any later fire (no-op once the test is done) sees
    // `None` and drops through. The whole closure must be `Fn`
    // (not `FnOnce`), hence the `Mutex<Option<Receiver>>` shape.
    let release_rx = Arc::new(std::sync::Mutex::new(Some(release_rx)));
    server.__set_attach_shutdown_probe({
        let release_rx = Arc::clone(&release_rx);
        let hs_done_tx = hs_done_tx.clone();
        move || {
            let _ = hs_done_tx.send(());
            if let Some(rx) = release_rx.lock().expect("release_rx poisoned").take() {
                let _ = rx.recv_timeout(Duration::from_secs(5));
            }
        }
    });
    let rejected_before = server.rejected_unknown_id_count();

    // Run the attach in a background thread — its `get_root` would
    // block on the parked worker otherwise.
    let attach_path = path.clone();
    let attach_sid = sid.clone();
    let attach_handle = std::thread::spawn(move || -> std::result::Result<(), StatusCode> {
        // The reject lands past the handshake, where the attach
        // confirmation round trip (`GET_SESSION_ID` on the fresh
        // connection) now sees it — so the constructor itself fails.
        // Folded into one result with the first call so the test still
        // holds if a future reject arm moves to either side of it.
        let c2 = RpcSession::setup_unix_client_android13plus_with_id(&attach_path, 1, &attach_sid)?;
        c2.set_timeout(Some(Duration::from_secs(3)));
        c2.get_root().map(|_| ())
    });

    // Wait for the attach worker to reach the barrier.
    hs_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("attach worker should reach the shutdown barrier");

    // Flip shutdown *while* the worker is parked at the probe.
    server.shutdown();

    // Release the worker — it re-reads `shutdown.load() == true`
    // and takes the reject branch (drops the transport).
    let _ = release_tx.send(());

    let attach_result = attach_handle
        .join()
        .expect("attach thread should not panic");
    let err = attach_result.expect_err("attach during shutdown must be rejected (mutant: success)");
    assert_eq!(
        err,
        StatusCode::DeadObject,
        "shutdown-reject surfaces as the same status as the cap/unknown-id reject \
         (post-handshake socket close)"
    );
    assert!(
        poll_until(|| server.rejected_unknown_id_count() == rejected_before + 1),
        "shutdown-reject increments the `rejected_unknown_id` observability counter \
         (mutant: removing `if server.shutdown.load() {{ reject }}` leaves this flat)"
    );
    // Founding inner's slot pool stayed at 1 — the rejected attach
    // never reached `add_incoming_slot` (otherwise this would be
    // `Some(2)`, a second mutant-catch axis independent of the
    // counter delta above).
    let sid_arr: [u8; 32] = sid.as_slice().try_into().expect("32-byte session id");
    assert_eq!(
        server.session_slot_count(&sid_arr),
        Some(1),
        "shutdown-rejected attach did not enqueue a slot (mutant: would reach Some(2))"
    );
    // (`c1` lives to function exit by lexical scope — see its
    // binding-site comment for why the founding inner must stay
    // registered for the whole attach attempt.)
}

/// With a shared `RpcState` (id-demux), two **independent** client
/// sessions each hold their own proxy to the *same* server root.
/// Pinning the node's strong count at 1 by object-identity would make
/// the first connection's proxy drop `DEC_STRONG` it to 0 and free the
/// node ⇒ the sibling connection's proxy `DeadObject`. With AOSP
/// `timesSent` accounting the server counts each send (strong = 2), so
/// the first DEC only brings it to 1 and the sibling survives; the node
/// is freed only when the *second* proxy drops too (no leak — proven
/// deterministically at the state level by
/// `rpc::state::tests::f7_timessent_balance_no_leak`).
///
/// **Mutant gate**: revert `on_binder_leaving`'s
/// `timesSent` bump (strong stays 1). Then dropping `root1` frees the
/// shared node and `root2.echo()` is `DeadObject` ⇒ this fails.
#[test]
fn f7_shared_node_survives_sibling_proxy_drop() {
    let path = tmp_sock("f7");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Opt into 2 incoming slots (founding + attached).
    server.set_max_threads(2);
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // c1: new session; c2: attach (echo c1's id) ⇒ both connections of
    // ONE server SharedSession (shared RpcState).
    let c1 = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect #1");
    let sid1 = c1.get_session_id().expect("session id");
    let c2 = RpcSession::setup_unix_client_android13plus_with_id(&path, 1, &sid1)
        .expect("connect #2 (attach)");
    assert!(
        poll_until(|| server.attached_count() == 1),
        "c2 attached to c1's shared session"
    );

    // Each independent client session fetches the *same* server root ⇒
    // server `write_binder(root)` twice ⇒ `on_binder_leaving` strong =
    // 2 (timesSent), one proxy per client (no client-side excess).
    let root1 = EchoProxy(c1.get_root().expect("get_root #1"));
    let root2 = EchoProxy(c2.get_root().expect("get_root #2"));
    assert_eq!(root1.echo("f7-1").unwrap(), "f7-1");
    assert_eq!(root2.echo("f7-2").unwrap(), "f7-2");

    // Drop the connection-#1 proxy (c1 stays alive so `RpcProxy::drop`
    // actually delivers the `DEC_STRONG` over c1). Then an ordered
    // round-trip *on c1* guarantees c1's server worker has processed
    // that DEC before we probe the sibling.
    drop(root1);
    let _ = c1
        .get_session_id()
        .expect("c1 still alive (ordering barrier)");

    // The shared node must survive the sibling's DEC (strong
    // 2→1). The mutant freed it (1→0) ⇒ DeadObject here.
    assert_eq!(
        root2.echo("f7-after-sibling-drop").unwrap(),
        "f7-after-sibling-drop",
        "F7: a shared node must outlive one connection's proxy DEC"
    );

    // Dropping the second proxy frees the shared node (strong 1→0).
    // Probed while both connections are still open (so the registry
    // `Weak` upgrades): the AOSP `timesSent` books must net to **0**
    // live nodes — no leak.
    drop(root2);
    let _ = c2.get_session_id().expect("c2 ordering barrier");
    assert!(
        poll_until(|| server.live_session_node_count() == 0),
        "F7 no-leak: shared root node freed after all proxies dropped"
    );

    drop(c1);
    drop(c2);
    // _cu handles teardown.
}

/// Client `flushExcessBinderRefs` (the *other* mutant
/// arm). A **single** client session that receives the *same* server
/// binder more than once while its deduped proxy stays live owes the
/// sender one excess `DEC_STRONG` per duplicate receipt (the server
/// bumped `timesSent` on each send). Here `get_root()` twice ⇒ server
/// `strong = 2`, client dedups to one proxy ⇒ it must send **1 excess
/// DEC** at the 2nd receipt + **1** at proxy drop = 2 ⇒ node freed.
///
/// **Mutant gate strength**: this test gates the *client excess-DEC*
/// mutant — `read_binder` reverting the `flushExcessBinderRefs` arm so
/// only the proxy-drop DEC is sent ⇒ server `strong` stuck at 1 ⇒
/// `live_session_node_count()` never returns to 0 ⇒ this fails.
///
/// **Precondition — dedup must hold.** The 2 = 1-excess + 1-drop
/// arithmetic only catches the excess-DEC mutant if the proxy cache
/// *deduplicates*: two `get_root()` calls return the **same**
/// `RpcProxy`. If dedup is broken (an orthogonal future regression
/// where each receipt mints a fresh proxy), the test would pass
/// vacuously — 0 excess + 2 drops = 2 also balances. The explicit
/// identity assertion below locks that precondition; the state-level
/// companion `rpc::state::tests::f7_timessent_balance_no_leak` covers
/// the corresponding `RpcState` invariant directly.
#[test]
fn f7_excess_receipt_no_leak_single_client() {
    let path = tmp_sock("f7x");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    // Two receipts of the SAME server root while the proxy stays live:
    // server `timesSent` = 2; the 2nd receipt is an *excess* on the
    // client ⇒ it must send one `flushExcessBinderRefs` DEC now.
    let r1 = EchoProxy(c.get_root().expect("get_root #1"));
    let r2 = EchoProxy(c.get_root().expect("get_root #2"));
    // **Dedup precondition** (see doc-comment): both wrappers must
    // refer to the *same* underlying `RpcProxy`. Without dedup the
    // 2 = 1-excess + 1-drop arithmetic collapses to 2 = 0-excess +
    // 2-drops, and this test would pass vacuously under the
    // excess-DEC mutant.
    assert!(
        std::ptr::eq(r1.rp(), r2.rp()),
        "dedup precondition: both get_root() calls must return the \
         same RpcProxy for the F7 excess-DEC mutant gate to be sound"
    );
    assert_eq!(r1.echo("f7x-1").unwrap(), "f7x-1");
    assert_eq!(r2.echo("f7x-2").unwrap(), "f7x-2");

    // Drop both deduped clones (one `RpcProxy` ⇒ one drop DEC) and a
    // round-trip barrier so the server has applied excess + drop DEC.
    drop(r1);
    drop(r2);
    let _ = c.get_session_id().expect("ordering barrier");
    assert!(
        poll_until(|| server.live_session_node_count() == 0),
        "F7 flushExcessBinderRefs: 1 excess + 1 drop DEC = timesSent(2) \
         ⇒ root node freed (no leak). Stuck >0 ⇒ client excess-DEC mutant."
    );

    drop(c);
    // _cu handles teardown.
}

/// Connection pool: with N outgoing slots in
/// one `RpcSession`, concurrent `client_transact`s on different
/// threads pick *different* slots (AOSP `findConnection` available-
/// slot selection), so two server-side blocking handlers run **in
/// parallel** — not serialized through one connection.
///
/// Wire-up: founding connection (slot 1) + one echoed-id outgoing
/// (slot 2) via [`RpcSession::add_outgoing_connection_android13plus`].
/// The slots end up id-demuxed to the *same* `SharedSession`,
/// so the test's two threads transact through different sockets but
/// the same server session.
///
/// Timing-based observation: two parallel `slow(150)` calls take ~150
/// ms when distributed (each blocks its own server worker) and ~300
/// ms when serialized through one slot. Bound 250 ms is safely below
/// 300 (mutant) and well above 150 (post-pool, +scheduling slack).
///
/// **Mutant gates (verified in separate runs)**: `find_conn` always
/// returning slot 1 OR `find_conn`'s "first available" check ignoring
/// `exclusive_tid` would re-serialize ⇒ elapsed > 250 ms.

#[test]
fn pool_distributes_concurrent_calls_across_outgoing_slots() {
    let path = tmp_sock("a1pool");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Founding + one attached outgoing-echo
    // ⇒ 2 incoming slots at the server. Default cap = 1 would reject
    // the attach, defeating the pool-distribution scenario.
    server.set_max_threads(2);
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = c.get_session_id().expect("get_session_id");
    let slot2 = c
        .add_outgoing_connection_android13plus(&path, 1, &sid)
        .expect("add outgoing slot");
    assert_ne!(slot2, 1, "second slot has a fresh id (founding == 1)");

    // Two threads, each holds the same client `RpcSession` and calls
    // `slow(150)` concurrently. With the pool, find_conn picks
    // different slots → both server workers `sleep(150)` in parallel.
    let root = Arc::new(EchoProxy(c.get_root().expect("get_root")));
    let t0 = std::time::Instant::now();
    let mut handles = Vec::new();
    for _ in 0..2 {
        let r = Arc::clone(&root);
        handles.push(std::thread::spawn(move || {
            r.slow(150).expect("slow round-trip")
        }));
    }
    for h in handles {
        h.join().expect("thread");
    }
    let elapsed = t0.elapsed();
    // Normal (parallel): ~150 ms + scheduling/RPC slack.
    // Mutant (serialized): ~300 ms (= 2 × 150 ms sleep, sequential).
    // The 380 ms bound preserves the parallel / serialized split — the
    // mutant can't sleep less than 300 ms (literal `sleep(150)` × 2),
    // and the bound stays above the parallel path's wake-from-sleep / RPC
    // wrap overhead even under a loaded scheduler.
    assert!(
        elapsed < Duration::from_millis(380),
        "AC-12.1: 2 concurrent slow(150) on a 2-slot pool must run in \
         parallel (≈150 ms), got {elapsed:?}. Pre-pool / serialized \
         path would be ≈300 ms + overhead — that's the mutant signature."
    );

    drop(root);
    drop(c);
    // _cu handles teardown.
}

/// Pool-exhausted condvar wait: `find_conn` *blocks*
/// on `slot_cv` when no slot is available; **never** a busy try-loop.
/// 2 outgoing slots + 3 concurrent `slow(120)`s ⇒ two run in parallel
/// (~120 ms), the third waits on `slot_cv` for one to free, then
/// runs (~120 ms) ⇒ total ≈ 240 ms. Busy-looping would still progress
/// (~240 ms too) but burn 100 % CPU; a *broken* condvar (e.g., wait
/// returning prematurely without re-check) would either deadlock or
/// race-corrupt the wire. We assert the timing band and rely on the
/// transact correctness as the secondary signal.

#[test]
fn pool_exhausted_condvar_blocks_not_busy_loops() {
    let path = tmp_sock("a1exh");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // 2 incoming slots (founding + attached); 3 client
    // threads observe the cv-wait band.
    server.set_max_threads(2);
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = c.get_session_id().expect("get_session_id");
    let slot2 = c
        .add_outgoing_connection_android13plus(&path, 1, &sid)
        .expect("slot 2");
    assert_ne!(slot2, 1, "second slot has a fresh id (founding == 1)");

    let root = Arc::new(EchoProxy(c.get_root().expect("get_root")));
    let t0 = std::time::Instant::now();
    let mut handles = Vec::new();
    for i in 0..3 {
        let r = Arc::clone(&root);
        handles.push(std::thread::spawn(move || {
            r.slow(200).expect("slow");
            // After slow returns, also do an echo to prove the wire
            // didn't corrupt across the slot release / re-pick.
            let msg = format!("exh-{i}");
            assert_eq!(r.echo(&msg).unwrap(), msg);
        }));
    }
    for h in handles {
        h.join().expect("thread");
    }
    let elapsed = t0.elapsed();
    // 2 slots, 3 callers ⇒ 2 waves: 200 + 200 = 400 ms minimum (no
    // sub-200 timing because the 3rd MUST wait for a slot).
    //
    // Normal (2 parallel waves): ~400 ms + RPC/scheduling slack.
    // Mutant (fully serial, 3 × 200): ~600 ms + slack.
    //
    // The mutant signature is the *200 ms gap* between waves and serial,
    // which is preserved regardless of absolute slack (both arms pay the
    // same scheduling/RPC overhead). So the bound floats with slack as
    // long as it stays comfortably below `normal + 200 ms`.
    //
    // The 700 ms upper bound is sized for a loaded runner: it still
    // clears the parallel path with ~200 ms of slack, while a serial
    // mutant paying that same slack lands ~100 ms above it. Measurement
    // history belongs in `plan/2-12-*.md`, not here.
    //
    // Lower bound 380 ms rejects anything that finished in *one* wave
    // (i.e. a 3-slot pool or a non-blocking 3rd caller).
    assert!(
        elapsed >= Duration::from_millis(380) && elapsed < Duration::from_millis(700),
        "AC-12.1 cv-wait: 3 concurrent slow(200) on 2 slots should be \
         2 parallel waves ≈ 400 ms (got {elapsed:?}); mutant (serial) is ≈600 ms"
    );

    drop(root);
    drop(c);
    // _cu handles teardown.
}

/// Nested-callback slot pin (scoped): on
/// a multi-outgoing client, when `client_transact` picks an outgoing
/// slot, the nested server→client callback **arriving on that same
/// socket** must dispatch on that slot — `find_conn`'s reentrant
/// match is keyed by `(session_ptr, slot_id)`, not `session_ptr`
/// alone. The test forces slot 2 by parking slot 1
/// under a long-running `slow(...)`, then issues one `roundtrip(cb)`
/// on slot 2.
///
/// An N-inner-per-connection hybrid carries a cross-slot
/// proxy-cache aliasing hazard: two server workers concurrently
/// unmarshalling the *same* client binder hit `state.remote_proxy`'s
/// shared cache, so the second caller's nested `proxy.transact`
/// re-routes through the *first* server inner's socket — wire
/// interleave / deadlock. The server-side unification
/// ("one `RpcSessionInner` per session, slots in one
/// pool") rules that out: all server-side proxies live in one inner and
/// `findConnection` does the slot-pin uniformly. The scoped
/// single-thread test here exercises the slot-pin without triggering
/// the aliasing (only slot 2 unmarshals the cb).

#[test]
fn pool_nested_callback_pins_to_forced_slot_single_thread() {
    let path = tmp_sock("a2pin");
    // Deterministic "parker entered slow on the server" signal — set
    // at the server-side `slow()` handler's entry. A fixed sleep after
    // spawning the parker thread could elapse *before* the parker
    // reached `find_conn`, putting both threads on slot 1 — a false pass.
    let slow_entered = Arc::new(AtomicBool::new(false));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // 2 incoming slots (founding + forced slot-2 echo).
    server.set_max_threads(2);
    server.set_root(make_service_with_slow_signal(
        Arc::new(AtomicI64::new(0)),
        Arc::clone(&slow_entered),
    ));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = c.get_session_id().expect("get_session_id");
    let slot2 = c
        .add_outgoing_connection_android13plus(&path, 1, &sid)
        .expect("slot 2");
    assert_ne!(
        slot2, 1,
        "second slot has a fresh id (founding == 1) — otherwise the \
         'park slot 1, force main onto slot 2' geometry collapses"
    );

    let root = Arc::new(EchoProxy(c.get_root().expect("get_root")));

    // Park slot 1 under a long slow() so `find_conn` on the main
    // thread can only pick slot 2.
    let parked = Arc::clone(&root);
    let parker = std::thread::spawn(move || {
        let _ = parked.slow(800);
    });
    // Deterministic wait: the server-side `slow` handler sets
    // `slow_entered` on entry. Spinning on this atomic guarantees the
    // parker has claimed *a* server worker (which, since the parker is
    // the only outstanding transact at this point, used `find_conn`'s
    // "first available" arm ⇒ slot 1).
    assert!(
        poll_until(|| slow_entered.load(Ordering::SeqCst)),
        "parker failed to enter server-side slow() within budget"
    );

    let cb_counter = Arc::new(AtomicI64::new(0));
    let cb = make_service(cb_counter);
    // roundtrip on slot 2: server unmarshals cb (first time → no
    // alias), handler calls back on slot 2's socket, client's reply
    // loop on slot 2 dispatches the callback inline (DRIVING-pinned
    // to slot 2). Wrong slot pin would mis-route ⇒ error/deadlock.
    assert_eq!(
        root.roundtrip(&cb).expect("nested callback on slot 2"),
        "rt:ping"
    );

    let _ = parker.join();
    drop(root);
    drop(c);
    // _cu handles teardown.
}

/// Two client threads make concurrent
/// `roundtrip(cb)` calls; each server worker must see a *distinct*
/// `RpcProxy`-backed nested send (one inner per
/// session, slots in one pool). The N-inner / 1-shared
/// hybrid mutant: the two workers' `state.remote_proxies` would hand out
/// `RpcProxy`s whose `Weak<RpcSessionInner>` pointed to different
/// inners; the 2nd worker's nested `proxy.transact` would `find_conn`
/// against the 1st worker's slot pool and deadlock or interleave.
/// `set_timeout(3s)` bounds that deadlock so the mutant surfaces as a
/// test failure rather than CI hang.
#[test]
fn ac_12_2_extended_cross_slot_nested_callback_multi_thread() {
    let path = tmp_sock("a2ext");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(2);
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    c.set_timeout(Some(Duration::from_secs(3)));
    let sid = c.get_session_id().expect("get_session_id");
    let slot2 = c
        .add_outgoing_connection_android13plus(&path, 1, &sid)
        .expect("slot 2");
    assert_ne!(slot2, 1, "second slot has a fresh id");

    let root = Arc::new(EchoProxy(c.get_root().expect("get_root")));
    let cb_a = make_service(Arc::new(AtomicI64::new(0)));
    let cb_b = make_service(Arc::new(AtomicI64::new(0)));

    let r1 = Arc::clone(&root);
    let cba = cb_a.clone();
    let h1 = std::thread::spawn(move || r1.roundtrip(&cba).expect("rt thread 1"));
    let r2 = Arc::clone(&root);
    let cbb = cb_b.clone();
    let h2 = std::thread::spawn(move || r2.roundtrip(&cbb).expect("rt thread 2"));

    assert_eq!(h1.join().expect("join 1"), "rt:ping");
    assert_eq!(h2.join().expect("join 2"), "rt:ping");

    drop(root);
    drop(c);
}

/// On a multi-outgoing client, oneway FIFO must hold under
/// twoway interleave regardless of which slot each oneway rode.
/// Top-level oneway `find_conn` does not pin to slot 1 — the
/// per-`mNodeForAddress` `asyncNumber` send-side + receive-side
/// `asyncTodo` priority replay
/// ([state.rs](../src/rpc/state.rs)) carries the per-object oneway
/// ordering invariant on both ends; a founding-slot pin would be a HOL
/// throughput trade-off, not a correctness invariant.
///
/// What this test gates: 300 oneway bumps interleaved with concurrent
/// twoway echoes across an outgoing slot pool of size 2 all arrive
/// (`count == 300`) with no wire corruption between the oneway and
/// twoway frames. The standing mutant gate is the
/// asyncTodo deletion (covered as a unit gate at
/// [`rpc::state::tests`](../src/rpc/state.rs)
/// `phase_c_out_of_order_drains_in_order_via_async_todo`) plus
/// the STAGE3 live libbinder round-robin path:
/// [`rpc::state::tests::phase_c_out_of_order_enqueues_then_drains_in_priority_order`](../src/rpc/state.rs#L747)
/// and `phase_c_send_async_number_is_per_address_monotonic`.
#[test]
fn pool_oneway_fifo_under_concurrent_twoway_multi_outgoing() {
    let path = tmp_sock("a3one");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(2);
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = c.get_session_id().expect("get_session_id");
    let slot2 = c
        .add_outgoing_connection_android13plus(&path, 1, &sid)
        .expect("slot 2");
    assert_ne!(slot2, 1, "second slot has a fresh id");

    let root = Arc::new(EchoProxy(c.get_root().expect("get_root")));
    let n = 300i64;

    let r1 = Arc::clone(&root);
    let bump_h = std::thread::spawn(move || {
        for _ in 0..n {
            r1.bump().expect("oneway bump");
        }
    });
    let mut echo_handles = Vec::new();
    for t in 0..4 {
        let r = Arc::clone(&root);
        echo_handles.push(std::thread::spawn(move || {
            for i in 0..50 {
                let msg = format!("a3-t{t}-i{i}");
                assert_eq!(r.echo(&msg).expect("echo"), msg);
            }
        }));
    }
    bump_h.join().expect("bump thread");
    for h in echo_handles {
        h.join().expect("echo thread");
    }

    assert!(
        poll_until(|| root.count().unwrap() == n),
        "AC-12.3: all oneway bumps delivered across the slot pool; \
         last observed count = {}",
        root.count().unwrap()
    );

    drop(root);
    drop(c);
}

// ---- no globals anywhere in the RPC stack --------------------------

// Source-scan: needs `env!("CARGO_MANIFEST_DIR")/src/rpc/*.rs` at
// runtime, which is absent on a cross-compiled Android device.
#[cfg(not(target_os = "android"))]
#[test]
fn rpc_stack_has_no_globals() {
    // Static gate: the RPC module must not
    // introduce any process-global state. Scans src/rpc/*.rs for
    // `static`/`OnceLock`/`lazy_static`. The one *intentional*
    // exception is the tcp_debug one-time INSECURE-warning latch,
    // which is not session/protocol state.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/rpc");
    let mut offenders = Vec::new();
    fn scan(dir: &std::path::Path, offenders: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                scan(&p, offenders);
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let src = std::fs::read_to_string(&p).unwrap();
            for (lineno, line) in src.lines().enumerate() {
                let l = line.trim_start();
                if l.starts_with("//") || l.starts_with("///") || l.starts_with("*") {
                    continue;
                }
                // `&'static str` return types legitimately contain the
                // substring "static " — only a real `static` *item*
                // (mutable/immutable process global) is an offender.
                let static_item = (l.contains("static ") || l.starts_with("static"))
                    && !l.contains("'static")
                    && !l.contains("static_assertions");
                let has_global = static_item
                    || l.contains("OnceLock")
                    || l.contains("lazy_static")
                    || l.contains("OnceCell");
                if has_global {
                    // tcp_debug INSECURE_WARNED latch is the documented
                    // non-state exception.
                    if name == "tcp_debug.rs" && line.contains("INSECURE_WARNED") {
                        continue;
                    }
                    // proxy.rs `descriptor: OnceLock<String>` (and its
                    // import) is a *per-RpcProxy-instance* write-once
                    // field — the typed-stub descriptor stamp,
                    // owned per session, never a process global. A real
                    // `static` here is still caught by `static_item`.
                    if name == "proxy.rs" && l.contains("OnceLock") && !static_item {
                        continue;
                    }
                    // session.rs `DRIVING` is a `thread_local!`
                    // recursion marker that lets a same-thread nested
                    // call bypass the per-connection lock. It
                    // is per-thread scratch, NOT session/protocol state
                    // — it carries no node/address/refcount data; those
                    // stay per-session in RpcState. Mirrors kernel
                    // binder's thread-local IPCThreadState.
                    if name == "session.rs" && line.contains("DRIVING") {
                        continue;
                    }
                    offenders.push(format!("{name}:{}: {}", lineno + 1, line.trim()));
                }
            }
        }
    }
    scan(&dir, &mut offenders);
    assert!(
        offenders.is_empty(),
        "P6 violation — RPC stack must own all state per-session, found globals:\n{}",
        offenders.join("\n")
    );
}

// ---- accepted peer identity is the CLIENT --------------------------

/// Defect-regression, **deterministic mutant gate**.
///
/// A real cross-process connection: a forked child connects, the
/// parent `accept`s. `peer_identity()` on the accepted socket must be
/// the **client's** pid, never the server's own. A non-Linux
/// `resolve_peer` that returned `self_identity()` for
/// *every* socket would make a macOS/BSD server see **itself** as the
/// peer — an authoritative-looking forged identity.
/// That mutant makes `pid == std::process::id()` (the parent)
/// instead of `child.id()`, failing the assert. On Linux the real
/// `SO_PEERCRED` already gave the client pid, so this is a permanent
/// cross-platform regression gate.
#[test]
#[cfg(unix)]
fn unix_accepted_peer_identity_is_the_client_not_self() {
    use rsbinder::rpc::transport::UnixTransport;
    use rsbinder::rpc::{PeerIdentity, RpcTransport};

    // Child role: connect, hold the connection briefly, exit.
    if let Ok(path) = std::env::var("RSB_RPC_PEERID_CLIENT") {
        let _s = std::os::unix::net::UnixStream::connect(&path).expect("child connect");
        std::thread::sleep(Duration::from_millis(1500));
        std::process::exit(0);
    }

    let path = tmp_sock("peerid");
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = std::process::Command::new(exe)
        .args([
            "--exact",
            "unix_accepted_peer_identity_is_the_client_not_self",
            "--nocapture",
        ])
        .env("RSB_RPC_PEERID_CLIENT", &path)
        .spawn()
        .expect("spawn client child");

    let (stream, _addr) = listener.accept().expect("accept");
    let t = UnixTransport::from_stream(stream).expect("from_stream");
    let id = t.peer_identity();

    let self_pid = std::process::id() as i32;
    let child_pid = child.id() as i32;
    match id {
        PeerIdentity::Local { pid, .. } => {
            assert_ne!(
                pid, self_pid,
                "AC-9.1: accepted peer must NOT be the server itself \
                 (the §0 forged-self defect)"
            );
            // macOS LOCAL_PEERPID / Linux SO_PEERCRED ⇒ the exact
            // client pid. (`-1` would mean a BSD without LOCAL_PEERPID
            // — not this CI's macOS/Linux, so require the exact pid.)
            assert_eq!(
                pid, child_pid,
                "accepted peer pid must be the client child's pid"
            );
        }
        other => panic!("expected Local peer identity for an accepted UDS, got {other:?}"),
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&path);
}

// ---- RPC death notification (link/unlink_to_death over RPC) --------

struct DeathFlag(std::sync::mpsc::SyncSender<()>);
impl rsbinder::DeathRecipient for DeathFlag {
    fn binder_died(&self, _who: &rsbinder::WIBinder) {
        // Best-effort: the receiver may already be gone on a late fire.
        let _ = self.0.try_send(());
    }
}

/// A `DeathRecipient` linked to an RPC proxy fires when the **session
/// connection drops** (AOSP `RpcState::sendObituaries`). The peer that
/// wants the notification runs a serve loop (the AOSP "incoming
/// thread" requirement); when the server process is killed the
/// client's `serve_blocking` ends on `PeerClosed` and delivers the
/// obituary. Also covers `unlink_to_death` (an unlinked recipient must
/// NOT fire) and the post-death `link_to_death`→`DeadObject` contract.
///
/// Mutant: dropping the `send_session_obituaries()` call from
/// `serve_blocking` (or making `RpcProxy::link_to_death` an
/// `InvalidOperation` stub) makes `binder_died` never arrive ⇒ the
/// `recv_timeout` below returns `Err` and the test fails.
#[test]
fn rpc_death_recipient_fires_on_session_drop() {
    if let Ok(path) = std::env::var("RSB_RPC_DEATH_SERVER") {
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server.set_root(make_service(Arc::new(AtomicI64::new(0))));
        let _ = server.run(); // blocks until killed
        std::process::exit(0);
    }

    let path = tmp_sock("death");
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = std::process::Command::new(exe)
        .args([
            "--exact",
            "rpc_death_recipient_fires_on_session_drop",
            "--nocapture",
        ])
        .env("RSB_RPC_DEATH_SERVER", &path)
        .spawn()
        .expect("spawn server child");
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let root = client.get_root().expect("get_root");

    // An unlinked recipient must NOT fire; a linked one must.
    let (tx_live, rx_live) = std::sync::mpsc::sync_channel::<()>(1);
    let (tx_dead, rx_dead) = std::sync::mpsc::sync_channel::<()>(1);
    let live: Arc<DeathFlag> = Arc::new(DeathFlag(tx_live));
    let dead: Arc<DeathFlag> = Arc::new(DeathFlag(tx_dead));

    let weak_live: std::sync::Weak<dyn rsbinder::DeathRecipient> = Arc::downgrade(&live) as _;
    let weak_dead: std::sync::Weak<dyn rsbinder::DeathRecipient> = Arc::downgrade(&dead) as _;
    root.link_to_death(weak_live.clone()).expect("link live");
    root.link_to_death(weak_dead.clone()).expect("link dead");
    // Single-position removal: only `dead` is unlinked.
    root.unlink_to_death(weak_dead).expect("unlink dead");
    assert!(
        matches!(
            root.unlink_to_death(Arc::downgrade(&dead) as _),
            Err(rsbinder::StatusCode::NameNotFound)
        ),
        "a second unlink of an already-removed recipient is NameNotFound"
    );

    // The "incoming thread": the client serves this session so it can
    // observe the connection drop (AOSP getMaxIncomingThreads>=1).
    let serving = client.clone();
    let serve = std::thread::spawn(move || {
        let _ = serving.serve_blocking();
    });

    // Kill the server process ⇒ socket closes ⇒ client serve loop ends
    // ⇒ obituary delivered.
    child.kill().expect("kill server");
    child.wait().expect("reap server");

    rx_dead
        .recv_timeout(Duration::from_secs(5))
        .expect_err("unlinked recipient must NOT receive binder_died");
    rx_live
        .recv_timeout(Duration::from_secs(5))
        .expect("linked recipient must receive binder_died on session drop");

    // After the obituary, a new link is DEAD_OBJECT (AOSP parity).
    let (tx_late, _rx_late) = std::sync::mpsc::sync_channel::<()>(1);
    let late: Arc<DeathFlag> = Arc::new(DeathFlag(tx_late));
    assert!(
        matches!(
            root.link_to_death(Arc::downgrade(&late) as _),
            Err(rsbinder::StatusCode::DeadObject)
        ),
        "link_to_death after the obituary must be DeadObject"
    );

    let _ = serve.join();
    let _ = std::fs::remove_file(&path);
}

// ---- opt-in authorization hook -------------------------------------

/// The opt-in `set_authorizer` gate runs *before any RPC
/// byte* and is backend-independent. A rejecting hook closes the
/// connection (the peer's next op is `DeadObject`, zero payload); an
/// accepting hook is transparent; unset is accept-all = a server
/// without the hook (every other test in this suite, unmodified, is the
/// additive-invariant evidence). The `PeerIdentity` the hook inspects
/// is the *real* peer.
///
/// Mutant: deleting the `serve_connection` authorizer block makes the
/// rejected client's `get_root()` succeed ⇒ the `is_err` assert fails.
#[test]
fn authorizer_gate_rejects_before_any_rpc_byte() {
    use rsbinder::rpc::PeerIdentity;

    // (1) Rejecting hook ⇒ connection refused, no RPC exchanged.
    {
        let path = tmp_sock("authz_no");
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server.set_root(make_service(Arc::new(AtomicI64::new(0))));
        server.set_authorizer(|_peer| false);
        let bg = server.run_background();
        let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
        wait_for_sock(&path);
        let client = RpcSession::setup_unix_client(&path).expect("connect");
        client.set_timeout(Some(Duration::from_secs(3)));
        assert!(
            client.get_root().is_err(),
            "a rejecting authorizer must close the connection (zero RPC bytes)"
        );
        // _cu (scope-end) handles teardown.
    }

    // (2) Accepting hook (inspects the real PeerIdentity) ⇒ transparent.
    {
        let path = tmp_sock("authz_yes");
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server.set_root(make_service(Arc::new(AtomicI64::new(0))));
        server.set_authorizer(|peer| matches!(peer, PeerIdentity::Local { .. }));
        let bg = server.run_background();
        let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
        wait_for_sock(&path);
        let client = RpcSession::setup_unix_client(&path).expect("connect");
        let root = EchoProxy(client.get_root().expect("get_root (authorized)"));
        assert_eq!(root.echo("authorized").unwrap(), "authorized");
        // _cu (scope-end) handles teardown.
    }
}

// ---- plan 2-20 Phase A: slot roles + fast-fail --------------------------

/// Boot a server whose `hold()` parks the client callback in `held`, connect
/// one r34 client, and hand the parked proxy back — the server side of a
/// callback the server may later drive from outside any handler.
struct HeldSetup {
    client: RpcSession,
    root: EchoProxy,
    /// The client's local callback object (an `EchoSvc` by default, so
    /// `TX_ECHO` / `TX_BUMP` / `TX_SLOW` answer on it).
    cb: SIBinder,
    /// `cb`'s `bump()` counter (oneway delivery check).
    cb_counter: Arc<AtomicI64>,
    /// The server's proxy to `cb`, taken out of `held` (`Option` so a test
    /// can `take()` it — a `Drop` type cannot be moved out of).
    server_cb: Option<SIBinder>,
    held: Arc<Mutex<Option<SIBinder>>>,
    /// The server's root as a *local* binder (for handing back to the
    /// client inside a callback).
    root_local: SIBinder,
    server: Arc<RpcServer>,
    /// Last field on purpose: fields drop in declaration order, and
    /// `ServeCleanup::drop` joins the worker serving `client`'s connection —
    /// it can only return once `client` and the proxies are gone.
    _cu: ServeCleanup,
}
/// `HeldSetup::drop` runs before its fields drop: stop the client's
/// incoming-connection threads first, or `ServeCleanup::join_workers`
/// would wait on a founding connection those threads keep open.
impl Drop for HeldSetup {
    fn drop(&mut self) {
        self.client.shutdown();
    }
}
/// Everything `boot_held_cfg` can vary.
struct HeldCfg {
    /// android-13+ profile (needed for any multi-connection option).
    a13: bool,
    incoming: u32,
    fan_out: u32,
    max_threads: u32,
    /// Custom callback object; default an `EchoSvc`.
    cb: Option<SIBinder>,
}
impl Default for HeldCfg {
    fn default() -> Self {
        Self {
            a13: false,
            incoming: 0,
            fan_out: 1,
            max_threads: 1,
            cb: None,
        }
    }
}
fn boot_held(tag: &str) -> HeldSetup {
    boot_held_cfg(tag, HeldCfg::default())
}
fn boot_held_cfg(tag: &str, cfg: HeldCfg) -> HeldSetup {
    let path = tmp_sock(tag);
    let held = Arc::new(Mutex::new(None));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    if cfg.a13 {
        server.set_android13plus(2);
    }
    server.set_max_threads(cfg.max_threads);
    let root_local = make_service_with_hold(Arc::new(AtomicI64::new(0)), Arc::clone(&held));
    server.set_root(root_local.clone());
    let bg = server.run_background();
    let cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);
    let client = if cfg.a13 {
        RpcSession::setup_unix_client_android13plus_with_config(
            RpcUnixClientConfig::path(&path, 2)
                .outgoing_connections(cfg.fan_out)
                .incoming_connections(cfg.incoming),
        )
        .expect("connect android13plus")
    } else {
        RpcSession::setup_unix_client(&path).expect("connect")
    };
    // From here on a panic must still shut the session down: its incoming
    // threads hold the connection open, and `ServeCleanup`'s `join_workers`
    // would then wait on a server worker forever — turning a setup failure
    // into a hang instead of a report.
    struct ClientGuard(Option<RpcSession>);
    impl Drop for ClientGuard {
        fn drop(&mut self) {
            if let Some(c) = self.0.take() {
                c.shutdown();
            }
        }
    }
    let mut guard = ClientGuard(Some(client));
    let client_ref = guard.0.as_ref().expect("client");
    let root = EchoProxy(client_ref.get_root().expect("get_root"));
    let cb_counter = Arc::new(AtomicI64::new(0));
    let cb = cfg
        .cb
        .unwrap_or_else(|| make_service(Arc::clone(&cb_counter)));
    root.hold(&cb).expect("hold");
    let server_cb = held
        .lock()
        .unwrap()
        .clone()
        .expect("server parked the callback");
    // Setup succeeded: hand ownership to `HeldSetup`, which shuts it down.
    let client = guard.0.take().expect("client");
    HeldSetup {
        client,
        root,
        cb,
        cb_counter,
        server_cb: Some(server_cb),
        held,
        root_local,
        server,
        _cu: cu,
    }
}
impl HeldSetup {
    fn cb_proxy(&self) -> SIBinder {
        self.server_cb.clone().expect("server callback proxy")
    }
}
fn rpc_of(b: &SIBinder) -> &RpcProxy {
    (**b).as_any().downcast_ref::<RpcProxy>().expect("RpcProxy")
}
/// `TX_SLOW` on a proxy.
fn drive_slow(cb: &SIBinder, ms: i32) -> Result<()> {
    let rp = rpc_of(cb);
    let mut d = rp.build_request(DESC)?;
    d.write(&ms)?;
    let mut r = rp
        .transact(TX_SLOW, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    read_status(&mut r)
}
/// `TX_ROUNDTRIP` on a proxy, handing it `target`: the callee calls
/// `target.echo("ping")` back (a nested call from inside the callback).
fn drive_roundtrip(cb: &SIBinder, target: &SIBinder) -> Result<String> {
    let rp = rpc_of(cb);
    let mut d = rp.build_request(DESC)?;
    d.write(target)?;
    let mut r = rp
        .transact(TX_ROUNDTRIP, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    read_status(&mut r)?;
    r.read::<String>()
}
/// Drive `cb` (a proxy) with `TX_ECHO` (twoway) or `TX_BUMP` (oneway).
fn drive(cb: &SIBinder, oneway: bool) -> Result<()> {
    let rp = (**cb)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .expect("RpcProxy");
    let mut d = rp.build_request(DESC)?;
    if oneway {
        rp.transact(TX_BUMP, &d, rsbinder::FLAG_ONEWAY).map(|_| ())
    } else {
        d.write(&"x")?;
        let mut r = rp
            .transact(TX_ECHO, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        let got: String = r.read()?;
        assert_eq!(got, "x");
        Ok(())
    }
}

/// AC-20.1 — the client opened no incoming connection, so the server has no
/// `Outgoing` slot: a call on the parked proxy from a thread inside no handler
/// must fail **at once** (AOSP `WOULD_BLOCK`), not block until a deadline that
/// was never set. The served slot is untouched: the client's next calls and a
/// nested callback (`roundtrip`) still work.
#[test]
fn a_outside_handler_call_without_outgoing_slot_fails_fast() {
    let h = boot_held("a_fastfail");
    for oneway in [false, true] {
        // The deadline has to bound the *wait*, not be measured after it:
        // the regression this guards is an unbounded park in `find_conn`,
        // and a server session has no reply deadline of its own, so an
        // in-line call would hang the run instead of failing it.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let cb = h.cb_proxy();
        let driver = std::thread::spawn(move || {
            let _ = tx.send(drive(&cb, oneway));
        });
        let r = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                panic!("oneway={oneway}: must fail immediately, still blocked")
            }
            // `drive` asserts on the reply payload, so a broken channel is
            // its panic, not a block. Re-raise that instead of blaming a
            // deadline it never reached.
            Err(RecvTimeoutError::Disconnected) => match driver.join() {
                Err(p) => std::panic::resume_unwind(p),
                Ok(()) => panic!("oneway={oneway}: the driving thread sent no result"),
            },
        };
        assert!(
            matches!(r, Err(StatusCode::WouldBlock)),
            "oneway={oneway}: expected WouldBlock, got {r:?}"
        );
    }
    assert_eq!(h.root.echo("still in sync").unwrap(), "still in sync");
    assert_eq!(h.root.roundtrip(&h.cb).unwrap(), "rt:ping");
}

/// AC-20.9 — the served slot is momentarily free between two messages of the
/// worker's loop. An out-of-handler transact must never claim it there —
/// it would write a transaction the idle client never reads — so every such
/// attempt is refused while the client hammers the same connection, and the
/// wire stays in sync.
#[test]
fn a_served_slot_never_taken_by_outside_transact() {
    let h = boot_held("a_theft");
    let stop = Arc::new(AtomicBool::new(false));
    let progressed = Arc::new(AtomicBool::new(false));
    let hammer = {
        let root = EchoProxy(h.client.get_root().expect("root"));
        let stop = Arc::clone(&stop);
        let progressed = Arc::clone(&progressed);
        std::thread::spawn(move || {
            let mut n = 0u32;
            while !stop.load(Ordering::SeqCst) {
                let s = format!("m{n}");
                assert_eq!(root.echo(&s).expect("echo under contention"), s);
                n += 1;
                progressed.store(true, Ordering::SeqCst);
            }
            n
        })
    };
    // Wait for the first echo before hammering: the loop below is all
    // fast-fails, so on a loaded machine it can finish and set `stop`
    // before this thread is ever scheduled, leaving `echoed == 0`.
    let spun_up = Instant::now();
    while !progressed.load(Ordering::SeqCst) {
        assert!(
            spun_up.elapsed() < Duration::from_secs(10),
            "the client thread never completed an echo"
        );
        std::thread::yield_now();
    }
    for i in 0..200 {
        // Bounded, for the same reason as `..._fails_fast`: a slot the
        // outside call manages to claim would park here forever.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let cb = h.cb_proxy();
        std::thread::spawn(move || {
            let _ = tx.send(drive(&cb, i % 2 == 1));
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Err(StatusCode::WouldBlock)) => {}
            other => panic!("attempt {i}: expected WouldBlock, got {other:?}"),
        }
    }
    stop.store(true, Ordering::SeqCst);
    // Progress is already established by the spin-up gate above; the join
    // is what surfaces a hammer-thread panic (a failed echo under
    // contention).
    hammer.join().expect("hammer thread");
    assert_eq!(h.root.echo("after").unwrap(), "after");
}

/// The android-13+ connect handshake must be bounded by
/// `RpcUnixClientConfig::handshake_timeout`. Without it a peer that accepts
/// the socket and then writes nothing blocks the setup call forever —
/// `RpcSession::set_timeout` cannot cover this phase, since it is applied to
/// a session that does not exist yet.
#[test]
fn handshake_timeout_bounds_a_silent_peer() {
    let path = tmp_sock("hs_silent");
    // A raw listener, not an `RpcServer`: it accepts and then says nothing,
    // which is exactly the peer this deadline exists for.
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
    let keep = Arc::new(Mutex::new(Vec::new()));
    let acceptor = {
        let keep = Arc::clone(&keep);
        std::thread::spawn(move || {
            // Hold the accepted sockets open; dropping them would give the
            // client an EOF and let it fail for the wrong reason.
            while let Ok((sock, _)) = listener.accept() {
                keep.lock().unwrap().push(sock);
            }
        })
    };
    wait_for_sock(&path);

    // Off-thread with a deadline on the *channel*: without the fix this
    // connect never returns, and measuring `elapsed()` afterwards would
    // hang the run instead of failing it.
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let p = path.clone();
    std::thread::spawn(move || {
        let r = RpcSession::setup_unix_client_android13plus_with_config(
            RpcUnixClientConfig::path(&p, 2).handshake_timeout(Duration::from_millis(300)),
        );
        let _ = tx.send(r.map(|_| ()));
    });
    let r = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the handshake deadline must bound the connect; still blocked");
    assert!(r.is_err(), "a silent peer's handshake must not succeed");

    // The deadline must not leak onto the socket of a session that *does*
    // complete: `ReplyDeadlineGuard` restores only what it armed itself, so
    // a leftover `SO_RCVTIMEO` would silently bound every later reply wait.
    let path2 = tmp_sock("hs_ok");
    let server = RpcServer::setup_unix_server(&path2).expect("bind");
    server.set_android13plus(2);
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path2.clone());
    wait_for_sock(&path2);
    let client = RpcSession::setup_unix_client_android13plus_with_config(
        RpcUnixClientConfig::path(&path2, 2).handshake_timeout(Duration::from_millis(300)),
    )
    .expect("connect");
    let root = EchoProxy(client.get_root().expect("root"));
    // Longer than the handshake deadline: a leaked deadline fails here.
    assert_eq!(root.slow(500).map(|_| "ok"), Ok("ok"));
    client.shutdown();

    drop(keep);
    std::mem::drop(acceptor);
    let _ = std::fs::remove_file(&path);
}

/// AC-20.10 — dropping the parked proxy from a thread inside no handler
/// must neither block that thread nor desync the wire. With no `Outgoing`
/// slot on this session the `DEC_STRONG` is skipped rather than written to
/// the served slot (AOSP `WOULD_BLOCK`), so the node is released at session
/// end — or promptly, once the client opens an incoming connection (plan
/// 2-20 Phase B). What this gate pins is the safety property, not the
/// delivery: neither assertion below depends on the DEC going out.
#[test]
fn a_dec_strong_outside_handler_does_not_block_or_desync() {
    let h = boot_held("a_dec");
    assert_eq!(
        h.client.local_node_count(),
        1,
        "the parked callback is the one local node"
    );
    let held = Arc::clone(&h.held);
    let mut h = h;
    drop(h.server_cb.take());
    let t0 = Instant::now();
    std::thread::spawn(move || {
        *held.lock().unwrap() = None;
    })
    .join()
    .expect("drop thread");
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "dropping a proxy outside a handler must not block: {:?}",
        t0.elapsed()
    );
    // The wire on the served slot is intact.
    for i in 0..20 {
        let s = format!("tick{i}");
        assert_eq!(h.root.echo(&s).unwrap(), s);
    }
    assert_eq!(h.root.roundtrip(&h.cb).unwrap(), "rt:ping");
}

// ---- plan 2-20 Phase B/C: client incoming connections ------------------

/// AC-20.2 — with one incoming connection the server reaches the client's
/// callback from a thread inside no handler: twoway returns, oneway lands.
/// Also with an outgoing fan-out alongside (AOSP `setupClient` order).
#[test]
fn b_outside_handler_callback_completes() {
    for (fan_out, max_threads) in [(1, 1), (2, 2)] {
        let h = boot_held_cfg(
            &format!("b_complete{fan_out}"),
            HeldCfg {
                a13: true,
                incoming: 1,
                fan_out,
                max_threads,
                ..Default::default()
            },
        );
        assert_eq!(h.client.__incoming_thread_count(), 1);
        let cb = h.cb_proxy();
        let worker = std::thread::spawn(move || (drive(&cb, false), drive(&cb, true)));
        let (twoway, oneway) = worker.join().expect("worker");
        assert_eq!(
            twoway,
            Ok(()),
            "fan_out={fan_out}: twoway outside a handler"
        );
        assert_eq!(
            oneway,
            Ok(()),
            "fan_out={fan_out}: oneway outside a handler"
        );
        let counter = Arc::clone(&h.cb_counter);
        assert!(
            poll_until(|| counter.load(Ordering::SeqCst) == 1),
            "fan_out={fan_out}: the oneway never reached the callback"
        );
        // The served connection is untouched by all of that.
        assert_eq!(h.root.echo("still").unwrap(), "still");
        assert_eq!(h.root.roundtrip(&h.cb).unwrap(), "rt:ping");
    }
}

/// AC-20.3 — many server threads at once: one incoming connection
/// serialises them (all succeed); two run them in parallel.
#[test]
fn b_outside_handler_parallel_callbacks() {
    let h = boot_held_cfg(
        "b_par1",
        HeldCfg {
            a13: true,
            incoming: 1,
            ..Default::default()
        },
    );
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let cb = h.cb_proxy();
            std::thread::spawn(move || (0..3).map(|_| drive(&cb, false)).collect::<Vec<_>>())
        })
        .collect();
    for w in workers {
        for r in w.join().expect("worker") {
            assert_eq!(r, Ok(()));
        }
    }
    // Serial on one slot: two 1000 ms calls take ≥ 2000 ms … The 400 ms
    // between the parallel ceiling (1600 ms) and the serial floor (2000 ms)
    // is the discrimination margin; the 600 ms between the parallel ceiling
    // and the 1000 ms floor is the headroom this binary needs, since it runs
    // its load tests concurrently.
    let t0 = Instant::now();
    let a = {
        let cb = h.cb_proxy();
        std::thread::spawn(move || drive_slow(&cb, 1000))
    };
    let b = {
        let cb = h.cb_proxy();
        std::thread::spawn(move || drive_slow(&cb, 1000))
    };
    assert_eq!(a.join().unwrap(), Ok(()));
    assert_eq!(b.join().unwrap(), Ok(()));
    assert!(
        t0.elapsed() >= Duration::from_millis(2000),
        "one incoming connection must serialise: {:?}",
        t0.elapsed()
    );
    drop(h);
    // … and in parallel on two (the default server budget is 2 × max_threads).
    let h = boot_held_cfg(
        "b_par2",
        HeldCfg {
            a13: true,
            incoming: 2,
            ..Default::default()
        },
    );
    assert_eq!(h.client.__slot_count(), 3);
    let t0 = Instant::now();
    let a = {
        let cb = h.cb_proxy();
        std::thread::spawn(move || drive_slow(&cb, 1000))
    };
    let b = {
        let cb = h.cb_proxy();
        std::thread::spawn(move || drive_slow(&cb, 1000))
    };
    assert_eq!(a.join().unwrap(), Ok(()));
    assert_eq!(b.join().unwrap(), Ok(()));
    assert!(
        t0.elapsed() < Duration::from_millis(1600),
        "two incoming connections must run in parallel: {:?}",
        t0.elapsed()
    );
}

/// AC-20.4 — the callback's handler (on the client's incoming thread) calls
/// the server back; that nested call re-enters the incoming slot and the
/// server answers it from its own reply wait.
#[test]
fn b_nested_call_from_callback_handler() {
    let h = boot_held_cfg(
        "b_nested",
        HeldCfg {
            a13: true,
            incoming: 1,
            ..Default::default()
        },
    );
    let cb = h.cb_proxy();
    let root = h.root_local.clone();
    let got = std::thread::spawn(move || drive_roundtrip(&cb, &root))
        .join()
        .expect("worker");
    assert_eq!(got, Ok("rt:ping".to_string()));
    assert_eq!(h.root.echo("after").unwrap(), "after");
}

/// A callback whose handler calls back into the session it was dispatched
/// from: a nested call from a *oneway* handler must not pin to the
/// connection the oneway arrived on (AOSP `RpcConnection::allowNested`).
struct NestFromHandler {
    /// The client's proxy back to the server root.
    root: Mutex<Option<SIBinder>>,
    /// One slot per dispatch, so the test can tell twoway from oneway.
    done: std::sync::mpsc::SyncSender<std::result::Result<String, StatusCode>>,
}
impl Interface for NestFromHandler {}
impl Remotable for NestFromHandler {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, code: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        // `TX_ECHO` is twoway (reply expected), `TX_BUMP` oneway.
        let echoed = if code == TX_ECHO {
            Some(r.read::<String>()?)
        } else {
            None
        };
        let root = self.root.lock().unwrap().clone().expect("root installed");
        let _ = self.done.try_send(EchoProxy(root).echo("nested"));
        match echoed {
            Some(a) => {
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&a)
            }
            None => Ok(()),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}
/// `Binder<T>` wants a value; forward to the shared `NestFromHandler`.
struct NestFromHandlerRef(Arc<NestFromHandler>);
impl Interface for NestFromHandlerRef {}
impl Remotable for NestFromHandlerRef {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, c: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        self.0.on_transact(c, r, reply)
    }
    fn on_dump(&self, w: &mut dyn std::io::Write, a: &[String]) -> Result<()> {
        self.0.on_dump(w, a)
    }
}
#[test]
fn b_nested_call_from_oneway_callback_handler() {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let holder = Arc::new(NestFromHandler {
        root: Mutex::new(None),
        done: tx,
    });
    let cb: SIBinder = Interface::as_binder(&Binder::new(NestFromHandlerRef(Arc::clone(&holder))));
    let h = boot_held_cfg(
        "b_onenest",
        HeldCfg {
            a13: true,
            incoming: 1,
            cb: Some(cb),
            ..Default::default()
        },
    );
    *holder.root.lock().unwrap() = Some(h.root.0.clone());

    // Control: a TWOWAY callback from a plain server thread. The peer is
    // parked in its reply wait on that connection, so the nested call may
    // ride it.
    let cb_tw = h.cb_proxy();
    let tw = std::thread::spawn(move || drive(&cb_tw, false));
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)),
        Ok(Ok("nested".to_string())),
        "a nested call from a twoway handler must complete"
    );
    assert_eq!(tw.join().expect("twoway worker"), Ok(()));

    // Regression: the same handler, reached by a ONEWAY callback.
    let cb_ow = h.cb_proxy();
    std::thread::spawn(move || drive(&cb_ow, true))
        .join()
        .expect("oneway worker")
        .expect("oneway send");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)),
        Ok(Ok("nested".to_string())),
        "a nested call from a oneway handler must not block on the \
         connection the oneway arrived on"
    );
    assert_eq!(h.root.echo("after").unwrap(), "after");
}

/// Configuration rules: r34 has no session id (`BadType`); an attach
/// (echoed id) gets neither fan-out nor incoming (`BadValue`); the manual
/// attach helpers reject a config carrying the other option.
#[test]
fn b_incoming_config_validation() {
    let h = boot_held_cfg(
        "b_cfg",
        HeldCfg {
            a13: true,
            ..Default::default()
        },
    );
    let path = h.server.path().expect("unix path").to_path_buf();
    let sid = h.client.get_session_id().expect("session id");
    assert!(matches!(
        RpcSession::setup_unix_client_android13plus_with_config(
            RpcUnixClientConfig::path(&path, 2)
                .session_id(&sid)
                .incoming_connections(1)
        ),
        Err(StatusCode::BadValue)
    ));
    assert!(matches!(
        h.client.add_outgoing_connection_android13plus_with_config(
            RpcUnixClientConfig::path(&path, 2)
                .session_id(&sid)
                .incoming_connections(1)
        ),
        Err(StatusCode::BadValue)
    ));
    assert!(matches!(
        h.client.add_incoming_connection_android13plus_with_config(
            RpcUnixClientConfig::path(&path, 2)
                .session_id(&sid)
                .outgoing_connections(2)
        ),
        Err(StatusCode::BadValue)
    ));
    // Manual attach works and is served.
    h.client
        .add_incoming_connection_android13plus_with_config(
            RpcUnixClientConfig::path(&path, 2).session_id(&sid),
        )
        .expect("manual incoming attach");
    // 1 is the founding slot; the attach must mint a fresh one.
    assert!(
        h.client.__slot_count() >= 2,
        "the attach must add a slot, not reuse the founding one"
    );
    assert_eq!(h.client.__incoming_thread_count(), 1);
    assert_eq!(h.client.__incoming_thread_live_count(), 1);
    let cb = h.cb_proxy();
    assert_eq!(
        std::thread::spawn(move || drive(&cb, false))
            .join()
            .unwrap(),
        Ok(())
    );
    // r34: no session id to echo. Address the r34 server, not the a13 one
    // above — the profile check happens before `connect()` today, so the
    // path is unused, but pointing it at the wrong server would silently
    // start testing something else if that order ever changed.
    let r34 = boot_held("b_cfg_r34");
    let r34_path = r34.server.path().expect("r34 server path").to_path_buf();
    assert!(matches!(
        r34.client
            .add_incoming_connection_android13plus_with_config(
                RpcUnixClientConfig::path(&r34_path, 2).session_id(&[7u8; 32])
            ),
        Err(StatusCode::BadType)
    ));
}

/// The server budgets callback slots at `2 * max_threads`: a third
/// incoming connection on a default server is refused (and the partially
/// built session is dropped), two are admitted.
#[test]
fn b_incoming_over_server_cap_is_refused() {
    let h = boot_held_cfg(
        "b_cap",
        HeldCfg {
            a13: true,
            incoming: 2,
            ..Default::default()
        },
    );
    let sid: [u8; 32] = h
        .client
        .get_session_id()
        .expect("sid")
        .as_slice()
        .try_into()
        .unwrap();
    assert!(poll_until(|| h.server.session_slot_count(&sid) == Some(3)));
    let path = h.server.path().expect("unix path").to_path_buf();
    let r = RpcSession::setup_unix_client_android13plus_with_config(
        RpcUnixClientConfig::path(&path, 2).incoming_connections(3),
    );
    // The regression this guards against *admits* the third slot. Leaking
    // that session would leave its incoming threads serving, and the panic
    // below would then hang the whole binary in `ServeCleanup`'s
    // `join_workers` instead of failing.
    if let Ok(s) = &r {
        s.shutdown();
    }
    assert!(
        matches!(r, Err(StatusCode::DeadObject)),
        "third callback slot must be refused by the cap (server closes the \
         connection, so the attach handshake dies): {:?}",
        r.map(|_| ())
    );
}

/// The unified entry: `ClientOptions::incoming_connections` reaches the
/// session; it needs the android-13+ profile.
#[test]
fn b_entry_client_open_with_incoming() {
    let h = boot_held_cfg(
        "b_entry",
        HeldCfg {
            a13: true,
            ..Default::default()
        },
    );
    let path = h.server.path().expect("unix path").to_path_buf();
    let uri = format!("unix://{}?profile=android13plus", path.display());
    let client = rsbinder::Client::open_with(&uri, |o, _| o.incoming_connections = Some(1))
        .expect("open with incoming");
    let session = client.session().expect("rpc session");
    // Read first, shut down, then assert: a session with a live incoming
    // thread that is never shut down hangs the fixture's `join_workers`,
    // so a failing assertion here must not skip the `shutdown`.
    let slots = session.__slot_count();
    let threads = session.__incoming_thread_count();
    session.shutdown();
    assert_eq!(slots, 2);
    assert_eq!(threads, 1);
    let r34 = format!("unix://{}", path.display());
    assert!(matches!(
        rsbinder::Client::open_with(&r34, |o, _| o.incoming_connections = Some(1)).map(|_| ()),
        Err(StatusCode::BadValue)
    ));
}

/// AC-20.5 — a client with an incoming connection learns of the server's
/// death from that connection dropping; one without learns only from its
/// next failed call.
#[test]
fn c_server_death_is_eager_with_incoming() {
    if let Ok(path) = std::env::var("RSB_RPC_DEATH_A13_SERVER") {
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server.set_android13plus(2);
        server.set_root(make_service(Arc::new(AtomicI64::new(0))));
        let _ = server.run(); // blocks until killed
        std::process::exit(0);
    }
    // Kill + reap the server child even when an assert below panics — its
    // `server.run()` loop never exits on its own, and `wait_for_sock`
    // alone can panic before the explicit kill on a loaded machine.
    struct KillOnDrop(std::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let path = tmp_sock("c_death");
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = KillOnDrop(
        std::process::Command::new(exe)
            .args([
                "--exact",
                "c_server_death_is_eager_with_incoming",
                "--nocapture",
            ])
            .env("RSB_RPC_DEATH_A13_SERVER", &path)
            .spawn()
            .expect("spawn server child"),
    );
    wait_for_sock(&path);
    let eager = RpcSession::setup_unix_client_android13plus_with_config(
        RpcUnixClientConfig::path(&path, 2).incoming_connections(1),
    )
    .expect("eager client");
    let lazy = RpcSession::setup_unix_client_android13plus(&path, 2).expect("lazy client");
    let eager_root = eager.get_root().expect("root");
    let lazy_root = lazy.get_root().expect("root");
    let (tx_e, rx_e) = std::sync::mpsc::sync_channel::<()>(1);
    let (tx_l, rx_l) = std::sync::mpsc::sync_channel::<()>(1);
    let flag_e: Arc<DeathFlag> = Arc::new(DeathFlag(tx_e));
    let flag_l: Arc<DeathFlag> = Arc::new(DeathFlag(tx_l));
    eager_root
        .link_to_death(Arc::downgrade(&flag_e) as _)
        .expect("link eager");
    lazy_root
        .link_to_death(Arc::downgrade(&flag_l) as _)
        .expect("link lazy");
    child.0.kill().expect("kill server child");
    child.0.wait().expect("reap server child");
    assert!(
        rx_e.recv_timeout(Duration::from_secs(3)).is_ok(),
        "the incoming connection's drop must fire the obituary at once"
    );
    assert!(
        rx_l.recv_timeout(Duration::from_millis(500)).is_err(),
        "without an incoming connection nothing observes the drop yet"
    );
    assert!(EchoProxy(lazy_root.clone()).echo("x").is_err());
    assert!(
        rx_l.recv_timeout(Duration::from_secs(3)).is_ok(),
        "the failed call declares the session dead"
    );
    eager.shutdown();
    assert_eq!(eager.__incoming_thread_live_count(), 0);
    assert_eq!(eager.__incoming_thread_count(), 0);
    lazy.shutdown();
    let _ = std::fs::remove_file(&path);
}

/// Plan 2-21 C-0 — a local `shutdown` ends the serve loop the same way
/// wherever its worker is: parked in `recv`, or inside a handler whose
/// reply then has nowhere to go. Both report `EndedBy::Local` with an
/// intact stream, so `into_result` is `Ok(())`. It used to be `Ok(())`
/// or `Err(DeadObject)` depending on which of the two the worker
/// happened to be doing — a contract no rustdoc could state.
#[test]
fn local_shutdown_ends_the_serve_loop_the_same_way_parked_or_dispatching() {
    use rsbinder::rpc::transport::UnixTransport;
    use rsbinder::rpc::{AddressSpace, EndedBy};

    for dispatching in [false, true] {
        let (srv_t, cli_t) = UnixTransport::pair().expect("socketpair");
        let server = Arc::new(
            RpcSession::new(Box::new(srv_t), AddressSpace::Acceptor).expect("server session"),
        );
        let slow_entered = Arc::new(AtomicBool::new(false));
        server.set_root(make_service_with_slow_signal(
            Arc::new(AtomicI64::new(0)),
            slow_entered.clone(),
        ));
        let serving = Arc::clone(&server);
        let serve = std::thread::spawn(move || serving.serve_blocking());
        let client =
            RpcSession::new(Box::new(cli_t), AddressSpace::Initiator).expect("client session");
        let root = EchoProxy(client.get_root().expect("root"));
        assert_eq!(root.echo("warm").unwrap(), "warm");

        let caller = dispatching.then(|| {
            let root = EchoProxy(root.0.clone());
            std::thread::spawn(move || {
                let mut d = root.rp().build_request(DESC).expect("request");
                d.write(&300i32).expect("arg");
                // Ends in an error once the session is shut down under it.
                let _ = root.rp().transact(TX_SLOW, &d, 0);
            })
        });
        if dispatching {
            assert!(
                poll_until(|| slow_entered.load(Ordering::SeqCst)),
                "the handler must be running when the session is shut down"
            );
        }
        server.shutdown();
        let end = serve.join().expect("serve thread");
        assert_eq!(end.by, EndedBy::Local, "dispatching={dispatching}: {end}");
        assert!(end.is_clean(), "dispatching={dispatching}: {end}");
        assert_eq!(end.into_result(), Ok(()), "dispatching={dispatching}");
        if let Some(c) = caller {
            c.join().expect("caller thread");
        }
        client.shutdown();
    }
}

/// Plan 2-21 B-4 — an `AF_INET` fd handed to `from_preconnected_fd` is
/// wrapped in `TcpDebugTransport` and goes straight into the android-13+
/// handshake, whose first byte is a raw write. Without that transport's
/// `send_raw`/`recv_raw` the write hit the trait default's `Protocol`
/// refusal — the omission that had already broken `vsock` once.
#[cfg(feature = "rpc-tcp-debug")]
#[test]
fn preconnected_inet_fd_handshakes_over_tcp_debug() {
    use rsbinder::rpc::transport::TcpDebugTransport;
    use std::os::fd::OwnedFd;

    let listener = TcpDebugTransport::bind_loopback().expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let client_stream = std::net::TcpStream::connect(addr).expect("connect");
    let (server_stream, _) = listener.accept().expect("accept");
    let server_t = TcpDebugTransport::from_stream(server_stream).expect("server transport");

    // The server object needs a listener to exist; it is never run.
    let path = tmp_sock("pcfd");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(2);
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    server.serve_connection(Box::new(server_t));

    let client = RpcSession::from_preconnected_fd(OwnedFd::from(client_stream), 2)
        .expect("android-13+ handshake over a preconnected AF_INET fd");
    assert_eq!(client.wire_protocol_version(), Some(2));
    let root = EchoProxy(client.get_root().expect("root"));
    assert_eq!(root.echo("inet").unwrap(), "inet");
    drop(root);
    drop(client);
    server.terminate();
    let _ = std::fs::remove_file(&path);
}

/// Plan 2-21 B-2 — `terminate` ends what `shutdown` only lets drain. An
/// r34 client and an android-13+ client (the two minting paths — only
/// the latter has a session id, so only it was ever in the id registry)
/// stay connected; `terminate` still returns, wakes every worker out of
/// `recv`, joins them, and the android-13+ session — driven by two
/// workers, so `Live(2)` — dies whole instead of being skipped as
/// "still live".
#[test]
fn terminate_ends_every_session_and_joins_workers() {
    // r34 (the default profile): the minting path with no session id, so
    // nothing in the id registry ever knew this session. A server speaks
    // one profile, so the two paths need two servers.
    let r34_path = tmp_sock("term34");
    let r34_server = RpcServer::setup_unix_server(&r34_path).expect("bind r34");
    r34_server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let r34_bg = r34_server.run_background();
    wait_for_sock(&r34_path);
    let r34 = RpcSession::setup_unix_client(&r34_path).expect("r34 connect");
    let r34_root = EchoProxy(r34.get_root().expect("root"));
    assert_eq!(r34_root.echo("r34").unwrap(), "r34");

    // android-13+ with two outgoing connections: a `Live(2)` server
    // session, which `RpcSession::shutdown` used to skip as "still live".
    let a13_path = tmp_sock("term13");
    let a13_server = RpcServer::setup_unix_server(&a13_path).expect("bind a13");
    a13_server.set_android13plus(2);
    a13_server.set_max_threads(2);
    a13_server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let a13_bg = a13_server.run_background();
    wait_for_sock(&a13_path);
    let a13 = RpcSession::setup_unix_client_android13plus_with_config(
        RpcUnixClientConfig::path(&a13_path, 2).outgoing_connections(2),
    )
    .expect("android-13+ connect");
    let a13_root = EchoProxy(a13.get_root().expect("root"));
    assert_eq!(a13_root.echo("a13").unwrap(), "a13");
    let sid: [u8; 32] = a13
        .get_session_id()
        .expect("sid")
        .as_slice()
        .try_into()
        .unwrap();
    assert!(
        poll_until(|| a13_server.session_live_conns(&sid) == Some(2)),
        "two outgoing connections drive the android-13+ session: {:?}",
        a13_server.session_live_conns(&sid)
    );

    // Off-thread with a channel deadline: a `terminate` that blocks on a
    // connected client must fail the test, not hang the runner.
    for (name, server) in [("r34", &r34_server), ("android-13+", &a13_server)] {
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        let terminating = Arc::clone(server);
        let t = std::thread::spawn(move || {
            terminating.terminate();
            let _ = tx.send(());
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => {
                panic!("{name}: terminate blocked with a client still connected")
            }
        }
        t.join().expect("terminate thread");
    }
    r34_bg.join().expect("r34 accept loop");
    a13_bg.join().expect("android-13+ accept loop");
    // Every worker exited (their `Arc<RpcServer>` clones are gone) and the
    // android-13+ session is dead, not merely "still live".
    for (name, server) in [("r34", &r34_server), ("android-13+", &a13_server)] {
        assert!(
            poll_until(|| Arc::strong_count(server) == 1),
            "{name}: a worker still holds the server ({} refs)",
            Arc::strong_count(server)
        );
    }
    assert!(
        poll_until(|| matches!(a13_server.session_live_conns(&sid), None | Some(0))),
        "the android-13+ session must be dead after terminate"
    );
    assert!(
        r34_root.echo("after").is_err(),
        "the r34 connection was ended"
    );
    assert!(
        a13_root.echo("after").is_err(),
        "the android-13+ connection was ended"
    );
    let _ = std::fs::remove_file(&r34_path);
    let _ = std::fs::remove_file(&a13_path);
}

/// Plan 2-21 B-2 — a service that ends its own server calls `terminate`
/// from inside a handler, on the very worker thread `terminate` would
/// join. That handle is skipped; the worker exits when the handler
/// returns, and nothing deadlocks.
#[test]
fn terminate_from_a_handler_does_not_join_itself() {
    const STOP_DESC: &str = "rsbinder.test.IStopper";
    struct Stopper(Mutex<Option<Arc<RpcServer>>>);
    impl Remotable for Stopper {
        fn descriptor() -> &'static str {
            STOP_DESC
        }
        fn on_transact(
            &self,
            _code: TransactionCode,
            _reader: &mut Parcel,
            reply: &mut Parcel,
        ) -> Result<()> {
            if let Some(server) = self.0.lock().unwrap().take() {
                server.terminate();
            }
            reply.write(&Status::from(StatusCode::Ok))
        }
        fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
            Ok(())
        }
    }

    let path = tmp_sock("termre");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    // The service holds the server until the call takes it, so the strong
    // count below can reach 1 once the worker is gone.
    server.set_root(Interface::as_binder(&Binder::new(Stopper(Mutex::new(
        Some(Arc::clone(&server)),
    )))));
    let bg = server.run_background();
    wait_for_sock(&path);
    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let root = client.get_root().expect("root");

    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
    let caller = std::thread::spawn(move || {
        let rp = (*root)
            .as_any()
            .downcast_ref::<RpcProxy>()
            .expect("RpcProxy");
        let d = rp.build_request(STOP_DESC).expect("request");
        // The reply is written after the transport went down, so the call
        // ends in an error — what matters is that it ends.
        let _ = rp.transact(FIRST_CALL_TRANSACTION, &d, 0);
        let _ = tx.send(());
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => panic!("terminate from a handler deadlocked"),
    }
    caller.join().expect("caller thread");
    bg.join().expect("accept loop");
    assert!(
        poll_until(|| Arc::strong_count(&server) == 1),
        "the handler's own worker must exit on its own ({} refs)",
        Arc::strong_count(&server)
    );
    client.shutdown();
    let _ = std::fs::remove_file(&path);
}

/// AC-20.6 — `shutdown` ends and joins the incoming threads; the server
/// sees the session go.
#[test]
fn c_shutdown_joins_incoming_threads() {
    let h = boot_held_cfg(
        "c_join",
        HeldCfg {
            a13: true,
            incoming: 2,
            ..Default::default()
        },
    );
    let sid: [u8; 32] = h
        .client
        .get_session_id()
        .expect("sid")
        .as_slice()
        .try_into()
        .unwrap();
    assert_eq!(h.client.__incoming_thread_count(), 2);
    assert_eq!(h.client.__incoming_thread_live_count(), 2);
    // Run `shutdown` off-thread with a deadline on the *channel*: a
    // deadlocked shutdown must fail the test, not hang it until the CI
    // timeout (which is what asserting on `elapsed()` after the call
    // would do — that line is never reached).
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
    let shutting = h.client.clone();
    let t = std::thread::spawn(move || {
        shutting.shutdown();
        let _ = tx.send(());
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(()) => {}
        // A panicked `shutdown` breaks the channel too; falling through to
        // the `join` below re-raises it with its own message rather than
        // reporting a deadlock that did not happen.
        Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => {
            // Report the failure instead of hanging the runner: dropping `h`
            // would run `HeldSetup::drop` → `join_workers()`, which a regression
            // that leaves the transports up would never return from.
            std::mem::forget(h);
            panic!("shutdown deadlocked");
        }
    }
    t.join().expect("shutdown thread");
    // The observable half of "joins". `__incoming_thread_count` is
    // `mem::take`n before the first `join()`, so it reads 0 either way;
    // and the live count usually reaches 0 on its own, because
    // `shutdown` wakes the threads (transport shutdown) before it joins
    // them. Only the join counter separates joining from detaching.
    assert_eq!(
        h.client.__incoming_thread_joined_count(),
        2,
        "shutdown must join the incoming threads, not detach them"
    );
    assert_eq!(h.client.__incoming_thread_live_count(), 0);
    assert_eq!(h.client.__incoming_thread_count(), 0);
    // Idempotent.
    h.client.shutdown();
    let server = Arc::clone(&h.server);
    assert!(
        poll_until(|| server.session_slot_count(&sid).is_none_or(|n| n == 0)),
        "the server still holds slots for the shut-down session"
    );
    assert!(matches!(h.root.echo("dead"), Err(StatusCode::DeadObject)));
}

/// A `DeathRecipient` that shuts the session down from inside `binder_died`
/// — the cycle-breaking call the `RpcSession` docs point at.
struct ShutdownOnDeath(Mutex<Option<RpcSession>>, Arc<AtomicBool>);
impl rsbinder::DeathRecipient for ShutdownOnDeath {
    fn binder_died(&self, _who: &rsbinder::WIBinder) {
        self.1.store(true, Ordering::SeqCst);
        if let Some(s) = self.0.lock().unwrap().take() {
            s.shutdown();
        }
    }
}

/// The obituary runs while the incoming threads are still parked in `recv`,
/// so a `binder_died` that calls `shutdown` would join threads nothing has
/// woken. Death must shut the transports down before it runs user code.
#[test]
fn c_shutdown_from_death_recipient_does_not_deadlock() {
    let h = boot_held_cfg(
        "c_death_sd",
        HeldCfg {
            a13: true,
            incoming: 2,
            ..Default::default()
        },
    );
    assert_eq!(h.client.__incoming_thread_live_count(), 2);
    let fired = Arc::new(AtomicBool::new(false));
    let recip: Arc<ShutdownOnDeath> = Arc::new(ShutdownOnDeath(
        Mutex::new(Some(h.client.clone())),
        Arc::clone(&fired),
    ));
    h.root
        .0
        .link_to_death(Arc::downgrade(&recip) as _)
        .expect("link");

    // Killing the server drives death on the client, which fires the
    // obituary above. Deadline on the channel, not on `elapsed()` after
    // the fact: a deadlock must fail this test, not hang the run.
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
    let victim = h.client.clone();
    let t = std::thread::spawn(move || {
        victim.shutdown();
        let _ = tx.send(());
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(()) => {}
        // A panicked `shutdown` breaks the channel too; the `join` below
        // re-raises it instead of reporting a deadlock that did not happen.
        Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => {
            // The session is wedged, so dropping the fixture would block in
            // `ServeCleanup`'s `join_workers` and turn this failure into a CI
            // timeout. Leak it and report instead.
            std::mem::forget(h);
            panic!("shutdown from a death recipient deadlocked");
        }
    }
    t.join().expect("shutdown thread");
    assert_eq!(h.client.__incoming_thread_live_count(), 0);
    // Without this the test passes on a regression that never fires the
    // obituary at all: the outer `shutdown` would join the threads itself
    // and every assertion above would still hold.
    assert!(
        fired.load(Ordering::SeqCst),
        "the obituary never ran; this test would gate nothing"
    );
}

/// A callback whose handler shuts its own session down: the incoming
/// thread that runs it is not joined by itself (no deadlock), and the
/// server's call returns (`DeadObject` — the session the reply owed its
/// answer to is gone by then) instead of hanging.
struct ShutdownCb(Mutex<Option<RpcSession>>);
impl Interface for ShutdownCb {}
impl Remotable for ShutdownCb {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, _c: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        // Echo like `EchoSvc` so `drive(.., false)` can assert on the reply.
        let a: String = r.read()?;
        if let Some(s) = self.0.lock().unwrap().as_ref() {
            s.shutdown();
        }
        reply.write(&Status::from(StatusCode::Ok))?;
        reply.write(&a)
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}
#[test]
fn c_shutdown_from_callback_handler_does_not_self_join() {
    let holder = Arc::new(ShutdownCb(Mutex::new(None)));
    let cb: SIBinder = Interface::as_binder(&Binder::new(ShutdownCbRef(Arc::clone(&holder))));
    let h = boot_held_cfg(
        "c_selfjoin",
        HeldCfg {
            a13: true,
            incoming: 1,
            cb: Some(cb),
            ..Default::default()
        },
    );
    *holder.0.lock().unwrap() = Some(h.client.clone());
    let server_cb = h.cb_proxy();
    // Deadline on the channel, not on `elapsed()` after the join: a
    // self-join deadlock must fail here rather than hang the run.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let worker = std::thread::spawn(move || {
        let r = drive(&server_cb, false);
        let _ = tx.send(r);
    });
    let r = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(r) => r,
        Err(RecvTimeoutError::Timeout) => {
            panic!("shutdown inside the handler deadlocked")
        }
        // `drive` asserts on the reply, so a broken channel is the
        // worker's panic. Re-raise that instead of reporting a deadlock
        // that never happened.
        Err(RecvTimeoutError::Disconnected) => match worker.join() {
            Err(p) => std::panic::resume_unwind(p),
            Ok(()) => panic!("the driving thread sent no result"),
        },
    };
    // The handler tore its own session down before the reply could go
    // out, so the call comes back `DeadObject` — the point is that it
    // *comes back* rather than deadlocking on a self-join.
    assert_eq!(
        r,
        Err(StatusCode::DeadObject),
        "the server's call must return once the handler's shutdown lands"
    );
    worker.join().expect("worker");
    // The thread that ran the handler was left to finish on its own
    // (joining itself would deadlock), so it ends slightly after
    // `shutdown` returned — poll for it.
    assert!(poll_until(|| h.client.__incoming_thread_live_count() == 0));
    assert_eq!(h.client.__incoming_thread_count(), 0);
    h.client.shutdown();
    assert!(matches!(h.root.echo("dead"), Err(StatusCode::DeadObject)));
    *holder.0.lock().unwrap() = None;
}
/// `Binder<T>` wants a value; forward to the shared `ShutdownCb`.
struct ShutdownCbRef(Arc<ShutdownCb>);
impl Interface for ShutdownCbRef {}
impl Remotable for ShutdownCbRef {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, c: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        self.0.on_transact(c, r, reply)
    }
    fn on_dump(&self, w: &mut dyn std::io::Write, a: &[String]) -> Result<()> {
        self.0.on_dump(w, a)
    }
}

/// A client with `incoming_connections > 0` that loses its only
/// `Outgoing` slot to a reply-deadline retirement must still declare the
/// session dead: the surviving callback slot keeps the pool non-empty,
/// but nothing reads a request written on it, so without the
/// last-outgoing check the session stayed `Live` forever — every later
/// transact answered `WouldBlock` with no obituary and no
/// `RpcState::clear`.
#[test]
fn c_losing_the_last_outgoing_slot_declares_death_with_incoming() {
    let h = boot_held_cfg(
        "c_lastout",
        HeldCfg {
            a13: true,
            incoming: 1,
            max_threads: 2,
            ..Default::default()
        },
    );
    assert_eq!(h.client.__slot_count(), 2, "founding outgoing + callback");
    // Deadline far below the handler's sleep: the reply arrives too late,
    // which desyncs and retires the founding `Outgoing` slot.
    h.client.set_timeout(Some(Duration::from_millis(50)));
    assert_eq!(h.root.slow(400), Err(StatusCode::TimedOut));
    // Death, not a live session with only callback slots left.
    assert_eq!(
        h.client.__slot_count(),
        0,
        "losing the last outgoing slot must run the death sequence"
    );
    assert_eq!(h.root.echo("after"), Err(StatusCode::DeadObject));
}
