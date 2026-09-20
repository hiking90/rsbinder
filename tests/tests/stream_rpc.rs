// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-7 Phase B, RPC half: a stream that crosses a real session.
//!
//! The unit tests beside `rsbinder::stream` put both ends in one
//! process, where a call on the sink dispatches straight into the object
//! and no parcel is ever built. What they cannot cover is what makes
//! this a binder mechanism: the sink goes out **as a binder object in an
//! argument**, arrives in another address space as a proxy with no
//! interface on it, and the producer's `oneway onBatch` has to reach the
//! receiver through it — while the credit runs the other way.
//!
//! It also pins the reason streaming needs more of an RPC session than
//! an ordinary call does. The producer pushes from a thread of its own,
//! outside any handler, which a default one-connection session cannot
//! carry; `Sink::new` refuses such a session up front rather than
//! failing on the first batch.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`. Each test builds
//! its own server and socket path, so they are parallel-safe.

#![cfg(feature = "rpc")]
#![allow(non_snake_case)]

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rsbinder::rpc::{RpcClientConfig, RpcServer, RpcSession};
use rsbinder::stream::{Receiver, Sink, DEFAULT_CREDIT_WINDOW, DEFAULT_MAX_BATCH_BYTES};
use rsbinder::{
    BinderResult, ExceptionCode, FromIBinder, Interface, SIBinder, Status, Strong, TransportCaps,
};

include!(concat!(env!("OUT_DIR"), "/stream_demo.rs"));

use streamdemo::IStreamDemo::{BnStreamDemo, IStreamDemo};

#[derive(Default)]
struct DemoSvc {
    sent: Arc<AtomicI32>,
    finished: Arc<AtomicBool>,
    last_error: Arc<AtomicI32>,
}

impl Interface for DemoSvc {}

impl DemoSvc {
    /// Start the producer on a thread of its own. That is the whole
    /// point: the handler returns the source right away and the pushing
    /// happens outside any transaction.
    fn spawn(
        &self,
        sink: &SIBinder,
        count: i32,
        max_batch_bytes: usize,
        initial_credits: u32,
        delay: Duration,
        ending: Option<Status>,
    ) -> BinderResult<SIBinder> {
        let (mut producer, source) =
            Sink::<i32>::with_limits(sink, max_batch_bytes, initial_credits)?;
        let handle = source.as_binder();
        let sent = self.sent.clone();
        let finished = self.finished.clone();
        let last_error = self.last_error.clone();
        thread::spawn(move || {
            // Held so the source object outlives the stream even if the
            // consumer is slow to resolve it.
            let _source = source;
            let mut stopped_early = false;
            for item in 0..count {
                if let Err(e) = producer.send(&item) {
                    last_error.store(i32::from(e), Ordering::SeqCst);
                    stopped_early = true;
                    break;
                }
                sent.fetch_add(1, Ordering::SeqCst);
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
            }
            if !stopped_early {
                let ended = match ending {
                    Some(status) => producer.end_with(&status),
                    None => producer.end(),
                };
                if let Err(e) = ended {
                    last_error.store(i32::from(e), Ordering::SeqCst);
                }
            }
            finished.store(true, Ordering::SeqCst);
        });
        Ok(handle)
    }
}

impl IStreamDemo for DemoSvc {
    fn r#subscribe(
        &self,
        sink: &SIBinder,
        count: i32,
        max_batch_bytes: i32,
        initial_credits: i32,
        delay_micros: i32,
    ) -> BinderResult<SIBinder> {
        self.spawn(
            sink,
            count,
            max_batch_bytes as usize,
            initial_credits as u32,
            Duration::from_micros(delay_micros.max(0) as u64),
            None,
        )
    }

    fn r#subscribeFailing(
        &self,
        sink: &SIBinder,
        count: i32,
        code: i32,
        message: &str,
    ) -> BinderResult<SIBinder> {
        self.spawn(
            sink,
            count,
            DEFAULT_MAX_BATCH_BYTES,
            DEFAULT_CREDIT_WINDOW,
            Duration::ZERO,
            Some(Status::new_service_specific_error(
                code,
                Some(message.to_string()),
            )),
        )
    }

    fn r#sent(&self) -> BinderResult<i32> {
        Ok(self.sent.load(Ordering::SeqCst))
    }

    fn r#finished(&self) -> BinderResult<bool> {
        Ok(self.finished.load(Ordering::SeqCst))
    }

    fn r#lastError(&self) -> BinderResult<i32> {
        Ok(self.last_error.load(Ordering::SeqCst))
    }
}

/// A server on its own socket, and a client session with `incoming`
/// callback connections.
struct Fixture {
    _server: thread::JoinHandle<()>,
    client: RpcSession,
    demo: Strong<dyn IStreamDemo>,
    path: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.client.close_session();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn fixture(tag: &str, incoming: u32) -> Fixture {
    let mut path = std::env::temp_dir();
    path.push(format!("rsb_stream_{tag}_{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(2);
    server
        .set_root(BnStreamDemo::new_binder(DemoSvc::default()).as_binder())
        .expect("set_root");
    let server = server.run_background();

    // `setup_unix_server` has already bound the listener, so connecting
    // straight away cannot race the background thread.
    let client = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 2).incoming_connections(incoming),
    )
    .expect("connect");
    let demo = <dyn IStreamDemo as FromIBinder>::try_from(client.get_root().expect("get_root"))
        .expect("cast the root to IStreamDemo");

    Fixture {
        _server: server,
        client,
        demo,
        path,
    }
}

/// Plan 10-7 AC-7.1 over RPC: every item crosses, in order, while the
/// bytes in flight stay inside the granted window.
///
/// 64-byte batches hold 16 integers each, so 1000 items are 63 batches —
/// enough that the consumer has to keep granting credit for the stream
/// to finish at all.
#[test]
fn a_stream_of_a_thousand_items_crosses_a_session() {
    let f = fixture("ok", 1);
    assert!(
        f.client.caps().contains(TransportCaps::CALLBACKS),
        "the fixture asked for an incoming connection"
    );

    let (mut rx, sink_binder) = Receiver::<i32>::new();
    let source = f
        .demo
        .r#subscribe(&sink_binder, 1000, 64, DEFAULT_CREDIT_WINDOW as i32, 0)
        .expect("subscribe");
    // The source arrives as a proxy with no interface on it — the RPC
    // wire carries an address and no descriptor — so this is also the
    // hand-built `request` path.
    rx.attach_source(&source).expect("attach_source");

    let got: Vec<i32> = (&mut rx).map(|item| item.expect("item")).collect();
    assert_eq!(got, (0..1000).collect::<Vec<_>>());
    assert!(
        rx.end_status().expect("a terminator arrived").is_ok(),
        "a stream that ran out ends with no exception"
    );
}

/// The producer runs outside any handler, so a session that cannot carry
/// a call in that direction is refused at `Sink::new` rather than at the
/// first batch.
#[test]
fn a_session_without_callback_connections_refuses_the_stream() {
    let f = fixture("nocb", 0);
    assert!(
        !f.client.caps().contains(TransportCaps::CALLBACKS),
        "a founding connection alone never grants CALLBACKS"
    );

    let (_rx, sink_binder) = Receiver::<i32>::new();
    let refused = f
        .demo
        .r#subscribe(&sink_binder, 10, 64, DEFAULT_CREDIT_WINDOW as i32, 0)
        .expect_err("the service cannot push over this session");
    assert_eq!(
        refused.transaction_error(),
        rsbinder::StatusCode::InvalidOperation,
        "the refusal names the transport, not the request: {refused:?}"
    );
}

/// The failure a service would have returned, arriving after the method
/// that started the stream already succeeded — with its service-specific
/// code intact, which is why the terminator carries three fields.
#[test]
fn a_failing_stream_keeps_its_service_specific_code_across_the_wire() {
    let f = fixture("fail", 1);

    let (mut rx, sink_binder) = Receiver::<i32>::new();
    let source = f
        .demo
        .r#subscribeFailing(&sink_binder, 3, 77, "quota exhausted")
        .expect("subscribeFailing");
    rx.attach_source(&source).expect("attach_source");

    assert_eq!(rx.next().expect("item 0").expect("ok"), 0);
    assert_eq!(rx.next().expect("item 1").expect("ok"), 1);
    assert_eq!(rx.next().expect("item 2").expect("ok"), 2);

    let failure = rx.next().expect("the failure").expect_err("an error");
    assert_eq!(failure.exception_code(), ExceptionCode::ServiceSpecific);
    assert_eq!(failure.service_specific_error(), 77);
    assert_eq!(failure.message(), Some("quota exhausted"));
    assert!(rx.next().is_none(), "the failure is reported once");
}

/// Plan 10-7 AC-7.2 over RPC: with one batch of credit and nothing
/// draining, the producer stops after that batch and stays stopped.
#[test]
fn the_window_bounds_what_the_producer_sends_before_anyone_drains() {
    let f = fixture("window", 1);

    let (mut rx, sink_binder) = Receiver::<i32>::with_credit_window(1);
    // Four bytes to a batch is one integer to a batch, and one opening
    // credit, so exactly one item may leave before a grant.
    let source = f
        .demo
        .r#subscribe(&sink_binder, 100, 4, 1, 0)
        .expect("subscribe");

    // Deliberately before `attach_source`: nothing can grant credit yet.
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        f.demo.r#sent().expect("sent"),
        1,
        "the opening window is one batch, and nothing has granted more"
    );

    rx.attach_source(&source).expect("attach_source");
    let got: Vec<i32> = (&mut rx).map(|item| item.expect("item")).collect();
    assert_eq!(got, (0..100).collect::<Vec<_>>());
}

/// A session that ends releases a consumer blocked in `recv`, the way a
/// dead process does on kernel binder (`run_stream_ac.sh`).
///
/// Back-pressure means there is usually no call in flight to fail, so
/// nothing reports the loss on its own: the death link `attach_source`
/// puts on the source is what ends the wait.
#[test]
fn a_session_that_ends_releases_a_blocked_consumer() {
    let f = fixture("dead", 1);

    let (mut rx, sink_binder) = Receiver::<i32>::new();
    // Paced at 5 ms an item with credit to spare, so when the session
    // goes the consumer is waiting and the producer is not parked.
    let source = f
        .demo
        .r#subscribe(&sink_binder, i32::MAX, 4, 1_000_000, 5_000)
        .expect("subscribe");
    rx.attach_source(&source).expect("attach_source");
    assert!(rx.next().expect("item 0").is_ok());

    f.client.close_session();

    // Bounded rather than `recv`: without the death link this waits for
    // a batch that cannot come, and a test that hangs reports nothing.
    let failure = loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            // Batches already queued come out first.
            Ok(Some(_)) => continue,
            Ok(None) if rx.is_finished() => {
                panic!("the stream ended clean, which the producer never did")
            }
            Ok(None) => panic!("the lost session left the consumer waiting"),
            Err(status) => break status,
        }
    };
    assert_eq!(
        failure.transaction_error(),
        rsbinder::StatusCode::DeadObject,
        "a lost peer is reported as such: {failure:?}"
    );
}

/// A consumer that walks away releases the producer instead of leaving
/// it parked on a credit that will never come.
#[test]
fn dropping_the_receiver_stops_the_producer() {
    let f = fixture("drop", 1);

    let (mut rx, sink_binder) = Receiver::<i32>::with_credit_window(1);
    let source = f
        .demo
        .r#subscribe(&sink_binder, 1_000_000, 4, 1, 0)
        .expect("subscribe");
    rx.attach_source(&source).expect("attach_source");
    assert_eq!(rx.next().expect("item 0").expect("ok"), 0);

    // Parked, and nothing left to grant: with the receiver still here
    // this is where it stays.
    thread::sleep(Duration::from_millis(200));
    assert!(
        !f.demo.r#finished().expect("finished"),
        "the producer must still be waiting for credit"
    );
    let stopped = f.demo.r#sent().expect("sent");
    assert!(
        stopped < 1_000_000,
        "the producer must be far short of the whole stream"
    );

    // What releases it is the cancel the drop sends. Polled rather than
    // slept on: the cancel and the producer's wake are on two threads.
    drop(rx);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !f.demo.r#finished().expect("finished") {
        assert!(
            std::time::Instant::now() < deadline,
            "dropping the receiver must release the producer"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        f.demo.r#sent().expect("sent"),
        stopped,
        "a cancelled producer sends nothing more"
    );
}
