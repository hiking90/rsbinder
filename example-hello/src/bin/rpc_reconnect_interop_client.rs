// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-10b AC-10b.10 STAGE3 — `rsbinder::Reconnecting` against a real
//! libbinder `RpcServer` serving only its root object
//! (`cpp/rpc_reconnect_interop_launcher.cpp`), restarted on the same socket
//! by `cpp/run_rpc_reconnect_interop.sh`.
//!
//! The URI has no `#name`: libbinder has no service directory, so the helper
//! must reach the root. Two modes, picked by the second argument:
//!
//! - `probe`: no incoming connection, so nothing tells the client the server
//!   went away; the first call after the restart finds the closed socket
//!   before it is sent and runs on the new connection.
//! - `death`: one incoming connection, so the session ends with the server
//!   and the helper reconnects with no call at all.
//!
//! Protocol with the script: print `PHASE1` once instance `a` answered, then
//! wait (up to 60 s) for the third argument, a marker file the script creates
//! once instance `b` is up. The first call after that must answer from `b`
//! — no failed call before it. Exit 0 and print `PASS` on success.

use std::time::{Duration, Instant};

use example_hello::IHello;
use rsbinder::Reconnecting;

fn main() {
    let mut args = std::env::args().skip(1);
    let sock = args
        .next()
        .expect("usage: rpc_reconnect_interop_client SOCKET probe|death");
    let mode = args
        .next()
        .expect("usage: rpc_reconnect_interop_client SOCKET probe|death MARKER");
    let restarted = args
        .next()
        .expect("usage: rpc_reconnect_interop_client SOCKET probe|death MARKER");
    let incoming = match mode.as_str() {
        "probe" => None,
        "death" => Some(1),
        other => panic!("unknown mode {other}"),
    };

    let uri = format!("unix://{sock}?profile=android13plus");
    let h = Reconnecting::<dyn IHello>::builder(&uri)
        .options(move |o, _| {
            o.incoming_connections = incoming;
            o.timeout = Some(Duration::from_secs(5));
        })
        .build()
        .expect("build");
    let first = h.with(|p| p.echo("x")).expect("echo on instance a");
    assert_eq!(first, "a:x");
    assert_eq!(h.generation(), 1);
    println!("PHASE1");

    let end = Instant::now() + Duration::from_secs(60);
    while !std::path::Path::new(&restarted).exists() {
        assert!(
            Instant::now() < end,
            "the script never restarted the server"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    if incoming.is_some() {
        // Only the death notification can start this reconnect: no call is made.
        while h.generation() < 2 {
            assert!(Instant::now() < end, "no reconnect after the restart");
            std::thread::sleep(Duration::from_millis(20));
        }
        println!("reconnected without a call (generation {})", h.generation());
    } else {
        // Nothing can tell this session the server left; only the next call can.
        assert_eq!(h.generation(), 1, "no reconnect before a call");
    }
    let reply = h
        .with(|p| p.echo("y"))
        .expect("the first call after the restart");
    assert_eq!(reply, "b:y");
    assert_eq!(h.generation(), 2, "exactly one reconnect");
    println!("reply {reply} (generation {})", h.generation());
    println!("PASS");
}
