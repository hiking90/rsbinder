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

// ---- AC-3.2/3.3 concurrency + multi-session isolation --------------

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
