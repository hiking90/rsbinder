// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-16 Phase A — `@EnforcePermission` over RPC fail-closes.
//!
//! An `@EnforcePermission` interface served over the RPC transport must
//! deny **every guarded method** with `EX_SECURITY`, never silently
//! grant. The bypass it closes: on the RPC dispatch path the kernel
//! thread-local calling context is never populated, so
//! `get_calling_uid()` reads `0` (= root), and Android's
//! `PermissionManagerService` *unconditionally grants root* — turning a
//! guarded method into a grant to any anonymous RPC peer. The deny is
//! transport-driven (`reader.is_for_rpc()` in
//! `permission_controller::check_permission`), so it fires **before** any
//! uid read or PMS lookup, independent of process shape and of any future
//! uid wiring (Phase B).
//!
//! Hermetic note on "both process shapes" (plan A.4): the pure-RPC shape
//! (`ProcessState` uninitialized) and the hybrid shape (kernel binder up
//! so PMS is reachable) take the **same** code path here — the deny
//! short-circuits ahead of PMS, so it is PMS-independent by construction.
//! This test runs the pure-RPC shape (no kernel binder on macOS/hermetic);
//! the deny's PMS-independence is what makes the hybrid case equivalent.
//! The un-annotated `echo` round-trips, proving the deny is scoped to the
//! guarded arms, not the whole interface.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`, so it never shares a
//! process with the kernel-binder unit tests. Each test builds its own
//! session pair → parallel-safe.

#![cfg(feature = "rpc")]
#![allow(non_snake_case)]

use std::thread;

use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{AddressSpace, RpcSession, RpcTransport};
use rsbinder::{ExceptionCode, Interface, SIBinder};

include!(concat!(env!("OUT_DIR"), "/rpc_perm_guard.rs"));

use rpcperm::IRpcPermGuard::{BnRpcPermGuard, IRpcPermGuard};

// ---- server impl: every guarded body would return `true` if reached ---
//
// The bodies intentionally return `Ok(true)` / echo the input — so if the
// `@EnforcePermission` deny did NOT fire, the client would observe `true`
// / the echoed string instead of `EX_SECURITY`. That makes a leaked check
// a hard failure, not a silent pass.

struct GuardSvc;
impl Interface for GuardSvc {}
impl IRpcPermGuard for GuardSvc {
    fn r#doSingle(&self) -> rsbinder::status::Result<bool> {
        Ok(true)
    }
    fn r#doAllOf(&self) -> rsbinder::status::Result<bool> {
        Ok(true)
    }
    fn r#doAnyOf(&self) -> rsbinder::status::Result<bool> {
        Ok(true)
    }
    fn r#echo(&self, message: &str) -> rsbinder::status::Result<String> {
        Ok(message.to_string())
    }
}

fn root() -> SIBinder {
    BnRpcPermGuard::new_binder(GuardSvc).as_binder()
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
        let guard = <dyn IRpcPermGuard as rsbinder::FromIBinder>::try_from(sib)
            .expect("generated BpRpcPermGuard resolves from an RPC binder");

        // Every guarded form denies with EX_SECURITY over RPC.
        for (name, res) in [
            ("doSingle", guard.r#doSingle()),
            ("doAllOf", guard.r#doAllOf()),
            ("doAnyOf", guard.r#doAnyOf()),
        ] {
            let err = res.expect_err(&format!(
                "{name}: @EnforcePermission must deny over RPC, not grant"
            ));
            assert_eq!(
                err.exception_code(),
                ExceptionCode::Security,
                "{name}: deny must be EX_SECURITY (got {err:?})"
            );
        }

        // The un-annotated method is unaffected — deny is per-arm, not
        // per-interface.
        assert_eq!(guard.r#echo("hi").unwrap(), "hi");
    }

    handle.join().expect("server thread");
}

#[test]
fn enforce_permission_denies_over_mem() {
    let (a, b) = MemTransport::pair();
    run(Box::new(a), Box::new(b));
}

#[test]
fn enforce_permission_denies_over_unix_socketpair() {
    let (a, b) = UnixTransport::pair().expect("socketpair");
    run(Box::new(a), Box::new(b));
}
