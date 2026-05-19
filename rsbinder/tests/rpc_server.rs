// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Subplan 2-3: multi-session `RpcServer`, real-process e2e, threads,
//! `getRemoteMaxThreads` negotiation, oneway FIFO, nested callbacks,
//! timeout, lifecycle, and the P6 no-global gate.
//!
//! Separate test binary (master §6). P6: each test builds its own
//! server + sessions ⇒ parallel-safe, no `--test-threads=1`.

#![cfg(feature = "rpc")]

use std::sync::atomic::{AtomicI64, Ordering};
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
    Interface::as_binder(&Binder::new(BnEcho2(Box::new(EchoSvc {
        counter,
        deeper: false,
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

    server.shutdown();
    let _ = bg.join();
    let _ = std::fs::remove_file(&path);
}

// ---- AC-3.3 multi-session isolation --------------------------------

#[test]
fn concurrent_clients_isolated_sessions() {
    let path = tmp_sock("iso");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
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

    server.shutdown();
    let _ = bg.join();
    let _ = std::fs::remove_file(&path);
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
    server.shutdown();
    let _ = bg.join();
    let _ = std::fs::remove_file(&path);
}

// ---- AC-3.5 oneway FIFO + non-blocking send ------------------------

#[test]
fn oneway_fifo_and_nonblocking() {
    let path = tmp_sock("ow");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(counter.clone()));
    let bg = server.run_background();
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

    server.shutdown();
    let _ = bg.join();
    let _ = std::fs::remove_file(&path);
}

// ---- AC-3.6 nested server→client callback --------------------------

#[test]
fn nested_callback_no_deadlock() {
    let path = tmp_sock("nest");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
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

    server.shutdown();
    let _ = bg.join();
    let _ = std::fs::remove_file(&path);
}

// ---- AC-3.8 timeout (hung server) ----------------------------------

#[test]
fn client_timeout_on_hung_server() {
    let path = tmp_sock("to");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    let bg = server.run_background();
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

    server.shutdown();
    let _ = bg.join();
    let _ = std::fs::remove_file(&path);
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
        server.shutdown();
        let _ = bg.join();
        // Reap the per-connection session worker too (Drop/shutdown
        // only join the accept loop); the worker has already drained on
        // the client drop above, so this just collects its handle.
        server.join_workers();
        let _ = std::fs::remove_file(&path);
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
    server.shutdown();
    let _ = bg.join();
    server.join_workers();
    let _ = std::fs::remove_file(&path);
}

// ---- AC-3.9 P6: no globals anywhere in the RPC stack ---------------

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
    let path = tmp_sock("authz_no");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    server.set_authorizer(|_peer| false);
    let bg = server.run_background();
    wait_for_sock(&path);
    {
        let client = RpcSession::setup_unix_client(&path).expect("connect");
        client.set_timeout(Some(Duration::from_secs(3)));
        assert!(
            client.get_root().is_err(),
            "a rejecting authorizer must close the connection (zero RPC bytes)"
        );
    }
    server.shutdown();
    let _ = bg.join();
    let _ = std::fs::remove_file(&path);

    // (2) Accepting hook (inspects the real PeerIdentity) ⇒ transparent.
    let path = tmp_sock("authz_yes");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_root(make_service(Arc::new(AtomicI64::new(0))));
    server.set_authorizer(|peer| matches!(peer, PeerIdentity::Local { .. }));
    let bg = server.run_background();
    wait_for_sock(&path);
    {
        let client = RpcSession::setup_unix_client(&path).expect("connect");
        let root = EchoProxy(client.get_root().expect("get_root (authorized)"));
        assert_eq!(root.echo("authorized").unwrap(), "authorized");
    }
    server.shutdown();
    let _ = bg.join();
    let _ = std::fs::remove_file(&path);
}
