// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The **generated** `Bp*` stub, which reaches the transport through
//! `as_remote()`, driven over the RPC transport,
//! with the **generated server stub reused unmodified**. No
//! hand-written proxy (contrast `rpc_e2e.rs`): this is
//! the single-stub end state. The interface (`rpcsmoke.IRpcSmoke`) is
//! compiled by `build.rs` to `OUT_DIR/rpc_smoke.rs`.
//!
//! - an arbitrary AIDL interface round-trips over `mem`,
//!   `unix` (and `tcp_debug` when its feature is on) through the
//!   generated `BpRpcSmoke`, which writes its own descriptor as the
//!   interface token.
//! - a non-remote / wrong binder is graceful: `try_from` yields a value
//!   or `Err`, never a panic.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`, so it never
//! shares a process with the kernel-binder unit tests.
//! Each test builds its own session pair → parallel-safe.

#![cfg(feature = "rpc")]

use std::thread;

use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{AddressSpace, RpcSession, RpcTransport};
use rsbinder::{Binder, FromIBinder, Interface, Remotable, SIBinder, ServiceSpecificError};

include!(concat!(env!("OUT_DIR"), "/rpc_smoke.rs"));

use rpcsmoke::IRpcSmoke::{BnRpcSmoke, IRpcSmoke};

// ---- server impl (the generated stub is reused unmodified) ----------

struct SmokeSvc;
impl Interface for SmokeSvc {}
impl IRpcSmoke for SmokeSvc {
    fn r#echo(&self, s: &str) -> rsbinder::BinderResult<String> {
        Ok(s.to_string())
    }
    fn r#add(&self, a: i32, b: i32) -> rsbinder::BinderResult<i32> {
        Ok(a + b)
    }
    fn r#ping(&self) -> rsbinder::BinderResult<()> {
        Ok(())
    }
    fn r#fail(&self, code: i32) -> rsbinder::BinderResult<()> {
        // Built from the type, not the `i32`, so the typed-error path is on the wire (plan 10-4).
        match SmokeError::from_code(code) {
            Some(err) => Err(rsbinder::Status::service_specific(err, Some("typed"))),
            None => Err(rsbinder::Status::new_service_specific_error(code, None)),
        }
    }
}

rsbinder::declare_binder_enum! {
    SmokeError : [i32; 2] {
        REFUSED = 1,
        EXHAUSTED = 2,
    }
}
rsbinder::impl_service_specific_error!(SmokeError);

fn root() -> SIBinder {
    BnRpcSmoke::new_binder(SmokeSvc).as_binder()
}

// ---- generated stub e2e over each transport -------------------------

fn run(server_t: Box<dyn RpcTransport>, client_t: Box<dyn RpcTransport>) {
    let server = RpcSession::new(server_t, AddressSpace::Acceptor).expect("RpcSession::new");
    server.set_root(root()).expect("set_root");
    let server_for_thread = server.clone();
    let handle = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });

    {
        let client = RpcSession::new(client_t, AddressSpace::Initiator).expect("RpcSession::new");
        let sib = client.get_root().expect("get_root");

        // try_from → Proxy::from_binder makes no transaction; methods go through as_remote().
        let smoke = <dyn IRpcSmoke as FromIBinder>::try_from(sib)
            .expect("AC-6.3: generated BpRpcSmoke resolves from an RPC binder");

        // Generated round-trip: the stub's own descriptor is the interface token.
        assert_eq!(
            smoke.r#echo("hello generated rpc").unwrap(),
            "hello generated rpc"
        );
        assert_eq!(smoke.r#echo("").unwrap(), "");
        assert_eq!(smoke.r#add(2, 3).unwrap(), 5);
        assert_eq!(smoke.r#add(-7, 7).unwrap(), 0);
        // oneway emit path (FLAG_ONEWAY branch of the generator).
        smoke.r#ping().unwrap();
        // A second call on the same proxy.
        assert_eq!(smoke.r#echo("again").unwrap(), "again");

        // Plan 10-4 AC-4.1, RPC half: the typed error crosses RPC unchanged.
        let status = smoke.r#fail(2).expect_err("fail always fails");
        assert_eq!(
            status.exception_code(),
            rsbinder::ExceptionCode::ServiceSpecific
        );
        assert_eq!(
            status.service_error::<SmokeError>(),
            Some(SmokeError::EXHAUSTED)
        );
        assert_eq!(status.message(), Some("typed"));
        // An undeclared code stays an `i32` and reads as `None`, not as another variant.
        let unknown = smoke.r#fail(41).expect_err("fail always fails");
        assert_eq!(unknown.service_error::<SmokeError>(), None);
        assert_eq!(unknown.service_specific_error(), 41);
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

// ---- bad binder is graceful, never a panic --------------------------

/// A native binder of an unrelated descriptor: neither an `IRpcSmoke` proxy nor `BnRpcSmoke`.
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

/// A local same-interface binder resolves natively; a foreign one is `Err`; neither panics.
#[test]
fn bad_binder_is_graceful_not_panic() {
    // Not a proxy: the native fallback resolves it, without a panic.
    let local = <dyn IRpcSmoke as FromIBinder>::try_from(root())
        .expect("local native IRpcSmoke resolves via the native fallback");
    assert_eq!(local.r#echo("local").unwrap(), "local");

    // Not remote, and the native cast fails on type: `Err`, never a panic.
    let foreign = Interface::as_binder(&Binder::new(BnOther));
    assert!(
        <dyn IRpcSmoke as FromIBinder>::try_from(foreign).is_err(),
        "AC-6.4: a wrong binder must be a graceful Err, not a panic"
    );
}
