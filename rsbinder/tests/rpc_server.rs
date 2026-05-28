// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Subplan 2-3: multi-session `RpcServer`, real-process e2e, threads,
//! `getRemoteMaxThreads` negotiation, oneway FIFO, nested callbacks,
//! timeout, lifecycle, and the P6 no-global gate.
//!
//! Separate test binary (master §6). P6: each test builds its own
//! server + sessions ⇒ parallel-safe, no `--test-threads=1`.

#![cfg(feature = "rpc")]

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rsbinder::rpc::{RpcProxy, RpcServer, RpcSession};
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

trait IEcho2: Interface {
    fn echo(&self, s: &str) -> Result<String>;
    fn bump(&self) -> Result<()>; // oneway
    fn count(&self) -> Result<i64>;
    fn slow(&self, ms: i32) -> Result<()>;
    /// Server calls `cb.echo("ping")` and returns its result
    /// (exercises a server→client nested callback).
    fn roundtrip(&self, cb: &SIBinder) -> Result<String>;
}

struct EchoSvc {
    counter: Arc<AtomicI64>,
    /// Optional self-handle so `roundtrip`'s callback can call back in
    /// (depth-3): the client callback re-invokes the *server*.
    deeper: bool,
    /// Set on **entry** to `slow()` so a test can wait deterministically
    /// for a parked thread to have actually claimed a server worker
    /// (e.g. AC-12.2's slot-pin scope). Tests that don't care pass a
    /// fresh `AtomicBool` and ignore it. Set is one-way (no reset) —
    /// "at least one slow call entered" is the only signal needed.
    slow_entered: Arc<AtomicBool>,
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
/// heuristic that races scheduler jitter (the AC-12.2 mutant-gate
/// reinforcement called out in review).
fn make_service_with_slow_signal(
    counter: Arc<AtomicI64>,
    slow_entered: Arc<AtomicBool>,
) -> SIBinder {
    Interface::as_binder(&Binder::new(BnEcho2(Box::new(EchoSvc {
        counter,
        deeper: false,
        slow_entered,
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
/// race-tightening check after the last sleep. Replaces the three
/// duplicated local `poll` closures the review flagged.
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
/// remainder of the suite (each subsequent timing-sensitive AC-12.1*
/// test then runs against a contaminated scheduler). Construct **once**
/// per test, right after `server.run_background()`, then let `Drop` do
/// the rest.
struct ServeCleanup {
    server: Arc<RpcServer>,
    bg: Option<std::thread::JoinHandle<()>>,
    path: std::path::PathBuf,
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
            path,
        }
    }
}
impl Drop for ServeCleanup {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(h) = self.bg.take() {
            // A panic in the background accept loop is a real SUT
            // bug (`RpcServer::run()` is supposed to return cleanly).
            // The previous `let _ = h.join()` silently discarded that
            // signal, so a future regression introducing an
            // `expect("...poisoned")` panic in the accept path would
            // pass every test green. Surface the payload to stderr —
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
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---- AC-3.1 real OS process e2e + AC-3.4 negotiation ---------------

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
        // AC-3.4: explicit negotiation, local=8, server advertises 2.
        assert_eq!(client.negotiate(8).expect("negotiate"), 2);
        assert_eq!(client.negotiated_max_threads(), 2);

        let root = EchoProxy(client.get_root().expect("get_root"));
        // AC-3.1: real cross-process AIDL call.
        assert_eq!(
            root.echo("over a real process").unwrap(),
            "over a real process"
        );
        assert_eq!(root.echo("").unwrap(), "");
        for i in 0..50 {
            assert_eq!(root.echo(&format!("n{i}")).unwrap(), format!("n{i}"));
        }
    }

    // AC-3.7: killing the client leaves the server able to exit cleanly.
    child.kill().expect("kill server child");
    child.wait().expect("reap server child");
    let _ = std::fs::remove_file(&path);
}

// ---- AC-3.2 concurrency: many threads, ONE shared session ----------

#[test]
fn concurrent_calls_single_shared_session() {
    let path = tmp_sock("shared");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // AC-3.2 (as written: "8 client threads on the SAME session").
    // ONE client session, its root proxy shared (Arc) across 8 threads
    // — exactly how a generated `Bp*` stub is used concurrently
    // (`SIBinder` is `Send`/`Sync`). Before the per-connection lock
    // this interleaved the framed stream / cross-delivered replies
    // (Major-2). Calls are internally serialized on the one
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

// ---- AC-3.3 multi-session isolation --------------------------------

#[test]
fn concurrent_clients_isolated_sessions() {
    let path = tmp_sock("iso");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // 8 client threads, each its own connection (independent session +
    // RpcState — P6). Each does 200 calls and must see only its own
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

// ---- 2-10: opt-in server-side connection admission bound -----------

/// `set_max_connections(N)` caps *concurrent* connection workers via
/// reactor-free accept backpressure (excess clients wait in the kernel
/// listen backlog, none dropped; a freed slot resumes accept). This is
/// the reactor-free, Android-faithful resource bound that the 2-10
/// decision record carves out as the genuine middle ground (the
/// no-reactor decision does NOT foreclose it).
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

// ---- AC-3.5 oneway FIFO + non-blocking send ------------------------

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

// ---- AC-3.6 nested server→client callback --------------------------

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

// ---- AC-3.8 timeout (hung server) ----------------------------------

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

// ---- 2-5b / G4(a): opt-in android-13+ versioned-wire profile -------

/// G4(a): the proven android-13+ connection handshake + AOSP-faithful
/// framing + `Android13PlusCodec` (G4 Layer-1, hermetic) now driving a
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
        (2, 2, 2),          // v2 — android-16 (subplan 2-8)
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

/// **2-15 AC-15.3 / 2-15.0 PoC** — the decoupled `TlsTransport`
/// (`Mutex<Connection>` + lock-free stream, subplan 2-15 §2.0) driving
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

        // Exercises the 2-15.5 convenience constructor (TCP-connect +
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

/// **AC-12.0b** (subplan 2-12 Phase A0b — `transport` split out of
/// `RpcSessionInner` into a shared `SharedSession` + server id-demux +
/// F4 death-trigger; see `plans/2-12-multi-connection-per-session.md`
/// §0 F9 / §2 / §6). A0b is the ≡Phase-A-risk structural core the
/// recursive review carved out of the (now byte-identical default)
/// A0a plumbing.
///
/// Proves the **id-demux attach** is real (not dead plumbing):
///  - client #1 — **empty** id ⇒ new session; server registers a
///    `Weak` of its `SharedSession`; default flow unchanged
///    (`attached/rejected == 0`, `session_registered >= 1`, full
///    round-trip);
///  - `get_session_id()` round-trips the server-minted 32-byte id
///    (the previously-missing client half of AOSP `setupClient`);
///  - client #2 — **echoes that id** ⇒ server resolves the live
///    session and **attaches** this connection to it: `c2` speaks the
///    *same* `SharedSession` (`c2.get_session_id() == sid1` — the
///    shared `rpc_session_id` lives in `SharedSession`, so this holds
///    **iff** state is shared, not a fresh per-connection session),
///    `attached_count == 1`, and `c2` is fully functional over the
///    attached connection;
///  - client #3 — an **unknown** 32-byte id ⇒ no live session ⇒
///    rejected (`rejected_unknown_id == 1`);
///  - **F4 (partial vs. full teardown)**: dropping the *attached* #2
///    must NOT tear the session down — #1 stays fully functional and
///    still reports the same shared id (a spurious obituary / early
///    teardown on a partial connection loss would break this). The
///    complementary "fires exactly once on *full* teardown,
///    byte-identical to pre-A0b" side is the unchanged
///    `rpc_death_recipient_fires_on_session_drop` (single-connection
///    `live_conns 1→0`) in this same suite.
///
/// **Mutant gate (== pre-A0b code)**: make the found branch build a
/// *fresh* session instead of attaching (`from_android13plus(.., None)`
/// in `serve_connection`'s attach arm). Then #2 gets its own
/// `SharedSession` ⇒ `c2.get_session_id() != sid1` ⇒ the shared-id
/// assertion fails. That is what makes the demux load-bearing rather
/// than the dead plumbing F1/F9 warn about.
#[test]
fn a0b_multi_connection_shared_session() {
    let path = tmp_sock("a0b");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1); // opt in to the versioned wire
                                 // Phase B.1 (AC-12.4): `set_max_threads` is now the advertised
                                 // *and* enforced per-session incoming-slot cap. This test exercises
                                 // founding + one attached connection ⇒ explicit opt-in at 2 slots
                                 // (default 1 ⇒ founding-only). AOSP-faithful: AC-12.0b is fundamentally
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
    let c3 = RpcSession::setup_unix_client_android13plus_with_id(&path, 1, &bogus)
        .expect("handshake completes (reject is post-handshake — A0b residual)");
    c3.set_timeout(Some(Duration::from_secs(3)));
    // Strengthen the unknown-id reject assertion (review m8 + round-2
    // M3): the original `is_err()` would also pass for an unrelated
    // error (e.g. handshake itself failed). Lock the contract to the
    // actual post-handshake-reject status set observed on UDS:
    //
    //  - **DeadObject** — the canonical path: server `drop(transport)`
    //    closes the socket; the client's next send/recv surfaces
    //    `io::ErrorKind::{BrokenPipe,ConnectionReset,UnexpectedEof,…}`
    //    which `RpcError::from(io)` maps to `PeerClosed` and then
    //    `StatusCode::DeadObject` (see `rsbinder/src/rpc/mod.rs`
    //    `From<io::Error> for RpcError` and the `PeerClosed →
    //    DeadObject` arm).
    //  - **TimedOut** — `set_timeout(3s)` fires before the close
    //    propagates.
    //  - **Unknown** — host-OS dependent: macOS in particular can
    //    surface a socket-close as an `io::Error` whose
    //    `raw_os_error() == None` (e.g. `ErrorKind::Other`), which
    //    `StatusCode::from(io::Error)` maps to `Unknown` rather than
    //    `PeerClosed`. This was observed in practice on this host
    //    during initial development. The ideal fix is to root-cause
    //    the host-specific path so it normalizes to `DeadObject` (the
    //    AOSP-faithful signature); kept as an accepted reject status
    //    for now — **FOLLOWUP**: trace and tighten.
    //
    // Anything outside this set means a different bug (and `Ok` is
    // the true mutant: server honored the unknown id).
    let err = c3.get_root().expect_err("unknown id rejected");
    assert!(
        matches!(
            err,
            StatusCode::DeadObject | StatusCode::TimedOut | StatusCode::Unknown
        ),
        "unknown id reject should surface as DeadObject (PeerClosed path) / \
         TimedOut (deadline wins) / Unknown (host-OS-dependent close), got {err:?}"
    );
    assert!(
        poll_until(|| server.rejected_unknown_id_count() == 1),
        "unknown id ⇒ rejected as UNKNOWN"
    );
    assert_eq!(server.attached_count(), 1, "attach count stable");

    // --- F4: drop **only** the ATTACHED connection #2 (a partial
    //     loss; `root2` is intentionally left alive — see below). The
    //     session must survive: the founding connection #1 keeps
    //     working and still reports the same shared id. A spurious
    //     obituary / early teardown on a partial connection loss
    //     (live_conns mis-gated) would make these DeadObject.
    //
    //     This test keeps its A0b-era shape: the liveness probe is
    //     `get_session_id` (a zero-address special transact) and it
    //     does not drop a sibling proxy here — that *was* F7 (multi-
    //     proxy refcount over a shared `RpcState`), at the time a
    //     Phase-A residual. **F7 is now fixed** (AOSP `timesSent` /
    //     `flushExcessBinderRefs`); the previously-avoided sibling-
    //     proxy-drop path is covered by `f7_shared_node_survives_
    //     sibling_proxy_drop` + `f7_excess_receipt_no_leak_single_
    //     client`. This test stays focused on A0b's own contract
    //     (attach works — shown above — + F4 no premature teardown on
    //     partial loss, shown here) so its mutant gate stays clean.
    drop(c2);
    // **Deterministic** wait for the server's attached worker to
    // exit (`serve_blocking_on` → `live_conns.fetch_sub`), replacing
    // the prior `sleep(50ms)` heuristic that raced scheduler jitter
    // under CI load. `session_live_conns` reads the F4 ledger
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
    drop(root2);
    drop(c1);
    drop(c3);
    // _cu's Drop handles shutdown/bg.join/join_workers/remove_file —
    // a panic above no longer leaks worker threads + socket file.
}

/// **AC-12.F8** (subplan 2-12 Phase A4 — server-side unification of
/// `RpcSessionInner` into a single inner per session; see
/// `plans/2-12-multi-connection-per-session.md` §2 Phase A4 / §6).
///
/// The post-A0b residual was that the *server* still built one
/// `RpcSessionInner` per accepted connection (sharing only the
/// `SharedSession`), so `state.remote_proxies`-cached `RpcProxy.weak:
/// Weak<RpcSessionInner>` pointed to the *first* worker's inner. A
/// later worker unmarshaling the same client binder hit the cache and
/// inherited that other inner — its nested `proxy.transact`
/// `find_conn`ed inside the other worker's slot pool, not its own:
/// cross-slot aliasing (F8). Phase A4 collapses N inners into 1 (slots
/// in one pool); every cached `RpcProxy.weak` now points to the only
/// inner, and any server worker's nested `proxy.transact` `find_conn`s
/// **inside its own pool**.
///
/// Witness via the *founding inner*'s slot count: after Phase A4 an
/// id-echoing attached connection adds a *slot* to that inner, so
/// `session_slot_count(sid) == Some(2)`. The F8 mutant (== pre-A4
/// code: server attach arm builds `from_android13plus(.., Some(shared))`
/// = a fresh inner sharing only `SharedSession`) leaves the founding
/// inner with its single founding slot ⇒ `Some(1)` — the assertion
/// fails. This is what makes Phase A4 load-bearing rather than a
/// no-op refactor.
///
/// AC-12.0b still asserts the *attach* itself (shared `SharedSession`
/// id round-trip + `attached_count == 1` + F4 partial-loss survival);
/// this test layers Phase A4's structural shape on top of that
/// without re-asserting the A0b contract.

#[test]
fn ac_12_f8_attach_unifies_to_single_inner() {
    let path = tmp_sock("ac12f8");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Phase B.1: opt into 2 incoming slots (default 1 ⇒ founding-only).
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

    // Attached connection (#2): echo #1's id ⇒ Phase A4 attach arm
    // adds a *slot* to the founding inner (rather than building a
    // fresh inner sharing only SharedSession — the F8 mutant).
    let c2 = RpcSession::setup_unix_client_android13plus_with_id(&path, 1, &sid)
        .expect("a13+ #2 (attach)");
    // Bound without the conventional `_` prefix — `root2` is moved into
    // an explicit `drop(...)` below to trigger the F4 partial-loss
    // reap; an underscore-prefix would have read as "intentionally
    // unused" and hidden that load-bearing drop.
    let root2 = EchoProxy(c2.get_root().expect("get_root #2"));
    assert!(
        poll_until(|| server.attached_count() == 1),
        "echo-id connection took the id-demux ATTACH path (A0b)"
    );
    // Phase A4 contract: the attached connection is a *slot on the
    // founding inner*, so the inner's slot pool now has 2 slots.
    // F8 mutant — each connection has its own inner with one slot —
    // would leave the founding inner at slot_count == 1.
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(2)),
        "Phase A4: attached connection adds a slot to the founding inner \
         (mutant: fresh-inner-on-attach leaves slot_count == 1)"
    );

    // Partial-loss reaping (re-uses the existing F4 ledger): drop the
    // attached and verify the founding inner's slot pool shrinks back
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

/// **AC-12.4 (Phase B.1 — `setMaxIncomingThreads` cap + negotiate
/// reflection; see `plans/2-12-multi-connection-per-session.md` §2
/// Phase B / §3 AC-12.4).** Phase B.1 unifies `set_max_threads(N)`'s
/// two meanings — advertise + enforce: the server attach arm refuses
/// an id-echoing connection when adding it would push `slot_count() >
/// max_threads_value()`. Default 1 ⇒ founding-only; multi-conn
/// callers opt in via explicit `set_max_threads(N >= 2)`.
///
/// Mutant gate (== pre-B.1 / advertise-only): cap check absent ⇒
/// 3rd attach silently succeeds and `session_slot_count(sid) ==
/// Some(3) > set_max_threads(2)` ⇒ the cap assertion fails (and the
/// `rejected_unknown_id_count` witness stays at 0).
///
/// Sub-AC (b): `GetMaxThreads` returns the same `max_threads_value()`
/// (advertise == enforce), so a client's `negotiate(local_max)`
/// records `min(local_max, server_cap)` — verified at the wire by
/// querying `negotiated_max_threads()` after a single
/// `GetMaxThreads` round-trip.
///
/// Sub-AC (c — `shutdown` gate): documented at the *code* level only
/// — the attach arm shows `if server.shutdown.load() { reject }`. A
/// deterministic e2e trigger needs an accept-pass / handshake-stall /
/// late-shutdown race that current test scaffolding does not bound;
/// kept as a §7.3 FOLLOWUP (test scaffolding, not a B.1 deferral —
/// the *behavior* is already in code and falls inside the
/// rejected_unknown_id observability).

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
    // 3 > max_threads(2) ⇒ refused at the attach arm (Phase B.1 cap).
    let c3 = RpcSession::setup_unix_client_android13plus_with_id(&path, 1, &sid)
        .expect("handshake completes (reject is post-handshake — A0b/B.1 residual)");
    c3.set_timeout(Some(Duration::from_secs(3)));
    let err = c3
        .get_root()
        .expect_err("3rd attach must be rejected by the per-session cap");
    assert!(
        matches!(
            err,
            StatusCode::DeadObject | StatusCode::TimedOut | StatusCode::Unknown
        ),
        "cap-reject surfaces as the same reject set as unknown-id reject \
         (post-handshake socket close), got {err:?}"
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

    // Sub-AC (b): negotiate reflects the server's advertised + enforced
    // value. Client opts into local_max=4; server advertises 2 ⇒
    // negotiated = min(4, 2) = 2. (Wire-level: a single GetMaxThreads
    // round-trip, AC-3.4.) The advertise == enforce equation is exactly
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

/// **Phase A F7** (the AC-12.0b residual, now fixed). With a shared
/// `RpcState` (A0b id-demux), two **independent** client sessions each
/// hold their own proxy to the *same* server root. Pre-F7 the server
/// pinned the node's strong count at 1 by object-identity, so the
/// first connection's proxy drop `DEC_STRONG`'d it to 0 and freed the
/// node ⇒ the sibling connection's proxy `DeadObject` (exactly what
/// AC-12.0b had to *avoid*). With AOSP `timesSent` accounting the
/// server counts each send (strong = 2), so the first DEC only brings
/// it to 1 and the sibling survives; the node is freed only when the
/// *second* proxy drops too (no leak — proven deterministically at the
/// state level by `rpc::state::tests::f7_timessent_balance_no_leak`).
///
/// **Mutant gate (== pre-F7 code)**: revert `on_binder_leaving`'s
/// `timesSent` bump (strong stays 1). Then dropping `root1` frees the
/// shared node and `root2.echo()` is `DeadObject` ⇒ this fails.
#[test]
fn f7_shared_node_survives_sibling_proxy_drop() {
    let path = tmp_sock("f7");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Phase B.1: opt into 2 incoming slots (founding + attached).
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

    // F7: the shared node must survive the sibling's DEC (strong
    // 2→1). Pre-F7 (mutant) it was freed (1→0) ⇒ DeadObject here.
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

/// **Phase A F7 — client `flushExcessBinderRefs`** (the *other* mutant
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

/// **AC-12.1 (Phase A — connection pool)**: with N outgoing slots in
/// one `RpcSession`, concurrent `client_transact`s on different
/// threads pick *different* slots (AOSP `findConnection` available-
/// slot selection), so two server-side blocking handlers run **in
/// parallel** — not serialized through one connection.
///
/// Wire-up: founding connection (slot 1) + one echoed-id outgoing
/// (slot 2) via [`RpcSession::add_outgoing_connection_android13plus`].
/// The slots end up id-demuxed to the *same* `SharedSession` (A0b),
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
    // Phase B.1: AC-12.1 wires founding + one attached outgoing-echo
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
    //
    // The macOS CI runner occasionally schedules the two threads onto
    // the same core under load and lands measurements in the 285-310 ms
    // band even though the path is structurally parallel (one slot per
    // worker, AOSP-style pool). 280 ms left ~5 ms slack and flaked.
    // 380 ms preserves the parallel / serialized split — the mutant
    // can't sleep less than 300 ms (literal `sleep(150)` × 2) so the
    // bound is still below it + its observed wake-from-sleep / RPC
    // wrap overhead (~80-100 ms = ~380-400 ms in practice). A tighter
    // bound would require either a sample-min over multiple runs or a
    // monotonic-clock barrier on the server side; both are scope creep.
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

/// **AC-12.1 — pool-exhausted condvar wait** (F2: `find_conn` *blocks*
/// on `slot_cv` when no slot is available; **never** a busy try-loop).
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
    // Phase B.1: 2 incoming slots (founding + attached); 3 client
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
    // macOS-latest CI under load measured 621 ms on the parallel path
    // (~221 ms slack — far above the ~50-100 ms assumed in the original
    // 550 ms bound). On the same loaded runner a serial mutant would land
    // at ~821 ms (600 + 221). The 700 ms upper bound therefore: (a) clears
    // the observed normal max with ~79 ms cushion, and (b) still trips on
    // any mutant whose slack is ≤ ~100 ms (the common case on Linux CI).
    // Same `1feaf52` pattern as the sibling pool test.
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

/// **AC-12.2 (Phase A — nested-callback slot pin / F8, scoped)**: on
/// a multi-outgoing client, when `client_transact` picks an outgoing
/// slot, the nested server→client callback **arriving on that same
/// socket** must dispatch on that slot — `find_conn`'s reentrant
/// match is keyed by `(session_ptr, slot_id)`, not `session_ptr`
/// alone (the pre-A key). The test forces slot 2 by parking slot 1
/// under a long-running `slow(...)`, then issues one `roundtrip(cb)`
/// on slot 2.
///
/// **F8 scoping note (documented next-increment).** This hybrid
/// architecture (A0b "N server-`RpcSessionInner` sharing one
/// `SharedSession`" + Phase-A client pool) has a known **cross-slot
/// proxy-cache aliasing** hazard: two server workers concurrently
/// unmarshalling the *same* client binder hit `state.remote_proxy`'s
/// shared cache, so the second caller's nested `proxy.transact`
/// re-routes through the *first* server inner's socket — wire
/// interleave / deadlock. The faithful fix is the **server-side
/// unification** ("one `RpcSessionInner` per session, slots in one
/// pool") so all server-side proxies live in one inner and
/// `findConnection` does the slot-pin uniformly — this is recorded
/// as the *next* Phase-A increment. The scoped single-thread test
/// here exercises the slot-pin without triggering the aliasing
/// (only slot 2 unmarshals the cb).

#[test]
fn pool_nested_callback_pins_to_forced_slot_single_thread() {
    let path = tmp_sock("a2pin");
    // Deterministic "parker entered slow on the server" signal — set
    // at the server-side `slow()` handler's entry. The prior version
    // used `sleep(30 ms)` after spawning the parker thread, which under
    // CI load could finish *before* the parker reached `find_conn` and
    // then both threads ended up on slot 1 (the test still passed —
    // including under the mutant the doc-comment claims to gate — so
    // it was a false-pass risk; see review M4).
    let slow_entered = Arc::new(AtomicBool::new(false));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Phase B.1: 2 incoming slots (founding + forced slot-2 echo).
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

/// AC-12.2-extended — two client threads make concurrent
/// `roundtrip(cb)` calls; each server worker must see a *distinct*
/// `RpcProxy`-backed nested send (Phase A4 / F8 — one inner per
/// session, slots in one pool). F8 mutant (N-inner / 1-shared
/// hybrid): the two workers' `state.remote_proxies` would hand out
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

/// **AC-12.3 (Phase A — oneway Option-1)**: on a multi-outgoing
/// client, all top-level **oneway** sends are pinned to the founding
/// slot (single-slot in-order delivery preserves the same-object
/// oneway FIFO from the pre-pool model), while **twoway** sends still
/// distribute (AC-12.1). 300 oneway bumps interleaved with twoway
/// echoes — all bumps must arrive (`count == 300`) and the wire on
/// the founding socket must not corrupt (twoways still round-trip).
///
/// **Trade-off recorded (HOL).** Option-1's price is head-of-line
/// blocking on the oneway-pinned slot — a slow oneway handler at the
/// peer stalls every other oneway through that slot. AOSP avoids
/// this via per-`mNodeForAddress` `asyncNumber` + receive-side
/// `asyncTodo` priority replay (Option-2 — F5/F6), deferred to Phase
/// C unless AC-12.6's real-libbinder multi-object-oneway gate forces
/// it.
///
/// **Order is not strictly asserted** (atomic counter — order-blind).
/// A strict-order gate would need a server-side sequence-recorder
/// (deferred); the per-slot in-order delivery is exercised here by
/// the count + the parallel twoway round-trips not corrupting the
/// shared wire.

#[test]
fn pool_oneway_pinned_to_founding_slot_multi_outgoing() {
    let path = tmp_sock("a3one");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Phase B.1: founding + one outgoing-echo ⇒ 2 incoming slots.
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
    assert_ne!(
        slot2, 1,
        "second slot has a fresh id (founding == 1) — otherwise the \
         'oneway pinned to founding' geometry collapses"
    );

    let root = Arc::new(EchoProxy(c.get_root().expect("get_root")));
    let n = 300i64;

    // Oneway bumps (pinned to slot 1) + concurrent twoway echoes
    // (distributed). The twoway echoes also exercise the wire on the
    // founding slot in between oneway sends — any frame interleave
    // would corrupt them.
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

    // Poll for the oneway bumps to drain.
    assert!(
        poll_until(|| root.count().unwrap() == n),
        "AC-12.3: all oneway bumps delivered (option-1 pin preserves \
         the per-slot oneway path); last observed count = {}",
        root.count().unwrap()
    );

    drop(root);
    drop(c);
    // _cu handles teardown.
}

// ---- AC-3.9 P6: no globals anywhere in the RPC stack ---------------

// Source-scan: needs `env!("CARGO_MANIFEST_DIR")/src/rpc/*.rs` at
// runtime, which is absent on a cross-compiled Android device.
#[cfg(not(target_os = "android"))]
#[test]
fn rpc_stack_has_no_globals() {
    // Static gate (master §6.2 V5 / AC-3.9): the RPC module must not
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
                // (mutable/immutable process global) is a P6 offender.
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
                    // field — the 2-6.B typed-stub descriptor stamp,
                    // owned per session, never a process global. A real
                    // `static` here is still caught by `static_item`.
                    if name == "proxy.rs" && l.contains("OnceLock") && !static_item {
                        continue;
                    }
                    // session.rs `DRIVING` is a `thread_local!`
                    // recursion marker that lets a same-thread nested
                    // call bypass the per-connection lock (AC-3.6). It
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

// ---- 2-9 Phase A / D1: accepted peer identity is the CLIENT --------

/// AC-9.1 / D1 (defect-regression, **deterministic mutant gate**).
///
/// A real cross-process connection: a forked child connects, the
/// parent `accept`s. `peer_identity()` on the accepted socket must be
/// the **client's** pid, never the server's own. Before subplan 2-9
/// Phase A the non-Linux `resolve_peer` returned `self_identity()` for
/// *every* socket, so a macOS/BSD server saw **itself** as the peer —
/// an authoritative-looking forged identity (the §0 contract defect).
/// Reverting Phase A makes `pid == std::process::id()` (the parent)
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
/// `serve_blocking` (or reverting `RpcProxy::link_to_death` to the old
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

// ---- 2-9 Phase B: opt-in authorization hook --------------------------

/// AC-9.4 — the opt-in `set_authorizer` gate runs *before any RPC
/// byte* and is backend-independent. A rejecting hook closes the
/// connection (the peer's next op is `DeadObject`, zero payload); an
/// accepting hook is transparent; unset is accept-all = the prior
/// behavior (every other test in this suite, unmodified, is the
/// additive-invariant evidence). Builds directly on Phase A: the
/// `PeerIdentity` the hook inspects is now the *real* peer.
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
