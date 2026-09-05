// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-17 Phase B — the unified entry API over RPC (`unix://`).
//! `serve` / `connect` / `Client` are the *only* rsbinder types touched
//! here; the kernel (`binder://`) form of the same calls is exercised on
//! REMOTE_LINUX by `test_entry_kernel_client_parity`.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`.

#![cfg(feature = "rpc")]
#![allow(non_snake_case)]

use std::path::PathBuf;

use rsbinder::{Interface, StatusCode, Strong};

include!(concat!(env!("OUT_DIR"), "/rpc_smoke.rs"));

use rpcsmoke::IRpcSmoke::{BnRpcSmoke, IRpcSmoke, IRpcSmokeAsync};

struct TaggedSvc {
    tag: String,
}
impl Interface for TaggedSvc {}
impl IRpcSmoke for TaggedSvc {
    fn r#echo(&self, s: &str) -> rsbinder::status::Result<String> {
        Ok(format!("{}:{}", self.tag, s))
    }
    fn r#add(&self, a: i32, b: i32) -> rsbinder::status::Result<i32> {
        Ok(a + b)
    }
    fn r#ping(&self) -> rsbinder::status::Result<()> {
        Ok(())
    }
}

fn tagged(tag: &str) -> Strong<dyn IRpcSmoke> {
    BnRpcSmoke::new_binder(TaggedSvc { tag: tag.into() })
}

struct SockPath(PathBuf);
impl SockPath {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("rsb_entry_{}_{}.sock", tag, std::process::id()));
        SockPath(p)
    }
    fn uri(&self, frag: &str) -> String {
        format!("unix://{}{frag}", self.0.display())
    }
}
impl Drop for SockPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The three-line shape from the plan: `serve(...).add(...).spawn()` and
/// `connect::<dyn I>(...)`. The proxy alone keeps the session alive.
#[test]
fn entry_serve_and_connect_over_unix() {
    let sock = SockPath::new("basic");
    let _guard = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add("hello", tagged("hello"))
        .expect("add")
        .spawn()
        .expect("spawn");

    let hello: Strong<dyn IRpcSmoke> = rsbinder::connect(&sock.uri("#hello")).expect("connect");
    assert_eq!(hello.r#echo("x").unwrap(), "hello:x");
    assert_eq!(hello.r#add(40, 2).unwrap(), 42);
    hello.r#ping().unwrap();
}

/// Plan 2-21 B-2 — dropping the guard while a client is still connected
/// ends the server: the workers are woken and joined instead of waited
/// on, and the client's next call fails. The guard is dropped on another
/// thread under a deadline so a regression fails the test rather than
/// hanging it: the guard's `stop_accepting` → join → `terminate` order is
/// what this pins.
#[test]
fn entry_guard_drop_ends_a_connected_client() {
    let sock = SockPath::new("guard-drop");
    let guard = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add("svc", tagged("svc"))
        .expect("add")
        .spawn()
        .expect("spawn");
    let svc: Strong<dyn IRpcSmoke> = rsbinder::connect(&sock.uri("#svc")).expect("connect");
    assert_eq!(svc.r#echo("x").unwrap(), "svc:x");

    let dropped = std::thread::spawn(move || drop(guard));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !dropped.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "ServerGuard::drop blocked with a client connected"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    dropped.join().expect("drop thread");
    assert!(svc.r#echo("after").is_err(), "the server ended the session");
}

/// `Client` resolves several names on one session; dropping it leaves
/// the proxies working (D5).
#[test]
fn entry_client_multi_lookup_and_proxy_outlives_client() {
    let sock = SockPath::new("multi");
    let server = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add("first", tagged("first"))
        .expect("add")
        .add("second", tagged("second"))
        .expect("add");
    let _guard = server.spawn().expect("spawn");

    let client = rsbinder::Client::open(&sock.uri("")).expect("open");
    let first: Strong<dyn IRpcSmoke> = client.get("first").expect("first");
    let second: Strong<dyn IRpcSmoke> = client.get("second").expect("second");
    assert!(client
        .try_get::<dyn IRpcSmoke>("nope")
        .expect("try_get")
        .is_none());
    assert_eq!(
        client.get::<dyn IRpcSmoke>("nope").unwrap_err(),
        StatusCode::NameNotFound
    );
    drop(client);
    assert_eq!(first.r#echo("a").unwrap(), "first:a");
    assert_eq!(second.r#echo("b").unwrap(), "second:b");
}

/// AC-22.4. Re-publishing a proxy of server A on server B is refused at
/// `add`, not left to fail on B's first client call. This is the mistake
/// the gateway pattern exists for: wrap the proxy in a local `Bn*`
/// (`BnRpcSmoke::new_binder(proxy)`) instead of forwarding the binder.
#[test]
fn entry_add_refuses_a_remote_binder() {
    let a = SockPath::new("gw_up");
    let b = SockPath::new("gw_down");

    let _up = rsbinder::serve(&a.uri(""))
        .expect("serve")
        .add("svc", tagged("upstream"))
        .expect("add")
        .spawn()
        .expect("spawn");

    let client = rsbinder::Client::open(&a.uri("")).expect("open");

    let refused = rsbinder::serve(&b.uri(""))
        .expect("serve")
        .add("svc", client.binder("svc").expect("binder"))
        .err();
    assert_eq!(
        refused,
        Some(StatusCode::InvalidOperation),
        "AC-22.4: a proxy cannot be re-published on another RPC server"
    );

    // A local service on the same endpoint is still accepted — the
    // refusal is about the binder, not about `b`.
    let _down = rsbinder::serve(&b.uri(""))
        .expect("serve")
        .add("svc", tagged("downstream"))
        .expect("add")
        .spawn()
        .expect("spawn");
}

/// Misuse is loud and early: a `#service` on `Client::open`, no service
/// on `connect`, an option for the wrong transport, and an unknown
/// scheme are all `BadValue`.
#[test]
fn entry_misuse_is_bad_value() {
    let sock = SockPath::new("misuse");
    let _guard = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add("svc", tagged("svc"))
        .expect("add")
        .spawn()
        .expect("spawn");

    assert_eq!(
        rsbinder::Client::open(&sock.uri("#svc")).err(),
        Some(StatusCode::BadValue)
    );
    assert_eq!(
        rsbinder::connect_binder(&sock.uri("")).err(),
        Some(StatusCode::BadValue)
    );
    assert_eq!(rsbinder::serve("ftp://x").err(), Some(StatusCode::BadValue));
    // Kernel-only option on an RPC server surfaces when the server starts.
    let sock2 = SockPath::new("misuse2");
    let r = rsbinder::serve(&sock2.uri(""))
        .expect("serve")
        .with(|o| o.call_restriction = Some(rsbinder::CallRestriction::None))
        .spawn();
    assert_eq!(r.err(), Some(StatusCode::BadValue));
    // RPC-only option on a kernel client, rejected before any binder init.
    let r = rsbinder::Client::open_with("binder://", |o, _| {
        o.timeout = Some(std::time::Duration::from_secs(1))
    });
    assert_eq!(r.err(), Some(StatusCode::BadValue));
}

/// `ServeOptions` reach the RPC server: `max_connections = 1` admits one
/// session and turns the second away (the cap is enforced per the
/// `RpcServer::set_max_connections` contract).
#[test]
fn entry_options_apply_to_rpc_server() {
    let sock = SockPath::new("opts");
    let _guard = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .with(|o| {
            o.max_connections = Some(1);
            o.handshake_timeout = Some(std::time::Duration::from_secs(5));
        })
        .add("svc", tagged("svc"))
        .expect("add")
        .spawn()
        .expect("spawn");

    let keep: Strong<dyn IRpcSmoke> = rsbinder::connect(&sock.uri("#svc")).expect("first session");
    assert_eq!(keep.r#echo("1").unwrap(), "svc:1");
    let second = rsbinder::Client::open_with(&sock.uri(""), |o, _| {
        o.timeout = Some(std::time::Duration::from_secs(2))
    })
    .and_then(|c| c.binder("svc"));
    assert!(
        second.is_err(),
        "second session must be refused under max_connections=1"
    );
    drop(keep);
}

/// `ClientOptions::handshake_timeout` is a plain public field, so a zero
/// duration can only be caught where the options are consumed. `open`
/// refuses it there with `BadValue` — before any connect — instead of
/// carrying it to a `set_read_timeout`/`connect_timeout` that rejects it
/// as an opaque I/O error, or (worse) dropping the caller's bound.
///
/// The zero is asserted on the **versioned** endpoint, where the option
/// does apply: without the facade's check the value reaches
/// `HandshakeDeadline::arm`, whose refusal is an opaque I/O error
/// (`Unknown`), so the assertion below is a real gate on the facade. The
/// r34 wire has no connection handshake for the option to bound, so there
/// it is refused whatever its value — `ClientOptions` promises an option
/// that does not apply is `BadValue`, never ignored.
#[test]
fn entry_zero_handshake_timeout_is_refused() {
    let sock13 = SockPath::new("zerohs13");
    let _guard13 = rsbinder::serve(&sock13.uri("?profile=android13plus"))
        .expect("serve")
        .add("svc", tagged("svc"))
        .expect("add")
        .spawn()
        .expect("spawn");

    let err = rsbinder::Client::open_with(&sock13.uri("?profile=android13plus"), |o, _| {
        o.handshake_timeout = Some(std::time::Duration::ZERO)
    })
    .expect_err("a zero handshake deadline must be refused before any connect");
    assert_eq!(err, rsbinder::StatusCode::BadValue);

    // Control: on the same endpoint a positive deadline connects.
    let ok = rsbinder::Client::open_with(&sock13.uri("?profile=android13plus"), |o, _| {
        o.handshake_timeout = Some(std::time::Duration::from_secs(5))
    })
    .and_then(|c| c.binder("svc"));
    assert!(ok.is_ok(), "a positive deadline still connects");

    // The r34 wire has no handshake phase, so the option is refused there
    // whatever its value.
    let sock = SockPath::new("zerohs");
    let _guard = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add("svc", tagged("svc"))
        .expect("add")
        .spawn()
        .expect("spawn");
    let err = rsbinder::Client::open_with(&sock.uri(""), |o, _| {
        o.handshake_timeout = Some(std::time::Duration::from_secs(5))
    })
    .expect_err("handshake_timeout does not apply to the r34 wire");
    assert_eq!(err, rsbinder::StatusCode::BadValue);
}

/// `connect_async` (feature `tokio`) resolves through `spawn_blocking`
/// and hands back a proxy the async stub can drive with `.await`.
#[test]
fn entry_connect_async_over_unix() {
    let sock = SockPath::new("async");
    let _guard = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add("svc", tagged("svc"))
        .expect("add")
        .spawn()
        .expect("spawn");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        // Sync interface first: `connect_async` is transport/interface
        // agnostic — it is the blocking connect moved off the runtime.
        let sync: Strong<dyn IRpcSmoke> = rsbinder::connect_async(&sock.uri("#svc"))
            .await
            .expect("connect_async");
        assert_eq!(sync.r#echo("a").unwrap(), "svc:a");

        // Async view of the same service, driven with `.await`.
        let r#async =
            rsbinder::connect_async::<dyn IRpcSmokeAsync<rsbinder::Tokio>>(&sock.uri("#svc"))
                .await
                .expect("connect_async (async interface)");
        assert_eq!(r#async.r#echo("b").await.unwrap(), "svc:b");
    });
}

/// `?profile=android13plus` selects the versioned wire on both sides.
#[test]
fn entry_android13plus_profile_roundtrip() {
    let sock = SockPath::new("a13");
    let _guard = rsbinder::serve(&sock.uri("?profile=android13plus"))
        .expect("serve")
        .add("svc", tagged("svc"))
        .expect("add")
        .spawn()
        .expect("spawn");
    let client = rsbinder::Client::open(&sock.uri("?profile=android13plus")).expect("open");
    assert_eq!(client.session().unwrap().wire_protocol_version(), Some(2));
    let svc: Strong<dyn IRpcSmoke> = client.get("svc").expect("get");
    assert_eq!(svc.r#echo("v2").unwrap(), "svc:v2");
}
