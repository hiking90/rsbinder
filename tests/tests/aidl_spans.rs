// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-9 AC-9.8: with the `tracing` feature, a proxy generated with
//! `Builder::trace(true)` opens `AIDL::rust::<descriptor>::<method>::client`
//! and `TracingObserver` opens `…::server`, over an RPC session.
//!
//! Checked beyond the names: the server span is entered while the handler
//! runs (a span the handler opens is its child), the client span's parent is
//! the caller's span for the sync and the async proxy alike (the async one is
//! entered on a pool thread), and every entered span is exited.
//!
//! The subscriber is written out here rather than taken from
//! `tracing-subscriber`: it only has to record parents and enter/exit
//! counts. It is process-global, so everything runs in one `#[test]`.

#![cfg(all(feature = "rpc", feature = "tracing"))]

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use rsbinder::observe::{set_observer, TracingObserver};
use rsbinder::rpc::transport::MemTransport;
use rsbinder::rpc::{AddressSpace, RpcSession};
use rsbinder::{FromIBinder, Interface, Strong, Tokio};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

include!(concat!(env!("OUT_DIR"), "/trace_demo.rs"));

use tracedemo::ITraceDemo::{BnTraceDemo, ITraceDemo, ITraceDemoAsync};

#[derive(Debug, Clone, Default)]
struct SpanRec {
    /// The `name` field for `aidl` spans, the span name otherwise.
    label: String,
    parent: Option<u64>,
    enters: u32,
    exits: u32,
}

#[derive(Default)]
struct Recorder {
    next: AtomicU64,
    spans: Mutex<HashMap<u64, SpanRec>>,
}

thread_local! {
    static STACK: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct NameField(Option<String>);

impl Visit for NameField {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "name" {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

#[derive(Clone)]
struct Sub(Arc<Recorder>);

impl Subscriber for Sub {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        let id = self.0.next.fetch_add(1, Ordering::Relaxed) + 1;
        let mut name = NameField(None);
        attrs.record(&mut name);
        let parent = if let Some(parent) = attrs.parent() {
            Some(parent.into_u64())
        } else if attrs.is_contextual() {
            STACK.with(|s| s.borrow().last().copied())
        } else {
            None
        };
        let label = name.0.unwrap_or_else(|| attrs.metadata().name().to_owned());
        self.0.spans.lock().unwrap().insert(
            id,
            SpanRec {
                label,
                parent,
                ..Default::default()
            },
        );
        Id::from_u64(id)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, _: &Event<'_>) {}

    fn enter(&self, id: &Id) {
        STACK.with(|s| s.borrow_mut().push(id.into_u64()));
        self.0
            .spans
            .lock()
            .unwrap()
            .get_mut(&id.into_u64())
            .unwrap()
            .enters += 1;
    }

    fn exit(&self, id: &Id) {
        STACK.with(|s| {
            let popped = s.borrow_mut().pop();
            assert_eq!(popped, Some(id.into_u64()), "exit out of order");
        });
        self.0
            .spans
            .lock()
            .unwrap()
            .get_mut(&id.into_u64())
            .unwrap()
            .exits += 1;
    }
}

impl Recorder {
    fn find(&self, label: &str) -> Vec<(u64, SpanRec)> {
        let mut found: Vec<_> = self
            .spans
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, rec)| rec.label == label)
            .map(|(id, rec)| (*id, rec.clone()))
            .collect();
        found.sort_by_key(|(id, _)| *id);
        found
    }

    fn one(&self, label: &str) -> (u64, SpanRec) {
        let found = self.find(label);
        assert_eq!(found.len(), 1, "{label}: {found:?}\nall: {:?}", self.spans);
        found.into_iter().next().unwrap()
    }
}

struct Svc;

impl Interface for Svc {}

impl ITraceDemo for Svc {
    fn r#ping(&self) -> rsbinder::BinderResult<()> {
        Ok(())
    }
    fn r#add(&self, a: i32, b: i32) -> rsbinder::BinderResult<i32> {
        tracing::trace_span!("handler").in_scope(|| Ok(a + b))
    }
    fn r#notify(&self, _s: &str) -> rsbinder::BinderResult<()> {
        Ok(())
    }
    fn r#observed(&self) -> rsbinder::BinderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

const PREFIX: &str = "AIDL::rust::tracedemo.ITraceDemo::";

#[test]
fn client_and_server_spans_carry_aosp_names() {
    let recorder = Arc::new(Recorder::default());
    tracing::subscriber::set_global_default(Sub(recorder.clone())).expect("global subscriber");
    set_observer(Some(Arc::new(TracingObserver)));

    let (a, b) = MemTransport::pair();
    let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("RpcSession::new");
    server
        .set_root(BnTraceDemo::new_binder(Svc).as_binder())
        .expect("set_root");
    let server_for_thread = server.clone();
    let serving = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });

    {
        let client =
            RpcSession::new(Box::new(b), AddressSpace::Initiator).expect("RpcSession::new");
        let root = client.get_root().expect("get_root");
        let svc = <dyn ITraceDemo as FromIBinder>::try_from(root.clone()).expect("proxy");

        tracing::trace_span!("caller").in_scope(|| {
            assert_eq!(svc.add(2, 3).expect("add"), 5);
        });
        assert_eq!(svc.getInterfaceVersion().expect("version"), 1);
        svc.notify("x").expect("notify");

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let async_svc: Strong<dyn ITraceDemoAsync<Tokio>> =
            <dyn ITraceDemo as FromIBinder>::try_from(root)
                .expect("proxy")
                .into_async::<Tokio>();
        // Created with the future in `async_caller`; entered later on a blocking-pool thread.
        rt.block_on(async {
            let call = tracing::trace_span!("async_caller").in_scope(|| async_svc.ping());
            call.await
        })
        .expect("async ping");
    }
    // One-way `notify` finished before `ping`'s reply: the session serves in arrival order.
    serving.join().expect("server thread");
    set_observer(None);

    let (caller, _) = recorder.one("caller");
    let (add_client, rec) = recorder.one(&format!("{PREFIX}add::client"));
    assert_eq!(
        rec.parent,
        Some(caller),
        "client span's parent is the caller's"
    );
    let (add_server, rec) = recorder.one(&format!("{PREFIX}add::server"));
    assert_ne!(
        rec.parent,
        Some(add_client),
        "no context crosses the wire yet (plan 10-22)"
    );
    let (_, handler) = recorder.one("handler");
    assert_eq!(
        handler.parent,
        Some(add_server),
        "the server span is entered while the handler runs"
    );

    for side in ["client", "server"] {
        recorder.one(&format!("{PREFIX}getInterfaceVersion::{side}"));
        recorder.one(&format!("{PREFIX}notify::{side}"));
    }
    let (async_caller, _) = recorder.one("async_caller");
    let (_, rec) = recorder.one(&format!("{PREFIX}ping::client"));
    assert_eq!(rec.parent, Some(async_caller));
    recorder.one(&format!("{PREFIX}ping::server"));

    for (id, rec) in recorder.spans.lock().unwrap().iter() {
        assert!(rec.enters >= 1, "span {id} never entered: {rec:?}");
        assert_eq!(rec.enters, rec.exits, "span {id}: {rec:?}");
    }
}
