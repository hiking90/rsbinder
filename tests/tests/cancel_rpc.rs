// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-5 AC-5.1, RPC half: a cancellation transport crossing a real
//! session.
//!
//! The unit tests next to `rsbinder::cancel` cancel a signal in the
//! process that made it. What they cannot cover is the part that makes
//! this a binder mechanism at all: the transport goes out **as a binder
//! object in a reply**, comes back as a proxy in another address space,
//! and the `oneway cancel()` on that proxy has to reach the original
//! signal. The kernel half of the same claim is
//! `tests/scripts/run_cancel_ac.sh`, which needs two processes.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`; each test builds
//! its own session pair, so they are parallel-safe.

#![cfg(feature = "rpc")]
#![allow(non_snake_case)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::thread;

use rsbinder::cancel::CancellationSignal;
use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{AddressSpace, RpcSession, RpcTransport};
use rsbinder::{FromIBinder, Interface, SIBinder};

include!(concat!(env!("OUT_DIR"), "/cancel_demo.rs"));

use canceldemo::ICancelDemo::{BnCancelDemo, ICancelDemo};

/// One signal per job name, as a service handling concurrent operations
/// would keep.
#[derive(Default)]
struct DemoSvc {
    jobs: Mutex<HashMap<String, CancellationSignal>>,
}

impl Interface for DemoSvc {}

impl ICancelDemo for DemoSvc {
    fn r#startJob(&self, job: &str) -> rsbinder::BinderResult<SIBinder> {
        let signal = CancellationSignal::new();
        let transport = signal.create_transport();
        self.jobs
            .lock()
            .unwrap()
            .insert(job.to_string(), signal.clone());
        Ok(transport.as_binder())
    }

    fn r#wasCanceled(&self, job: &str) -> rsbinder::BinderResult<bool> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .get(job)
            .map(|signal| signal.is_canceled())
            .unwrap_or(false))
    }
}

fn run(server_t: Box<dyn RpcTransport>, client_t: Box<dyn RpcTransport>) {
    let server = RpcSession::new(server_t, AddressSpace::Acceptor).expect("RpcSession::new");
    server
        .set_root(BnCancelDemo::new_binder(DemoSvc::default()).as_binder())
        .expect("set_root");
    let server_for_thread = server.clone();
    let handle = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });

    {
        let client = RpcSession::new(client_t, AddressSpace::Initiator).expect("RpcSession::new");
        let demo = <dyn ICancelDemo as FromIBinder>::try_from(client.get_root().expect("get_root"))
            .expect("cast the root to ICancelDemo");

        let transport = demo.r#startJob("job-a").expect("startJob");
        assert!(
            !demo.r#wasCanceled("job-a").expect("wasCanceled"),
            "a fresh signal must not read as cancelled"
        );

        // The claim: this proxy is in the caller's address space, and
        // the `oneway cancel()` on it reaches the signal the service
        // still holds.
        rsbinder::cancel::cancel_remote(&transport).expect("cancel_remote");

        // `cancel` is oneway, so the reply to it says nothing about
        // delivery. What orders the two here is the session: one RPC
        // session serves its transactions in order, so by the time this
        // answers, the cancel has been handled. Kernel binder gives no
        // such order — the transport is a different node than the
        // service — which is why `run_cancel_ac.sh` polls instead.
        assert!(
            demo.r#wasCanceled("job-a").expect("wasCanceled"),
            "the service must see the cancel the caller sent"
        );

        // A second job on the same service is untouched — the signal is
        // per-operation, not per-service.
        let _ = demo.r#startJob("job-b").expect("startJob");
        assert!(!demo.r#wasCanceled("job-b").expect("wasCanceled"));
        assert!(demo.r#wasCanceled("job-a").expect("wasCanceled"));

        // Cancelling twice is not an error: `cancel` is idempotent, and
        // the transport is still a live binder.
        rsbinder::cancel::cancel_remote(&transport).expect("second cancel_remote");
        assert!(demo.r#wasCanceled("job-a").expect("wasCanceled"));
    }

    handle.join().expect("server thread");
}

#[test]
fn a_transport_cancels_across_a_mem_session() {
    let (a, b) = MemTransport::pair();
    run(Box::new(a), Box::new(b));
}

#[test]
fn a_transport_cancels_across_a_unix_session() {
    let (a, b) = UnixTransport::pair().expect("socketpair");
    run(Box::new(a), Box::new(b));
}

/// A binder that is not a cancellation transport is refused rather than
/// transacted against: `cancel_remote` casts first.
#[test]
fn cancel_remote_refuses_a_foreign_binder() {
    let demo = BnCancelDemo::new_binder(DemoSvc::default()).as_binder();
    assert_eq!(
        rsbinder::cancel::cancel_remote(&demo).err(),
        Some(rsbinder::StatusCode::BadType)
    );
}
