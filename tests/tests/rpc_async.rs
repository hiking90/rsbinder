// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Async-over-RPC e2e (subplan 2-3 §7-2 "async adapter is follow-up").
//!
//! Closes the documented capability gap: the generated **async** `Bp*`
//! and the **async service** adapter (`new_async_binder`) were verified
//! only over *kernel* binder (AC-6.5), never over the RPC transport.
//!
//! There is **no new RPC production code** behind async-over-RPC — it is
//! exactly the same `spawn_blocking` bridge the kernel async path uses:
//!
//!   * **client**: the generated `IRpcSmokeAsync<Tokio>` calls
//!     `P::spawn(move || as_remote()?.submit_transact(..), ..)`. For
//!     `Tokio`, `submit_transact` (→ `RpcProxy` → blocking
//!     `client_transact`) runs on `tokio::task::spawn_blocking`; the
//!     reply parse is the async continuation.
//!   * **server**: `BnRpcSmoke::new_async_binder(impl …AsyncService, rt)`
//!     wraps each call in `rt.block_on(..)`, invoked from the blocking
//!     `serve_blocking` worker.
//!
//! So this binary proves the existing adapters interoperate over a real
//! `UnixTransport` (and `mem`), including the T1-1 / AC-3.2 invariant
//! under genuine async concurrency: many in-flight calls on **one
//! shared session** are serialized by the per-connection `conn_lock`
//! and never cross-deliver replies (the r34 wire has no correlation id).
//! True async *I/O* (a non-blocking `RpcTransport`, no blocking worker)
//! remains the separately-deferred §7-2 item — it is *not* exercised
//! here and is not required for this adapter to be correct.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`, so it never shares
//! a process with the kernel-binder unit tests (master §6). P6: each
//! test builds its own session pair → parallel-safe.

#![cfg(feature = "rpc")]
#![allow(non_snake_case)]

use std::thread;

use async_trait::async_trait;

use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{AddressSpace, RpcSession, RpcTransport};
use rsbinder::{FromIBinder, Interface, SIBinder, Strong, Tokio, TokioRuntime};

include!(concat!(env!("OUT_DIR"), "/rpc_smoke.rs"));

use rpcsmoke::IRpcSmoke::{BnRpcSmoke, IRpcSmoke, IRpcSmokeAsync, IRpcSmokeAsyncService};

// ---- async server impl (driven by the generated new_async_binder) ----

struct SmokeAsyncSvc;
impl Interface for SmokeAsyncSvc {}

#[async_trait]
impl IRpcSmokeAsyncService for SmokeAsyncSvc {
    async fn r#echo(&self, s: &str) -> rsbinder::status::Result<String> {
        // A real `.await` point on the server side: proves the
        // `rt.block_on` adapter drives a genuinely async handler over
        // the blocking RPC serve loop.
        tokio::task::yield_now().await;
        Ok(s.to_string())
    }
    async fn r#add(&self, a: i32, b: i32) -> rsbinder::status::Result<i32> {
        Ok(a + b)
    }
    async fn r#ping(&self) -> rsbinder::status::Result<()> {
        Ok(())
    }
}

/// Server root = an **async** service binder. `rt` is a multi-thread
/// runtime handle so `block_on` works from the (non-runtime) blocking
/// serve worker thread.
fn async_root(rt: TokioRuntime<tokio::runtime::Handle>) -> SIBinder {
    BnRpcSmoke::new_async_binder(SmokeAsyncSvc, rt).as_binder()
}

/// Drive the generated **async** `Bp*` over `client_t` against an async
/// service on `server_t`. The blocking session setup (`get_root` +
/// `try_from` + `into_async`) is done *before* entering the runtime; the
/// transacts themselves are `.await`ed.
fn run(server_t: Box<dyn RpcTransport>, client_t: Box<dyn RpcTransport>) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let handle = rt.handle().clone();

    let server = RpcSession::new(server_t, AddressSpace::Acceptor);
    server.set_root(async_root(TokioRuntime(handle.clone())));
    let server_for_thread = server.clone();
    let jh = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });

    // Blocking client bootstrap (one connection, one root).
    let client = RpcSession::new(client_t, AddressSpace::Initiator);
    let sib = client.get_root().expect("get_root");
    let smoke: Strong<dyn IRpcSmokeAsync<Tokio>> = <dyn IRpcSmoke as FromIBinder>::try_from(sib)
        .expect("generated BpRpcSmoke resolves from an RPC binder")
        .into_async::<Tokio>();

    rt.block_on(async move {
        // Sequential: generated async client (spawn_blocking) ↔ async
        // service (block_on). Exact values.
        assert_eq!(
            smoke.r#echo("hello async rpc").await.unwrap(),
            "hello async rpc"
        );
        assert_eq!(smoke.r#echo("").await.unwrap(), "");
        assert_eq!(smoke.r#add(2, 3).await.unwrap(), 5);
        assert_eq!(smoke.r#add(-7, 7).await.unwrap(), 0);
        // oneway emit path under async (returns immediately, no reply).
        smoke.r#ping().await.unwrap();

        // In-task concurrency: two awaited calls polled together. Both
        // are spawn_blocking-backed → they really do contend for the
        // per-connection `conn_lock`; the join must still return each
        // call's own correct reply (no interleaved frame / cross-deliver).
        let (e, a) = tokio::join!(smoke.r#echo("joined"), smoke.r#add(10, 20));
        assert_eq!(e.unwrap(), "joined");
        assert_eq!(a.unwrap(), 30);

        // Cross-task concurrency on **one shared session** (T1-1 /
        // AC-3.2 under async): N tasks, each its own clone of the async
        // proxy, distinct payloads. Each task must observe *its own*
        // echo back — a frame interleave or reply mis-route would make
        // at least one assertion fail.
        let mut set = tokio::task::JoinSet::new();
        for i in 0..16i32 {
            let s = smoke.clone();
            set.spawn(async move {
                let payload = format!("task-{i}");
                assert_eq!(s.r#echo(&payload).await.unwrap(), payload);
                assert_eq!(s.r#add(i, i).await.unwrap(), 2 * i);
            });
        }
        while let Some(r) = set.join_next().await {
            r.expect("async task panicked");
        }
    });

    drop(client);
    jh.join().expect("server thread");
}

#[test]
fn async_generated_stub_over_unix_socketpair() {
    let (a, b) = UnixTransport::pair().expect("socketpair");
    run(Box::new(a), Box::new(b));
}

#[test]
fn async_generated_stub_over_mem() {
    let (a, b) = MemTransport::pair();
    run(Box::new(a), Box::new(b));
}
