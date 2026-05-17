// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Subplan 2-6.B AC-6.3 / AC-6.4: the **generated** `Bp*` stub —
//! which now emits `as_remote().ok_or(BadType)?` instead of the
//! kernel-only `as_proxy().unwrap()` — driven over the RPC transport,
//! with the **generated server stub reused unmodified**. No
//! hand-written proxy (contrast `rpc_e2e.rs`, subplan 2-2): this is
//! the single-stub end state. The interface (`rpcsmoke.IRpcSmoke`) is
//! compiled by `build.rs` to `OUT_DIR/rpc_smoke.rs`.
//!
//! - **AC-6.3**: an arbitrary AIDL interface round-trips over `mem`,
//!   `unix` (and `tcp_debug` when its feature is on) through the
//!   generated `BpRpcSmoke`, whose `from_binder` stamps the RPC
//!   descriptor in place (subplan 2-6.B).
//! - **AC-6.4**: a non-remote / wrong binder is graceful — the
//!   generator's old `.unwrap()` panic is gone (golden-proven), and
//!   `try_from` yields a value or `Err`, never a panic.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`, so it never
//! shares a process with the kernel-binder unit tests (master §6).
//! P6: each test builds its own session pair → parallel-safe.

#![cfg(feature = "rpc")]
#![allow(non_snake_case)]

use std::thread;

use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{AddressSpace, RpcSession, RpcTransport};
use rsbinder::{Binder, FromIBinder, Interface, Remotable, SIBinder};

include!(concat!(env!("OUT_DIR"), "/rpc_smoke.rs"));

use rpcsmoke::IRpcSmoke::{BnRpcSmoke, IRpcSmoke};

// ---- server impl (the generated stub is reused unmodified) ----------

struct SmokeSvc;
impl Interface for SmokeSvc {}
impl IRpcSmoke for SmokeSvc {
    fn r#echo(&self, s: &str) -> rsbinder::status::Result<String> {
        Ok(s.to_string())
    }
    fn r#add(&self, a: i32, b: i32) -> rsbinder::status::Result<i32> {
        Ok(a + b)
    }
    fn r#ping(&self) -> rsbinder::status::Result<()> {
        Ok(())
    }
}

fn root() -> SIBinder {
    BnRpcSmoke::new_binder(SmokeSvc).as_binder()
}

// ---- AC-6.3: generated stub e2e over each transport -----------------

fn run(server_t: Box<dyn RpcTransport>, client_t: Box<dyn RpcTransport>) {
    let server = RpcSession::new(server_t, AddressSpace::Acceptor);
    server.set_root(root());
    let server_for_thread = server.clone();
    let handle = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });

    {
        let client = RpcSession::new(client_t, AddressSpace::Initiator);
        let sib = client.get_root().expect("get_root");

        // The single generated stub: `try_from` → `Proxy::from_binder`
        // (stamps the RPC descriptor on the cached proxy in place) →
        // generated methods use `as_remote().ok_or(BadType)?`.
        let smoke = <dyn IRpcSmoke as FromIBinder>::try_from(sib)
            .expect("AC-6.3: generated BpRpcSmoke resolves from an RPC binder");

        // Scalar + string round-trip, exact values (build_parcel →
        // prepare_transact[interface token from the stamped descriptor]
        // → submit_transact → read_response, all generated).
        assert_eq!(smoke.r#echo("hello generated rpc").unwrap(), "hello generated rpc");
        assert_eq!(smoke.r#echo("").unwrap(), "");
        assert_eq!(smoke.r#add(2, 3).unwrap(), 5);
        assert_eq!(smoke.r#add(-7, 7).unwrap(), 0);
        // oneway emit path (FLAG_ONEWAY branch of the generator).
        smoke.r#ping().unwrap();
        // Re-call to prove the stamped descriptor is stable across
        // transactions (OnceLock first-write-wins, not per-call).
        assert_eq!(smoke.r#echo("again").unwrap(), "again");
    }

    handle.join().expect("server thread");
}

#[test]
fn generated_stub_over_mem() {
    let (a, b) = MemTransport::pair();
    run(Box::new(a), Box::new(b));
}

#[test]
fn generated_stub_over_unix_socketpair() {
    let (a, b) = UnixTransport::pair().expect("socketpair");
    run(Box::new(a), Box::new(b));
}

#[cfg(feature = "rpc-tcp-debug")]
#[test]
fn generated_stub_over_tcp_debug() {
    use rsbinder::rpc::transport::TcpDebugTransport;
    let (a, b) = TcpDebugTransport::pair_loopback().expect("loopback pair");
    run(Box::new(a), Box::new(b));
}

// ---- AC-6.4: bad binder is graceful, never a panic ------------------

/// A foreign native binder of an unrelated descriptor — neither an
/// `IRpcSmoke` proxy nor `BnRpcSmoke`.
struct BnOther;
impl Remotable for BnOther {
    fn descriptor() -> &'static str {
        "rpcsmoke.Other"
    }
    fn on_transact(
        &self,
        _code: rsbinder::TransactionCode,
        _reader: &mut rsbinder::Parcel,
        _reply: &mut rsbinder::Parcel,
    ) -> rsbinder::Result<()> {
        Err(rsbinder::StatusCode::UnknownTransaction)
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> rsbinder::Result<()> {
        Ok(())
    }
}

/// AC-6.4: the generator's old `as_proxy().unwrap()` (which panicked
/// on a non-`ProxyHandle` binder) is gone — golden-proven it now
/// emits `as_remote().ok_or(StatusCode::BadType)?`. At the boundary,
/// `try_from`:
///   * a local same-interface native binder → resolves via the
///     `from_binder` → native fallback (gate is now
///     `as_remote().is_some()`), **no panic**;
///   * a foreign-descriptor binder → `Err`, **no panic** (the old
///     `.unwrap()` would have aborted the process).
#[test]
fn bad_binder_is_graceful_not_panic() {
    // Same-interface local native binder: not a proxy, so the
    // `as_remote()` gate is `None` and the native fallback resolves
    // it. The point is that this does not panic.
    let local = <dyn IRpcSmoke as FromIBinder>::try_from(root())
        .expect("local native IRpcSmoke resolves via the native fallback");
    assert_eq!(local.r#echo("local").unwrap(), "local");

    // Foreign-descriptor binder: from_binder rejects (descriptor
    // mismatch + not remote) and the native cast fails too → Err,
    // never a panic.
    let foreign = Interface::as_binder(&Binder::new(BnOther));
    assert!(
        <dyn IRpcSmoke as FromIBinder>::try_from(foreign).is_err(),
        "AC-6.4: a wrong binder must be a graceful Err, not a panic"
    );
}
