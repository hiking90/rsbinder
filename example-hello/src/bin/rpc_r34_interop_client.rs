// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! rsbinder client half of the r34 wire STAGE3 gate (plan 2-25 Phase 7,
//! gate B): an rsbinder `RpcSession` on the default r34 profile against a
//! real android-12 libbinder `RpcServer` (`cpp/rpc_r34_interop.cpp server`,
//! `setMaxThreads(3)`) on an SDK 31 emulator. Run it through
//! `cpp/run_rpc_r34_interop.sh`.
//!
//! ```text
//! rpc_r34_interop_client <sock> <server_pid>
//! ```
//!
//! The last check kills `<server_pid>` during a call, so the server must be
//! started again before another run. Exit 0 means every check passed.

use std::time::{Duration, Instant};

use rsbinder::rpc::{FileDescriptorTransportMode, RpcProxy, RpcSession};
use rsbinder::*;

// Must match `cpp/rpc_r34_interop.cpp`.
const ROOT_DESC: &str = "rsbinder.test.IR34Interop";
const CALLBACK_DESC: &str = "rsbinder.test.IR34Callback";

const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_SLOW_ECHO: TransactionCode = FIRST_CALL_TRANSACTION + 1;
const TX_ONEWAY: TransactionCode = FIRST_CALL_TRANSACTION + 2;
const TX_GET_LOG: TransactionCode = FIRST_CALL_TRANSACTION + 3;
const TX_INVOKE_CALLBACK: TransactionCode = FIRST_CALL_TRANSACTION + 4;
const TX_NULL_BINDER: TransactionCode = FIRST_CALL_TRANSACTION + 5;
const TX_HOLD: TransactionCode = FIRST_CALL_TRANSACTION + 6;
const TX_RELEASE: TransactionCode = FIRST_CALL_TRANSACTION + 7;
const TX_ECHO_BINDER: TransactionCode = FIRST_CALL_TRANSACTION + 8;
const TX_BYTES: TransactionCode = FIRST_CALL_TRANSACTION + 11;
const TX_CB_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;

/// The android-12 server's `setMaxThreads`, passed by the runner.
const SERVER_MAX_THREADS: u32 = 3;

type BoxResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Callback;

impl Interface for Callback {}
impl Remotable for Callback {
    fn descriptor() -> &'static str {
        CALLBACK_DESC
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
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&format!("cb-echo:{s}"))
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
        Ok(())
    }
}

fn fail(code: i32, what: String) -> ! {
    eprintln!("[rsbinder-client] FAIL ({code}): {what}");
    std::process::exit(code)
}

fn proxy(root: &SIBinder) -> &RpcProxy {
    (**root)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .unwrap_or_else(|| fail(2, "root is not an RpcProxy".into()))
}

/// Twoway `code` with `fill` writing the arguments; returns the reply past
/// its status header.
fn call(
    rp: &RpcProxy,
    code: TransactionCode,
    fill: impl FnOnce(&mut Parcel) -> Result<()>,
) -> Result<Parcel> {
    let mut d = rp.build_request(ROOT_DESC)?;
    fill(&mut d)?;
    let mut r = rp
        .transact(code, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    let st: Status = r.read()?;
    if !st.is_ok() {
        return Err(StatusCode::from(st));
    }
    Ok(r)
}

fn echo(rp: &RpcProxy, s: &str) -> Result<String> {
    call(rp, TX_ECHO, |d| d.write(&s.to_string()))?.read()
}

fn poll_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if pred() {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn main() -> BoxResult<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("rsbinder::rpc=info"),
    )
    .format_timestamp_millis()
    .init();

    let mut args = std::env::args().skip(1);
    let sock = args
        .next()
        .unwrap_or_else(|| "/data/local/tmp/a12r34.sock".to_string());
    let server_pid: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| {
        fail(
            2,
            "usage: rpc_r34_interop_client <sock> <server_pid>".into(),
        )
    });

    // (B1) root + echo over one connection.
    let session = RpcSession::setup_unix_client(&sock)?;
    let root = session.get_root()?;
    let rp = proxy(&root);
    let got = echo(rp, "sanity")?;
    if got != "sanity" {
        fail(10, format!("echo got {got:?}"));
    }
    println!("(B1) PASS: root + echo");

    // (B2) GET_MAX_THREADS.
    let negotiated = session.negotiate(8)?;
    if negotiated != SERVER_MAX_THREADS {
        fail(
            11,
            format!("negotiate(8) = {negotiated}, want {SERVER_MAX_THREADS}"),
        );
    }
    println!("(B2) PASS: negotiate(8) = {negotiated}");

    // (B3) GET_SESSION_ID is android-12's int32.
    let id = session.get_session_id()?;
    if id.len() != 4 {
        fail(12, format!("session id {id:02x?} is not 4 bytes"));
    }
    let id_value = i32::from_le_bytes([id[0], id[1], id[2], id[3]]);
    println!("(B3) PASS: get_session_id() = {id:02x?} ({id_value})");

    // (B4) GET_FD_MODE is an rsbinder extension; android-12 answers
    // UNKNOWN_TRANSACTION, read as None.
    let mode = session.negotiate_fd_transport(FileDescriptorTransportMode::Unix)?;
    if mode != FileDescriptorTransportMode::None {
        fail(13, format!("negotiate_fd_transport(Unix) = {mode:?}"));
    }
    println!("(B4) PASS: negotiate_fd_transport(Unix) = None");

    // (B5) nested callback: the server first asks for the callback's
    // descriptor (AIBinder_associateClass), then calls it, both inside our
    // outstanding call.
    let cb: SIBinder = Interface::as_binder(&Binder::new(Callback));
    let got: String = call(rp, TX_INVOKE_CALLBACK, |d| {
        d.write(&cb)?;
        d.write(&"ping".to_string())
    })?
    .read()?;
    if got != "cb-echo:ping" {
        fail(14, format!("nested callback got {got:?}"));
    }
    println!("(B5) PASS: nested callback");

    // (B6) null binder then int, in both directions.
    for arg in [None, Some(cb.clone())] {
        let mut r = call(rp, TX_NULL_BINDER, |d| {
            d.write(&arg)?;
            d.write(&41i32)
        })?;
        let was_null: i32 = r.read()?;
        let back: Option<SIBinder> = r.read()?;
        let next: i32 = r.read()?;
        if was_null != i32::from(arg.is_none()) || back.is_some() || next != 42 {
            fail(
                15,
                format!(
                    "null binder: was_null={was_null} back={} next={next}",
                    back.is_some()
                ),
            );
        }
    }
    println!("(B6) PASS: null binder then int, both directions");

    // (B7) our own binder comes back as itself.
    let back: SIBinder = call(rp, TX_ECHO_BINDER, |d| d.write(&cb))?.read()?;
    if back != cb {
        fail(16, "echoed binder is not our callback".into());
    }
    drop(back);
    println!("(B7) PASS: own binder echoed back as the local object");

    // (B8) oneway order (android-12 checks asyncNumber per node).
    const N: i32 = 20;
    for i in 0..N {
        let mut d = rp.build_request(ROOT_DESC)?;
        d.write(&i)?;
        rp.transact(TX_ONEWAY, &d, FLAG_ONEWAY)?;
    }
    let mut log: Vec<i32> = Vec::new();
    let full = poll_until(Duration::from_secs(3), || {
        log = call(rp, TX_GET_LOG, |_| Ok(()))
            .and_then(|mut r| r.read())
            .unwrap_or_default();
        log.len() >= N as usize
    });
    if !full || log != (0..N).collect::<Vec<_>>() {
        fail(17, format!("oneway log {log:?}"));
    }
    println!("(B8) PASS: {N} oneway calls in order");

    // (B9) one binder held twice by the server, then released: our node
    // count returns to its baseline.
    let base = session.local_node_count();
    let held: SIBinder = Interface::as_binder(&Binder::new(Callback));
    call(rp, TX_HOLD, |d| d.write(&held))?;
    call(rp, TX_HOLD, |d| d.write(&held))?;
    drop(held);
    let mid = session.local_node_count();
    if mid != base + 1 {
        fail(18, format!("nodes {base} -> {mid} with one binder held"));
    }
    let released: i32 = call(rp, TX_RELEASE, |_| Ok(()))?.read()?;
    if released != 2 {
        fail(19, format!("released {released}"));
    }
    let mut after = mid;
    let back_to_base = poll_until(Duration::from_secs(3), || {
        // A DEC_STRONG is read while waiting for a reply.
        let _ = echo(rp, "d");
        after = session.local_node_count();
        after == base
    });
    if !back_to_base {
        fail(
            20,
            format!("nodes {base} -> {mid} -> {after} after release"),
        );
    }
    println!("(B9) PASS: binder held twice then released, nodes {base} -> {mid} -> {after}");

    // (B10) 90 KB both ways stays under android-12's 100,000-byte limit.
    let mut r = call(rp, TX_BYTES, |d| {
        d.write(&0i32)?;
        d.write(&vec![0x5au8; 90_000])
    })?;
    let seen: i32 = r.read()?;
    if seen != 90_000 {
        fail(21, format!("90 KB request: server saw {seen}"));
    }
    let mut r = call(rp, TX_BYTES, |d| {
        d.write(&90_000i32)?;
        d.write(&Vec::<u8>::new())
    })?;
    let _: i32 = r.read()?;
    let big: Vec<u8> = r.read()?;
    if big.len() != 90_000 {
        fail(22, format!("90 KB reply: {} bytes", big.len()));
    }
    println!("(B10) PASS: 90 KB request and reply");

    // (B11) a 110 KB request: android-12 refuses the body and closes the
    // connection, which was the session's only one, so the session ends on
    // both sides.
    let res = call(rp, TX_BYTES, |d| {
        d.write(&0i32)?;
        d.write(&vec![0x5au8; 110_000])
    });
    match res {
        Err(StatusCode::DeadObject) => {}
        other => fail(23, format!("110 KB request: {:?}", other.map(|_| ()))),
    }
    match echo(rp, "after") {
        Err(StatusCode::DeadObject) => {}
        other => fail(24, format!("call after the 110 KB request: {other:?}")),
    }
    println!("(B11) PASS: 110 KB request ends the session (DeadObject)");
    drop(root);
    drop(session);

    // (B12) server killed during a call. No death link: an r34 client reads
    // its one connection only while it waits for a reply, so rsbinder refuses
    // `link_to_death` there.
    let session = RpcSession::setup_unix_client(&sock)?;
    let root = session.get_root()?;
    let rp = proxy(&root);
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        // SAFETY: kill(2) with a pid and a signal number; no memory is passed.
        unsafe { libc::kill(server_pid, libc::SIGKILL) }
    });
    let started = Instant::now();
    let res = call(rp, TX_SLOW_ECHO, |d| {
        d.write(&"slow".to_string())?;
        d.write(&5000i32)
    });
    let elapsed = started.elapsed();
    if killer.join().expect("killer thread") != 0 {
        fail(25, format!("kill({server_pid}) failed"));
    }
    match res {
        Err(StatusCode::DeadObject) => {}
        other => fail(
            26,
            format!("call during server kill: {:?}", other.map(|_| ())),
        ),
    }
    match echo(rp, "after-kill") {
        Err(StatusCode::DeadObject) => {}
        other => fail(27, format!("call after server kill: {other:?}")),
    }
    println!("(B12) PASS: server killed mid-call -> DeadObject after {elapsed:?}");

    println!("GATE B PASS");
    Ok(())
}
