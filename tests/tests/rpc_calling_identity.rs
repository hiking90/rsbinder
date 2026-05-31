// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-16 Phase B — `get_calling_uid()` / `get_calling_pid()` /
//! `is_handling_transaction()` over the RPC transport.
//!
//! A Unix-domain (and the in-memory) RPC connection carries a
//! kernel-vouched `PeerIdentity::Local { uid, pid }`; the dispatch path
//! stamps it into the RPC calling context for the handler's duration. A
//! handler served over RPC therefore observes the connecting peer's
//! identity — proving a hand-written uid ACL runs transport-agnostically
//! on kernel binder *and* Unix RPC. For a same-process hermetic transport
//! (`mem` / `socketpair`) the peer *is* this process, so the observed uid
//! is `getuid()` and pid is `process::id()` — exactly what a cross-process
//! peer ACL would compare against, just with a known value.
//!
//! Granularity caveat (documented): the identity is **connection-level**,
//! not per-method — an RPC connection is opened once by one peer process.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`. Each test builds its
//! own session pair → parallel-safe.

#![cfg(feature = "rpc")]
#![allow(non_snake_case)]

use std::thread;

use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{AddressSpace, RpcSession, RpcTransport};
use rsbinder::{Interface, SIBinder};

include!(concat!(env!("OUT_DIR"), "/rpc_caller.rs"));

use rpccaller::IRpcCaller::{BnRpcCaller, IRpcCaller};

struct CallerSvc;
impl Interface for CallerSvc {}
impl IRpcCaller for CallerSvc {
    fn r#callingUid(&self) -> rsbinder::status::Result<i64> {
        Ok(rsbinder::get_calling_uid() as i64)
    }
    fn r#callingPid(&self) -> rsbinder::status::Result<i64> {
        Ok(rsbinder::get_calling_pid() as i64)
    }
    fn r#handlingTransaction(&self) -> rsbinder::status::Result<bool> {
        Ok(rsbinder::is_handling_transaction())
    }
    fn r#callerKind(&self) -> rsbinder::status::Result<String> {
        use rsbinder::rpc::PeerIdentity;
        use rsbinder::Caller;
        Ok(match rsbinder::calling_caller() {
            Some(Caller::Rpc(PeerIdentity::Local { uid, .. })) => format!("rpc-local:{uid}"),
            Some(Caller::Kernel { .. }) => "kernel".to_string(),
            // `Caller` / `PeerIdentity` are `#[non_exhaustive]`: a uid-less
            // RPC transport (or a future variant) lands here.
            Some(_) => "rpc-other".to_string(),
            None => "none".to_string(),
        })
    }
}

fn root() -> SIBinder {
    BnRpcCaller::new_binder(CallerSvc).as_binder()
}

fn run(server_t: Box<dyn RpcTransport>, client_t: Box<dyn RpcTransport>) {
    let server = RpcSession::new(server_t, AddressSpace::Acceptor).expect("RpcSession::new");
    server.set_root(root());
    let server_for_thread = server.clone();
    let handle = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });

    {
        let client = RpcSession::new(client_t, AddressSpace::Initiator).expect("RpcSession::new");
        let sib = client.get_root().expect("get_root");
        let caller = <dyn IRpcCaller as rsbinder::FromIBinder>::try_from(sib)
            .expect("generated BpRpcCaller resolves from an RPC binder");

        // The hermetic transports report this process as the peer, so the
        // handler observes our own uid/pid — never the `0` (= root) that a
        // pre-Phase-B (unpopulated) RPC dispatch would have surfaced.
        let expected_uid = rustix::process::getuid().as_raw() as i64;
        assert_eq!(
            caller.r#callingUid().unwrap(),
            expected_uid,
            "RPC handler must observe the peer uid (here: this process)"
        );
        // The pre-Phase-B bypass surfaced `0` (root). Guard against it only
        // when we are not actually running as root, so the test is not
        // self-contradictory under a root runner (where expected_uid == 0).
        if expected_uid != 0 {
            assert_ne!(
                caller.r#callingUid().unwrap(),
                0,
                "uid must be the real peer uid, not the pre-Phase-B root bypass"
            );
        }
        assert_eq!(
            caller.r#callingPid().unwrap(),
            std::process::id() as i64,
            "RPC handler must observe the peer pid"
        );
        assert!(
            caller.r#handlingTransaction().unwrap(),
            "is_handling_transaction() must be true inside an RPC handler"
        );
        // Phase C: `calling_caller()` exposes the full transport-tagged
        // peer — here a Unix RPC local peer with this process's uid.
        assert_eq!(
            caller.r#callerKind().unwrap(),
            format!("rpc-local:{}", rustix::process::getuid().as_raw()),
            "calling_caller() must surface Caller::Rpc(PeerIdentity::Local) over Unix RPC"
        );

        // Outside any handler the calling context is clear.
        assert_eq!(rsbinder::get_calling_uid(), 0);
        assert!(!rsbinder::is_handling_transaction());
    }

    handle.join().expect("server thread");
}

#[test]
fn calling_identity_over_mem() {
    let (a, b) = MemTransport::pair();
    run(Box::new(a), Box::new(b));
}

#[test]
fn calling_identity_over_unix_socketpair() {
    let (a, b) = UnixTransport::pair().expect("socketpair");
    run(Box::new(a), Box::new(b));
}
