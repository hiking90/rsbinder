// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-9 AC-9.5/9.6, RPC half: a transaction observer sees each call
//! over an RPC session with the method name, one-way flag, caller identity
//! and transport, and a panic inside it leaves the call's result alone.
//!
//! The kernel half is `tests/scripts/run_observer_ac.sh` (`observer_probe`),
//! which prints lines in the same format, so the two halves are compared
//! field by field. It also covers what one serving thread cannot: an
//! observer that makes a binder call of its own.
//!
//! The observer is process-wide, so both transports run in one `#[test]`.

#![cfg(feature = "rpc")]

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rsbinder::observe::{set_observer, TransactionObserver, TxnContext};
use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{AddressSpace, RpcSession, RpcTransport};
use rsbinder::{FromIBinder, Interface, TransportCaps};

include!(concat!(env!("OUT_DIR"), "/trace_demo.rs"));

use tracedemo::ITraceDemo::{BnTraceDemo, ITraceDemo};

const DESCRIPTOR: &str = "tracedemo.ITraceDemo";

/// Same line format as `observer_probe`.
fn line(ctx: &TxnContext<'_>, result: &rsbinder::Result<()>, tag: bool) -> String {
    format!(
        "{} oneway={} uid={} pid={} transport={} result={result:?} tag={}",
        ctx.method.unwrap_or("?"),
        ctx.is_oneway,
        ctx.calling_uid,
        ctx.calling_pid,
        if ctx.transport == TransportCaps::KERNEL {
            "kernel"
        } else {
            "rpc"
        },
        if tag { "some" } else { "none" },
    )
}

struct Recorder {
    lines: Arc<Mutex<Vec<String>>>,
}

impl TransactionObserver for Recorder {
    fn on_transact(&self, ctx: &TxnContext<'_>) -> Option<Box<dyn Any + Send>> {
        if ctx.descriptor == DESCRIPTOR && ctx.method == Some("ping") {
            panic!("observer bug on ping");
        }
        Some(Box::new(()))
    }

    fn on_reply(
        &self,
        ctx: &TxnContext<'_>,
        tag: Option<Box<dyn Any + Send>>,
        result: &rsbinder::Result<()>,
        _elapsed: Duration,
    ) {
        if ctx.descriptor == DESCRIPTOR && ctx.method != Some("observed") {
            self.lines
                .lock()
                .unwrap()
                .push(line(ctx, result, tag.is_some()));
        }
    }
}

struct Svc {
    lines: Arc<Mutex<Vec<String>>>,
}

impl Interface for Svc {}

impl ITraceDemo for Svc {
    fn r#ping(&self) -> rsbinder::BinderResult<()> {
        Ok(())
    }
    fn r#add(&self, a: i32, b: i32) -> rsbinder::BinderResult<i32> {
        Ok(a + b)
    }
    fn r#notify(&self, _s: &str) -> rsbinder::BinderResult<()> {
        Ok(())
    }
    fn r#observed(&self) -> rsbinder::BinderResult<Vec<String>> {
        Ok(self.lines.lock().unwrap().clone())
    }
}

fn run(
    server_t: Box<dyn RpcTransport>,
    client_t: Box<dyn RpcTransport>,
    lines: &Arc<Mutex<Vec<String>>>,
) {
    lines.lock().unwrap().clear();
    let server = RpcSession::new(server_t, AddressSpace::Acceptor).expect("RpcSession::new");
    server
        .set_root(
            BnTraceDemo::new_binder(Svc {
                lines: lines.clone(),
            })
            .as_binder(),
        )
        .expect("set_root");
    let server_for_thread = server.clone();
    let handle = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });

    {
        let client = RpcSession::new(client_t, AddressSpace::Initiator).expect("RpcSession::new");
        let svc = <dyn ITraceDemo as FromIBinder>::try_from(client.get_root().expect("get_root"))
            .expect("ITraceDemo proxy");

        svc.ping()
            .expect("a panicking observer must not fail the call");
        assert_eq!(svc.add(2, 3).expect("add"), 5);
        svc.notify("x").expect("notify");

        // `notify` is one-way: poll until its reply callback has run.
        let mut seen = Vec::new();
        for _ in 0..100 {
            seen = svc.observed().expect("observed");
            if seen.len() == 3 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let uid = rustix::process::getuid().as_raw();
        let pid = std::process::id();
        let common = format!("uid={uid} pid={pid} transport=rpc result=Ok(())");
        assert_eq!(
            seen,
            [
                format!("ping oneway=false {common} tag=none"),
                format!("add oneway=false {common} tag=some"),
                format!("notify oneway=true {common} tag=some"),
            ]
        );
    }

    handle.join().expect("server thread");
}

#[test]
fn observer_sees_rpc_transactions() {
    let lines = Arc::new(Mutex::new(Vec::new()));
    set_observer(Some(Arc::new(Recorder {
        lines: lines.clone(),
    })));

    let (a, b) = MemTransport::pair();
    run(Box::new(a), Box::new(b), &lines);
    let (a, b) = UnixTransport::pair().expect("socketpair");
    run(Box::new(a), Box::new(b), &lines);

    set_observer(None);
}
