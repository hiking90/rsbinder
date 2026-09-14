// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `BinderAsyncRuntime` adapter regression tests (plan 11 Phase A, AC-11.1).
//!
//! `Bn*::new_async_binder` hands back a **synchronous** `Strong<dyn IFoo>`
//! whose generated wrapper implements every method as
//! `rt.block_on(inner.method())`. Whether that call works depends on which
//! thread makes it, and the failing positions are ordinary ones — a
//! gateway calling its own service, a service's own start-up code, a test.
//!
//! Every case here uses a **local** binder, so none of it needs a kernel
//! device or the RPC transport: the same `TokioRuntime` adapter is on the
//! path either way. The scenario names match the probe under
//! `plans/11-async-probe/`.
//!
//! What is deliberately absent: a current-thread runtime whose owning
//! thread is blocked *outside* `Runtime::block_on` (probe case B). Its
//! timer and IO drivers stop, so the symptom is a call that never returns
//! rather than an error, and no adapter change can fix it — it is
//! documented in `TokioRuntime` and in the book instead.

#![allow(non_snake_case)]

use std::thread;
use std::time::Duration;

use async_trait::async_trait;

use rsbinder::{Interface, Strong, Tokio, TokioRuntime};

include!(concat!(env!("OUT_DIR"), "/async_rt.rs"));

use asyncrt::IAsyncRt::{BnAsyncRt, IAsyncRt, IAsyncRtAsync, IAsyncRtAsyncService};

struct Svc;
impl Interface for Svc {}

#[async_trait]
impl IAsyncRtAsyncService for Svc {
    async fn r#echo(&self, msg: &str) -> rsbinder::BinderResult<String> {
        // A timer, not `yield_now`: a runtime whose driver is not being
        // polled still completes a yield, so only a sleep separates
        // "driven" from "merely entered".
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(format!("echo:{msg}"))
    }
}

fn service(handle: tokio::runtime::Handle) -> Strong<dyn IAsyncRt> {
    BnAsyncRt::new_async_binder(Svc, TokioRuntime(handle))
}

fn multi_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// A — the sync handle called from inside `Runtime::block_on` on a
/// multi-threaded runtime. Panicked with "Cannot start a runtime from
/// within a runtime" before the adapter released the core.
#[test]
fn sync_handle_from_multi_thread_block_on() {
    let rt = multi_thread();
    rt.block_on(async {
        let svc = service(tokio::runtime::Handle::current());
        assert_eq!(svc.echo("hi").expect("echo"), "echo:hi");
    });
}

/// A' — same call from a spawned task, i.e. a real worker thread. This is
/// the one that actually hands the core off to the blocking pool.
#[test]
fn sync_handle_from_spawned_task() {
    let rt = multi_thread();
    rt.block_on(async {
        let svc = service(tokio::runtime::Handle::current());
        tokio::spawn(async move { svc.echo("task").expect("echo") })
            .await
            .map(|got| assert_eq!(got, "echo:task"))
            .expect("join")
    });
}

/// A2 — the same local service reached as an async handle. `into_async`
/// dispatches straight to the async service and never enters `block_on`,
/// which is why it is the advice for a path that calls repeatedly.
#[test]
fn async_handle_from_multi_thread() {
    let rt = multi_thread();
    rt.block_on(async {
        let svc: Strong<dyn IAsyncRtAsync<Tokio>> =
            service(tokio::runtime::Handle::current()).into_async();
        assert_eq!(svc.echo("local").await.expect("echo"), "echo:local");
    });
}

/// B2 — a current-thread runtime whose owning thread is parked inside
/// `Runtime::block_on`, called from one of its blocking-pool threads.
/// `Handle::block_on` is legal there and the parked owner keeps the timer
/// driver running, so the adapter must not reject the flavor.
#[test]
fn sync_handle_from_current_thread_blocking_pool() {
    let rt = current_thread();
    rt.block_on(async {
        let svc = service(tokio::runtime::Handle::current());
        tokio::task::spawn_blocking(move || svc.echo("pool").expect("echo"))
            .await
            .map(|got| assert_eq!(got, "echo:pool"))
            .expect("join")
    });
}

/// C — an ordinary thread outside the runtime: what a kernel binder looper
/// or an RPC session thread looks like. The adapter must leave this path
/// exactly as it was.
#[test]
fn sync_handle_from_outside_runtime() {
    let rt = multi_thread();
    let svc = service(rt.handle().clone());
    let got = thread::spawn(move || svc.echo("thread").expect("echo"))
        .join()
        .expect("join");
    assert_eq!(got, "echo:thread");
}

/// C2 — a multi-threaded runtime's blocking-pool thread: where a nested
/// inbound transaction lands when the async client's `spawn_blocking` call
/// is itself serving one. Tokio treats the thread as outside the runtime,
/// so `block_in_place` runs inline.
#[test]
fn sync_handle_from_multi_thread_blocking_pool() {
    let rt = multi_thread();
    rt.block_on(async {
        let svc = service(tokio::runtime::Handle::current());
        tokio::task::spawn_blocking(move || svc.echo("pool").expect("echo"))
            .await
            .map(|got| assert_eq!(got, "echo:pool"))
            .expect("join")
    });
}

/// B3 — a current-thread runtime's own thread. `block_on` there is
/// re-entrant by construction and there is no core to release, so this
/// stays a panic; the `TokioRuntime` docs name it. Asserted so the
/// adapter is not quietly extended to swallow it.
#[test]
#[should_panic(expected = "Cannot start a runtime from within a runtime")]
fn sync_handle_from_current_thread_owner_panics() {
    let rt = current_thread();
    rt.block_on(async {
        let svc = service(tokio::runtime::Handle::current());
        let _ = svc.echo("owner");
    });
}
