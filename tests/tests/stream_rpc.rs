// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-7 Phase B, RPC half: a stream that crosses a real session.
//!
//! The unit tests beside `rsbinder::stream` put both ends in one
//! process, where a call on the sink dispatches straight into the object
//! and no parcel is ever built. What they cannot cover is what makes
//! this a binder mechanism: the endpoint goes out **as a parcelable in
//! an argument**, its sink arrives in another address space as a proxy
//! with no interface on it, and the producer's `oneway onBatch` has to
//! reach the receiver through it — while the source the producer
//! introduces itself with travels the same way, and the credit granted
//! on it runs back.
//!
//! It also pins the reason streaming needs more of an RPC session than
//! an ordinary call does. The producer pushes from a thread of its own,
//! outside any handler, which a default one-connection session cannot
//! carry; `Sink::open` refuses such a session up front rather than
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
use rsbinder::stream::{Receiver, ReceiverPolicy, Sink, SinkPolicy, StreamEndpoint};
use rsbinder::{
    BinderResult, ExceptionCode, FromIBinder, Interface, SIBinder, Status, Strong, TransportCaps,
};

include!(concat!(env!("OUT_DIR"), "/stream_demo.rs"));

use streamdemo::IStreamDemo::{BnStreamDemo, IStreamDemo};

/// The producer's default opening window, as the `int` the fixture
/// carries it in.
fn default_credits() -> i32 {
    SinkPolicy::default().initial_credits as i32
}

#[derive(Default)]
struct DemoSvc {
    sent: Arc<AtomicI32>,
    finished: Arc<AtomicBool>,
    last_error: Arc<AtomicI32>,
    uploaded: Arc<AtomicI32>,
    upload_ordered: Arc<AtomicBool>,
    upload_finished: Arc<AtomicBool>,
    upload_error: Arc<AtomicI32>,
}

impl Interface for DemoSvc {}

/// Where `subscribeAsync` runs its producers. Process-wide and never
/// dropped: a runtime dropped with a blocking task in flight waits for it.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .expect("runtime")
    })
}

fn sink_policy(max_batch_bytes: i32, initial_credits: i32) -> SinkPolicy {
    SinkPolicy {
        max_batch_bytes: max_batch_bytes as usize,
        initial_credits: initial_credits as u32,
        ..SinkPolicy::default()
    }
}

impl DemoSvc {
    /// Start the producer on a thread of its own. That is the whole
    /// point: the handler returns right away and the pushing happens
    /// outside any transaction.
    fn spawn(
        &self,
        endpoint: &StreamEndpoint,
        count: i32,
        policy: &SinkPolicy,
        delay: Duration,
        ending: Option<Status>,
    ) -> BinderResult<()> {
        let mut producer = Sink::<i32>::open_with(endpoint, policy)?;
        let sent = self.sent.clone();
        let finished = self.finished.clone();
        let last_error = self.last_error.clone();
        thread::spawn(move || {
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
        Ok(())
    }
}

impl IStreamDemo for DemoSvc {
    fn r#subscribe(
        &self,
        endpoint: &StreamEndpoint,
        count: i32,
        max_batch_bytes: i32,
        initial_credits: i32,
        delay_micros: i32,
    ) -> BinderResult<()> {
        self.spawn(
            endpoint,
            count,
            &sink_policy(max_batch_bytes, initial_credits),
            Duration::from_micros(delay_micros.max(0) as u64),
            None,
        )
    }

    fn r#subscribeAsync(
        &self,
        endpoint: &StreamEndpoint,
        count: i32,
        max_batch_bytes: i32,
        initial_credits: i32,
    ) -> BinderResult<()> {
        let mut producer =
            Sink::<i32>::open_with(endpoint, &sink_policy(max_batch_bytes, initial_credits))?;
        let sent = self.sent.clone();
        let finished = self.finished.clone();
        let last_error = self.last_error.clone();
        runtime().spawn(async move {
            let outcome = async {
                for item in 0..count {
                    producer.send_async(&item).await?;
                    sent.fetch_add(1, Ordering::SeqCst);
                }
                producer.end_async().await
            }
            .await;
            if let Err(e) = outcome {
                last_error.store(i32::from(e), Ordering::SeqCst);
            }
            finished.store(true, Ordering::SeqCst);
        });
        Ok(())
    }

    fn r#subscribeFailing(
        &self,
        endpoint: &StreamEndpoint,
        count: i32,
        code: i32,
        message: &str,
    ) -> BinderResult<()> {
        self.spawn(
            endpoint,
            count,
            &SinkPolicy::default(),
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

    // The upload direction is exercised by the kernel half
    // (`stream_probe`); this implementation keeps the fixture whole.
    fn r#upload(&self, producer: &SIBinder, ring_bytes: i32) -> BinderResult<StreamEndpoint> {
        let (mut rx, endpoint) = Receiver::<i32>::with_policy(
            producer,
            &ReceiverPolicy {
                ring_bytes: ring_bytes.max(0) as usize,
                ..ReceiverPolicy::default()
            },
        )?;
        let uploaded = self.uploaded.clone();
        let ordered = self.upload_ordered.clone();
        let finished = self.upload_finished.clone();
        let error = self.upload_error.clone();
        ordered.store(true, Ordering::SeqCst);
        thread::spawn(move || {
            let mut expected = 0i32;
            while let Some(item) = rx.recv() {
                match item {
                    Ok(item) => {
                        if item != expected {
                            ordered.store(false, Ordering::SeqCst);
                        }
                        expected += 1;
                        uploaded.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(status) => {
                        error.store(i32::from(status.transaction_error()), Ordering::SeqCst);
                        break;
                    }
                }
            }
            finished.store(true, Ordering::SeqCst);
        });
        Ok(endpoint)
    }

    fn r#uploaded(&self) -> BinderResult<i32> {
        Ok(self.uploaded.load(Ordering::SeqCst))
    }

    fn r#uploadOrdered(&self) -> BinderResult<bool> {
        Ok(self.upload_ordered.load(Ordering::SeqCst))
    }

    fn r#uploadFinished(&self) -> BinderResult<bool> {
        Ok(self.upload_finished.load(Ordering::SeqCst))
    }

    fn r#uploadError(&self) -> BinderResult<i32> {
        Ok(self.upload_error.load(Ordering::SeqCst))
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

impl Fixture {
    /// A receiver against the service's proxy — an RPC proxy, so the
    /// endpoint carries only the sink — with the grant window and the
    /// widest opening window set.
    fn receiver(&self, credit_window: u32, max_opening: u32) -> (Receiver<i32>, StreamEndpoint) {
        Receiver::<i32>::with_policy(
            &self.demo.as_binder(),
            &ReceiverPolicy {
                credit_window,
                max_opening,
                ..ReceiverPolicy::default()
            },
        )
        .expect("a receiver against a live session")
    }

    fn default_receiver(&self) -> (Receiver<i32>, StreamEndpoint) {
        Receiver::<i32>::new(&self.demo.as_binder()).expect("a receiver against a live session")
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

/// An RPC peer makes a sink-only endpoint: there is no shared memory to
/// carry a ring across a socket.
#[test]
fn an_rpc_peer_gets_a_sink_only_endpoint() {
    let f = fixture("endpoint", 1);
    let (_rx, endpoint) = f.default_receiver();
    assert!(endpoint.ring.is_none());
    assert!(endpoint.sink.is_some());
}

/// AC-7.1 over RPC: 1000 items in 63 batches cross in order inside the granted window.
#[test]
fn a_stream_of_a_thousand_items_crosses_a_session() {
    let f = fixture("ok", 1);
    assert!(
        f.client.caps().contains(TransportCaps::CALLBACKS),
        "the fixture asked for an incoming connection"
    );

    let (mut rx, endpoint) = f.default_receiver();
    f.demo
        .r#subscribe(&endpoint, 1000, 64, default_credits(), 0)
        .expect("subscribe");

    let got: Vec<i32> = (&mut rx).map(|item| item.expect("item")).collect();
    assert_eq!(got, (0..1000).collect::<Vec<_>>());
    assert!(
        rx.end_status().expect("a terminator arrived").is_ok(),
        "a stream that ran out ends with no exception"
    );
}

/// The opening window travels in `onStart`; the consumer holds the producer to it.
#[test]
fn a_window_wider_than_the_consumer_accepts_is_refused_across_a_session() {
    let f = fixture("wide", 1);

    let (mut rx, endpoint) = f.default_receiver();
    f.demo
        .r#subscribe(&endpoint, 1000, 64, default_credits() + 1, 0)
        .expect("`onStart` is oneway, so the service cannot hear the refusal");

    let refusal = rx
        .recv_timeout(Duration::from_secs(10))
        .expect_err("the stream must end before any item");
    assert_eq!(refusal.exception_code(), ExceptionCode::IllegalArgument);

    // The same service, a consumer made for that window: every item.
    let window = default_credits() as u32;
    let (mut rx, endpoint) = f.receiver(window, window + 1);
    f.demo
        .r#subscribe(&endpoint, 1000, 64, default_credits() + 1, 0)
        .expect("subscribe");
    // With a bound: a grant that did not cross the wire leaves both ends
    // alive and waiting, which is a hang rather than a failure.
    let mut got = 0;
    while got < 1000 {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Some(_)) => got += 1,
            other => panic!("stalled after {got} items: {other:?}"),
        }
    }
}

/// The producer as a task: one credit and four-byte batches, so 299 of 300 batches await a grant.
#[test]
fn an_async_producer_streams_across_a_session() {
    let f = fixture("async", 1);

    let (mut rx, endpoint) = f.receiver(2, default_credits() as u32);
    f.demo
        .r#subscribeAsync(&endpoint, 300, 4, 1)
        .expect("subscribeAsync");

    let mut got = Vec::new();
    while got.len() < 300 {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Some(item)) => got.push(item),
            other => panic!("stalled after {} items: {other:?}", got.len()),
        }
    }
    assert_eq!(got, (0..300).collect::<Vec<_>>());
    assert!(
        rx.recv_timeout(Duration::from_secs(10))
            .expect("a clean end")
            .is_none()
            && rx.is_finished(),
        "the terminator follows the last batch"
    );
    assert_eq!(f.demo.r#lastError().expect("lastError"), 0);
}

/// The producer runs outside any handler, so a session that cannot carry
/// a call in that direction is refused at `Sink::open` rather than at the
/// first batch.
#[test]
fn a_session_without_callback_connections_refuses_the_stream() {
    let f = fixture("nocb", 0);
    assert!(
        !f.client.caps().contains(TransportCaps::CALLBACKS),
        "a founding connection alone never grants CALLBACKS"
    );

    let (_rx, endpoint) = f.default_receiver();
    let refused = f
        .demo
        .r#subscribe(&endpoint, 10, 64, default_credits(), 0)
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

    let (mut rx, endpoint) = f.default_receiver();
    f.demo
        .r#subscribeFailing(&endpoint, 3, 77, "quota exhausted")
        .expect("subscribeFailing");

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

    let (mut rx, endpoint) = f.receiver(1, default_credits() as u32);
    // Four bytes to a batch is one integer to a batch, and one opening
    // credit, so exactly one item may leave before a grant.
    f.demo
        .r#subscribe(&endpoint, 100, 4, 1, 0)
        .expect("subscribe");

    // Nothing has been read yet, and credit is only granted for batches
    // the consumer has taken.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while f.demo.r#sent().expect("sent") < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "the first batch never left"
        );
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        f.demo.r#sent().expect("sent"),
        1,
        "the opening window is one batch, and nothing has granted more"
    );

    let got: Vec<i32> = (&mut rx).map(|item| item.expect("item")).collect();
    assert_eq!(got, (0..100).collect::<Vec<_>>());
}

/// A session that ends releases a consumer blocked in `recv`: only the death links end the wait.
#[test]
fn a_session_that_ends_releases_a_blocked_consumer() {
    let f = fixture("dead", 1);

    // A consumer takes no wider an opening window than it was made for.
    let (mut rx, endpoint) = f.receiver(default_credits() as u32, 1_000_000);
    // Half a minute between items and credit to spare: after the first
    // item nothing else arrives within this test, so when the session
    // goes the consumer is waiting and the producer is not parked.
    f.demo
        .r#subscribe(&endpoint, i32::MAX, 4, 1_000_000, 30_000_000)
        .expect("subscribe");
    assert!(rx.next().expect("item 0").is_ok());
    // Pays the owed credit, so the dead session is seen by the death link, not a grant.
    assert!(rx.try_recv().expect("still running").is_none());

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

    let (mut rx, endpoint) = f.receiver(1, default_credits() as u32);
    f.demo
        .r#subscribe(&endpoint, 1_000_000, 4, 1, 0)
        .expect("subscribe");
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
