// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-10b — `Reconnecting` over RPC (`unix://`), hermetic: the server is
//! this process's own `serve(...).spawn()`, stopped and started again on the
//! same socket path to stand in for a restart.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`.

#![cfg(feature = "rpc")]
#![allow(non_snake_case)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rsbinder::reconnect::{ConnectError, ReconnectPolicy, Reconnecting};
use rsbinder::{Interface, ServerGuard, Status, StatusCode, Strong};

include!(concat!(env!("OUT_DIR"), "/rpc_smoke.rs"));

use rpcsmoke::IRpcSmoke::{BnRpcSmoke, IRpcSmoke};

/// Echoes with its tag, so a reply tells which server instance answered.
struct Svc {
    tag: &'static str,
}
impl Interface for Svc {}
impl IRpcSmoke for Svc {
    fn r#echo(&self, s: &str) -> rsbinder::BinderResult<String> {
        if s == "dead" {
            // A live handler relaying DeadObject: the session is fine.
            return Err(StatusCode::DeadObject.into());
        }
        if s == "slow" {
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(format!("{}:{s}", self.tag))
    }
    fn r#add(&self, a: i32, b: i32) -> rsbinder::BinderResult<i32> {
        Ok(a + b)
    }
    fn r#ping(&self) -> rsbinder::BinderResult<()> {
        Ok(())
    }
    fn r#fail(&self, code: i32) -> rsbinder::BinderResult<()> {
        Err(Status::new_service_specific_error(code, None))
    }
}

struct Sock(PathBuf);
impl Sock {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("rsb_reconnect_{tag}_{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Sock(p)
    }
    fn uri(&self, query: &str, frag: &str) -> String {
        format!("unix://{}{query}{frag}", self.0.display())
    }
    fn serve(&self, query: &str, tag: &'static str) -> ServerGuard {
        rsbinder::serve(&self.uri(query, ""))
            .expect("serve")
            .add("smoke", BnRpcSmoke::new_binder(Svc { tag }))
            .expect("add")
            .spawn()
            .expect("spawn")
    }
}
impl Drop for Sock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const PROFILE: &str = "?profile=android13plus";

fn echo(h: &Reconnecting<dyn IRpcSmoke>, s: &str) -> Result<String, Status> {
    h.with(|p| p.r#echo(s))
}

/// Up to five seconds for `f`; the last evaluation is the verdict.
fn eventually(mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    f()
}

/// AC-10b.1 (a): a session with an incoming connection notices the restart by itself.
#[test]
fn reconnects_after_a_restart_noticed_by_death_notification() {
    let sock = Sock::new("death");
    let guard = sock.serve(PROFILE, "a");
    let connects = Arc::new(AtomicU32::new(0));
    let counted = connects.clone();
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri(PROFILE, "#smoke"))
        .options(|o, _| o.incoming_connections = Some(1))
        .on_connect(move |c| {
            counted.fetch_add(1, Ordering::SeqCst);
            c.proxy().r#ping()?;
            Ok(())
        })
        .build()
        .expect("build");
    assert_eq!(h.generation(), 1);
    assert_eq!(echo(&h, "x").unwrap(), "a:x");

    drop(guard);
    let _guard = sock.serve(PROFILE, "b");
    // No call in between: only the death notification can start this reconnect.
    assert!(
        eventually(|| h.generation() == 2),
        "reconnected without a call"
    );
    let p = h.current().expect("connected");
    assert_eq!(p.r#echo("x").unwrap(), "b:x");
    assert_eq!(connects.load(Ordering::SeqCst), 2, "on_connect ran again");
    assert_eq!(echo(&h, "y").unwrap(), "b:y");
}

/// AC-10b.1 (b): r34 misses the restart; the next call sees the closed socket and runs anew.
#[test]
fn first_call_after_an_unnoticed_restart_runs_on_the_new_connection() {
    let sock = Sock::new("r34");
    let guard = sock.serve("", "a");
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .build()
        .expect("build");
    assert_eq!(echo(&h, "x").unwrap(), "a:x");

    drop(guard);
    let _guard = sock.serve("", "b");
    let runs = AtomicU32::new(0);
    let reply = h.with(|p| {
        runs.fetch_add(1, Ordering::SeqCst);
        p.r#echo("x")
    });
    assert_eq!(reply.unwrap(), "b:x");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the closure ran exactly once"
    );
    assert_eq!(h.generation(), 2);
}

/// AC-10b.2 (a): with the server down, a call between attempts does not wait.
#[test]
fn a_call_between_attempts_fails_at_once() {
    let sock = Sock::new("down");
    let guard = sock.serve("", "a");
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .build()
        .expect("build");
    drop(guard);
    // The call that finds the close waits for the first attempt (refused at once), then fails.
    let err = echo(&h, "x").unwrap_err();
    assert_eq!(err.transaction_error(), StatusCode::DeadObject);
    assert!(eventually(|| h.last_error().is_some()), "an attempt failed");
    let t0 = Instant::now();
    let err = echo(&h, "x").unwrap_err();
    assert_eq!(err.transaction_error(), StatusCode::DeadObject);
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "{:?}",
        t0.elapsed()
    );
}

/// AC-10b.7b: a client may start before its server.
#[test]
fn build_before_the_server_connects_in_the_background() {
    let sock = Sock::new("late");
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .build()
        .expect("a missing server is not a build error");
    assert_eq!(h.generation(), 0);
    assert!(h.current().is_err());
    let _guard = sock.serve("", "a");
    let p = h
        .wait_connected(Some(Duration::from_secs(5)))
        .expect("connected");
    assert_eq!(p.r#echo("x").unwrap(), "a:x");
    assert_eq!(h.generation(), 1);
}

/// AC-10b.10c: what another attempt cannot change is a build error.
#[test]
fn build_refuses_what_a_retry_cannot_fix() {
    let sock = Sock::new("refuse");
    let _guard = sock.serve("", "a");
    let build = |uri: String| Reconnecting::<dyn IRpcSmoke>::builder(&uri).build().err();
    assert_eq!(build("bogus://x".into()), Some(StatusCode::BadValue));
    assert_eq!(build("binder://".into()), Some(StatusCode::BadValue));
    // Multi-connection options need the android13plus profile: setup, not the transport.
    let r = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .options(|o, _| o.incoming_connections = Some(1))
        .build()
        .err();
    assert_eq!(r, Some(StatusCode::BadValue));
    // An RPC cast does not ask the server, so a root of another interface fails at the call.
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", ""))
        .build()
        .expect("the cast does not check the interface");
    assert_eq!(
        echo(&h, "x").unwrap_err().transaction_error(),
        StatusCode::BadType
    );
    assert_eq!(h.generation(), 1);
}

/// AC-10b.10c: `on_connect` stops the helper only by `ConnectError::stop`.
#[test]
fn on_connect_stops_only_when_asked() {
    let sock = Sock::new("hook");
    let _guard = sock.serve("", "a");
    let stopped = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .on_connect(|_| Err(ConnectError::stop(StatusCode::PermissionDenied)))
        .build()
        .err();
    assert_eq!(stopped, Some(StatusCode::PermissionDenied));

    let tries = Arc::new(AtomicU32::new(0));
    let counted = tries.clone();
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .on_connect(move |_| {
            // `?` on a code that also names a configuration error: still a retry.
            if counted.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(StatusCode::BadValue)?;
            }
            Ok(())
        })
        .build()
        .expect("a retried on_connect is not a build error");
    let p = h
        .wait_connected(Some(Duration::from_secs(5)))
        .expect("third try connects");
    assert_eq!(p.r#echo("x").unwrap(), "a:x");
    assert_eq!(tries.load(Ordering::SeqCst), 3);
}

/// A call ending its session with `TimedOut` reconnects: the session's end is the signal.
#[test]
fn a_call_that_ends_the_session_reconnects_without_another_call() {
    let sock = Sock::new("deadline");
    let _guard = sock.serve("", "a");
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .options(|o, _| o.timeout = Some(Duration::from_millis(100)))
        .build()
        .expect("build");
    let err = echo(&h, "slow").unwrap_err();
    assert_eq!(err.transaction_error(), StatusCode::TimedOut);
    assert!(
        eventually(|| h.generation() == 2),
        "reconnected without a call"
    );
}

/// AC-10b.7 (c): `DeadObject` from a live handler is the handler's answer, not a session end.
#[test]
fn a_relayed_dead_object_does_not_reconnect() {
    let sock = Sock::new("relay");
    let _guard = sock.serve("", "a");
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .build()
        .expect("build");
    let err = echo(&h, "dead").unwrap_err();
    assert_eq!(err.transaction_error(), StatusCode::DeadObject);
    assert_eq!(echo(&h, "x").unwrap(), "a:x");
    assert_eq!(h.generation(), 1, "still the first connection");
}

/// AC-10b.10d: a closed helper's weak handle and the attempt cap.
#[test]
fn weak_handle_and_attempt_cap() {
    let sock = Sock::new("cap");
    let mut policy = ReconnectPolicy::default();
    policy.max_attempts = Some(2);
    policy.initial = Duration::from_millis(1);
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .policy(policy)
        .build()
        .expect("build");
    let weak = h.downgrade();
    assert!(eventually(|| h.is_closed()), "two failed attempts close it");
    assert!(h.last_error().is_some());
    assert_eq!(
        h.wait_connected(None).err(),
        h.last_error(),
        "a closed helper reports why"
    );
    assert_eq!(weak.generation(), Some(0));
    drop(h);
    assert_eq!(weak.generation(), None);
    assert_eq!(
        weak.with(|p| p.r#echo("x"))
            .unwrap_err()
            .transaction_error(),
        StatusCode::DeadObject
    );
}

/// AC-10b.9: drop does not wait for a reconnect stuck on a server that never answers.
#[test]
fn drop_does_not_wait_for_a_stuck_attempt() {
    let sock = Sock::new("stuck");
    let guard = sock.serve("", "a");
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .build()
        .expect("build");
    drop(guard);
    // Accepts and then says nothing: the attempt's lookup waits for a reply that never comes.
    let listener = std::os::unix::net::UnixListener::bind(&sock.0).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let (accepted_tx, accepted) = std::sync::mpsc::channel();
    let silent = std::thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if let Ok((stream, _)) = listener.accept() {
                accepted_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_secs(2));
                drop(stream);
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let _ = echo(&h, "x");
    accepted
        .recv_timeout(Duration::from_secs(5))
        .expect("the reconnect attempt reached the silent server");
    let t0 = Instant::now();
    drop(h);
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "{:?}",
        t0.elapsed()
    );
    silent.join().unwrap();
}

/// `connected().await` waits across attempts without a blocked thread, and ends on close.
#[test]
fn connected_resolves_once_the_server_is_up() {
    let sock = Sock::new("async");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .build()
        .expect("build");
    let uri = sock.uri("", "");
    let late = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        rsbinder::serve(&uri)
            .expect("serve")
            .add("smoke", BnRpcSmoke::new_binder(Svc { tag: "a" }))
            .expect("add")
            .spawn()
            .expect("spawn")
    });
    let p = rt
        .block_on(async { tokio::time::timeout(Duration::from_secs(5), h.connected()).await })
        .expect("within the bound")
        .expect("connected");
    assert_eq!(p.r#echo("x").unwrap(), "a:x");
    let _guard = late.join().unwrap();

    let mut policy = ReconnectPolicy::default();
    policy.max_attempts = Some(1);
    let doomed =
        Reconnecting::<dyn IRpcSmoke>::builder(&Sock::new("async_doomed").uri("", "#smoke"))
            .policy(policy)
            .build()
            .expect("build");
    let r = rt
        .block_on(async { tokio::time::timeout(Duration::from_secs(5), doomed.connected()).await });
    assert!(r.expect("a closing helper wakes the waiter").is_err());
}

/// `Strong` of the async-free interface is what `current` hands out.
#[test]
fn current_hands_out_the_proxy_without_waiting() {
    let sock = Sock::new("current");
    let _guard = sock.serve("", "a");
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&sock.uri("", "#smoke"))
        .build()
        .expect("build");
    let p: Strong<dyn IRpcSmoke> = h.current().expect("connected");
    assert_eq!(p.r#add(40, 2).unwrap(), 42);
}
