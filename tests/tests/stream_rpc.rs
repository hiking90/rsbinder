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

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::rpc::{RpcClientConfig, RpcServer, RpcSession};
use rsbinder::stream::{PingPolicy, Receiver, ReceiverPolicy, Sink, SinkPolicy, StreamEndpoint};
use rsbinder::{
    BinderResult, ExceptionCode, FromIBinder, Interface, SIBinder, Status, Strong, TransportCaps,
};

include!(concat!(env!("OUT_DIR"), "/stream_demo.rs"));

use streamdemo::IStreamDemo::{BnStreamDemo, IStreamDemo};

/// The producer's default opening window, as the fixture's `int`.
fn default_credits() -> i32 {
    SinkPolicy::default().initial_credits as i32
}

/// Cloned into the server and kept by the test, which reads the counters without the wire.
#[derive(Clone, Default)]
struct DemoSvc {
    /// What the service's producers ping with.
    ping: PingPolicy,
    sent: Arc<AtomicI32>,
    finished: Arc<AtomicBool>,
    last_error: Arc<AtomicI32>,
    uploaded: Arc<AtomicI32>,
    upload_ordered: Arc<AtomicBool>,
    upload_finished: Arc<AtomicBool>,
    upload_error: Arc<AtomicI32>,
}

impl Interface for DemoSvc {}

/// Never dropped: a runtime dropped with a blocking task in flight waits for it.
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
    /// Own thread on purpose: the handler returns at once and pushing is outside any transaction.
    fn spawn(
        &self,
        endpoint: &StreamEndpoint,
        count: i32,
        policy: &SinkPolicy,
        delay: Duration,
        ending: Option<Status>,
    ) -> BinderResult<()> {
        let policy = SinkPolicy {
            ping: self.ping,
            ..policy.clone()
        };
        let mut producer = Sink::<i32>::open_with(endpoint, &policy)?;
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
        let policy = SinkPolicy {
            ping: self.ping,
            ..sink_policy(max_batch_bytes, initial_credits)
        };
        let mut producer = Sink::<i32>::open_with(endpoint, &policy)?;
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

    // Upload is exercised by the kernel half (`stream_probe`); this keeps the fixture whole.
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

/// A server on its own socket and a client session with `incoming` callback connections.
struct Fixture {
    _server: thread::JoinHandle<()>,
    client: RpcSession,
    demo: Strong<dyn IStreamDemo>,
    path: std::path::PathBuf,
    /// The service object's shared state.
    svc: DemoSvc,
    relay: Option<Relay>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.client.close_session();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// What [`fixture`] leaves at its defaults.
#[derive(Default)]
struct Setup {
    /// Route every connection of the session through a [`Relay`].
    relay: bool,
    client_timeout: Option<Duration>,
    server_reply_timeout: Option<Duration>,
    producer_ping: PingPolicy,
}

/// A unix relay that stops forwarding with connections left open, as a silent `adb forward` does.
struct Relay {
    path: PathBuf,
    frozen: Arc<AtomicBool>,
}

impl Relay {
    /// The accept thread is left blocked at the end, as the process exits after the tests.
    fn start(upstream: &Path, tag: &str) -> Relay {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rsb_stream_relay_{tag}_{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind the relay");
        let frozen = Arc::new(AtomicBool::new(false));
        let upstream = upstream.to_path_buf();
        let gate = frozen.clone();
        thread::spawn(move || {
            for near in listener.incoming() {
                let Ok(near) = near else { return };
                let Ok(far) = UnixStream::connect(&upstream) else {
                    return;
                };
                let (near_rx, far_rx) = (
                    near.try_clone().expect("clone"),
                    far.try_clone().expect("clone"),
                );
                let (a, b) = (gate.clone(), gate.clone());
                thread::spawn(move || Self::pump(near_rx, far, a));
                thread::spawn(move || Self::pump(far_rx, near, b));
            }
        });
        Relay { path, frozen }
    }

    /// Frozen, it holds what it has read and reads no more; the socket stays open.
    fn pump(mut from: UnixStream, mut to: UnixStream, frozen: Arc<AtomicBool>) {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            while frozen.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(10));
            }
            if to.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = to.shutdown(std::net::Shutdown::Write);
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        // Held bytes then flow into sessions already over, and the pumps see them close.
        self.frozen.store(false, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Fixture {
    /// Against an RPC proxy, so the endpoint carries only the sink.
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

    /// Stop the relay forwarding either way, connections left open.
    fn freeze(&self) {
        let relay = self.relay.as_ref().expect("a fixture with a relay");
        relay.frozen.store(true, Ordering::SeqCst);
    }
}

fn fixture(tag: &str, incoming: u32) -> Fixture {
    fixture_with(tag, incoming, Setup::default())
}

fn fixture_with(tag: &str, incoming: u32, setup: Setup) -> Fixture {
    let mut path = std::env::temp_dir();
    path.push(format!("rsb_stream_{tag}_{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let svc = DemoSvc {
        ping: setup.producer_ping,
        ..DemoSvc::default()
    };
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(2);
    server.set_reply_timeout(setup.server_reply_timeout);
    server
        .set_root(BnStreamDemo::new_binder(svc.clone()).as_binder())
        .expect("set_root");
    let server = server.run_background();

    let relay = setup.relay.then(|| Relay::start(&path, tag));
    let dial = relay
        .as_ref()
        .map_or(path.as_path(), |relay| relay.path.as_path());
    // `setup_unix_server` already bound the listener, so connecting now cannot race the thread.
    let client = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(dial, 2).incoming_connections(incoming),
    )
    .expect("connect");
    client.set_timeout(setup.client_timeout);
    let demo = <dyn IStreamDemo as FromIBinder>::try_from(client.get_root().expect("get_root"))
        .expect("cast the root to IStreamDemo");

    Fixture {
        _server: server,
        client,
        demo,
        path,
        svc,
        relay,
    }
}

/// An RPC peer gets a sink-only endpoint: no shared memory carries a ring across a socket.
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

/// Plan 2-23 §3.3: 20000 one-item batches each way; their DEC_STRONGs must not stall a socket.
#[test]
fn twenty_thousand_items_cross_each_way_without_a_stall() {
    const N: i32 = 20_000;
    let f = fixture("bulk", 1);

    let (mut rx, endpoint) = f.default_receiver();
    f.demo
        .r#subscribe(&endpoint, N, 4, default_credits(), 0)
        .expect("subscribe");
    for want in 0..N {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Some(item)) => assert_eq!(item, want),
            other => panic!("download stalled after {want} items: {other:?}"),
        }
    }
    assert!(
        rx.recv_timeout(Duration::from_secs(10))
            .expect("a clean end")
            .is_none(),
        "the terminator follows the last batch"
    );

    let token = rsbinder::stream::Token::new();
    let ring_bytes = ReceiverPolicy::default().ring_bytes as i32;
    let endpoint = f
        .demo
        .r#upload(&token.binder(), ring_bytes)
        .expect("upload");
    let policy = SinkPolicy {
        send_timeout: Some(Duration::from_secs(10)),
        ..sink_policy(4, default_credits())
    };
    let mut tx = Sink::<i32>::open_with(&endpoint, &policy).expect("open");
    for item in 0..N {
        tx.send(&item)
            .unwrap_or_else(|e| panic!("upload stalled at item {item}: {e:?}"));
    }
    tx.end().expect("end");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !f.demo.r#uploadFinished().expect("uploadFinished") {
        assert!(
            std::time::Instant::now() < deadline,
            "the upload consumer never ended"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(f.demo.r#uploaded().expect("uploaded"), N);
    assert!(f.demo.r#uploadOrdered().expect("uploadOrdered"));
    assert_eq!(f.demo.r#uploadError().expect("uploadError"), 0);
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
    // Bounded: a grant lost on the wire leaves both ends waiting, a hang rather than a failure.
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

/// Without incoming connections the consumer refuses in its own process, before any call is made.
#[test]
fn a_session_without_callback_connections_refuses_the_stream() {
    let f = fixture("nocb", 0);
    assert!(
        !f.client.caps().contains(TransportCaps::CALLBACKS),
        "a founding connection alone never grants CALLBACKS"
    );

    assert_eq!(
        Receiver::<i32>::new(&f.demo.as_binder()).err(),
        Some(rsbinder::StatusCode::InvalidOperation),
        "nothing would read the producer's batches nor see it die"
    );
}

/// A late failure keeps its service-specific code, which is why the terminator has three fields.
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

/// Plan 10-7 AC-7.2 over RPC: one batch of credit, no drain, so the producer stops after it.
#[test]
fn the_window_bounds_what_the_producer_sends_before_anyone_drains() {
    let f = fixture("window", 1);

    let (mut rx, endpoint) = f.receiver(1, default_credits() as u32);
    // One integer per 4-byte batch and one opening credit: one item may leave before a grant.
    f.demo
        .r#subscribe(&endpoint, 100, 4, 1, 0)
        .expect("subscribe");

    // Nothing read yet, and credit is granted only for batches the consumer has taken.
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
    // 30 s per item, spare credit: at close the consumer is waiting and the producer not parked.
    f.demo
        .r#subscribe(&endpoint, i32::MAX, 4, 1_000_000, 30_000_000)
        .expect("subscribe");
    assert!(rx.next().expect("item 0").is_ok());
    // Pays the owed credit, so the dead session is seen by the death link, not a grant.
    assert!(rx.try_recv().expect("still running").is_none());

    f.client.close_session();

    // Bounded rather than `recv`: without the death link this would hang, reporting nothing.
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

/// Dropping the receiver releases a producer parked on a credit that will never come.
#[test]
fn dropping_the_receiver_stops_the_producer() {
    let f = fixture("drop", 1);

    let (mut rx, endpoint) = f.receiver(1, default_credits() as u32);
    f.demo
        .r#subscribe(&endpoint, 1_000_000, 4, 1, 0)
        .expect("subscribe");
    assert_eq!(rx.next().expect("item 0").expect("ok"), 0);

    // Parked with nothing left to grant: while the receiver lives, it stays here.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while f.demo.r#sent().expect("sent") < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the grant for batch 0 must let item 1 through"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !f.demo.r#finished().expect("finished"),
        "the producer must still be waiting for credit"
    );
    let stopped = f.demo.r#sent().expect("sent");
    assert_eq!(
        stopped, 2,
        "item 0 on the opening credit, item 1 on the one grant"
    );

    // The drop's cancel releases it; polled, as the cancel and the wake are on two threads.
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

// ---- Plan 2-24 D9: a peer gone silent behind a relay ----

/// The reply deadline of the pinging side: a ping after a third, the session's end a deadline on.
const DEADLINE: Duration = Duration::from_millis(600);

/// Well past the four thirds a ping takes to end the session, well short of the 10 s bound.
const NOTICED_WITHIN: Duration = Duration::from_secs(3);

/// A download whose consumer is waiting, the producer idle between items, and the relay frozen.
fn frozen_download(tag: &str, ping: PingPolicy) -> (Fixture, Receiver<i32>) {
    let f = fixture_with(
        tag,
        1,
        Setup {
            relay: true,
            client_timeout: Some(DEADLINE),
            ..Setup::default()
        },
    );
    let (mut rx, endpoint) = Receiver::<i32>::with_policy(
        &f.demo.as_binder(),
        &ReceiverPolicy {
            max_opening: 1_000_000,
            ping,
            ..ReceiverPolicy::default()
        },
    )
    .expect("a receiver");
    // 30 s per item and spare credit: after item 0 neither end sends anything on its own.
    f.demo
        .r#subscribe(&endpoint, i32::MAX, 4, 1_000_000, 30_000_000)
        .expect("subscribe");
    assert!(rx.next().expect("item 0").is_ok());
    // Pays the owed credit, so the wait below has no grant to send.
    assert!(rx.try_recv().expect("still running").is_none());
    f.freeze();
    (f, rx)
}

/// A waiting consumer pings the producer, and the unanswered ping ends the stream.
#[test]
fn a_waiting_consumer_notices_a_producer_gone_silent_behind_a_relay() {
    let (_f, mut rx) = frozen_download("ping_rx", PingPolicy::Inherit);

    let started = Instant::now();
    let failure = rx
        .recv_timeout(Duration::from_secs(10))
        .expect_err("the ping goes unanswered and the session ends");
    assert_eq!(
        failure.transaction_error(),
        rsbinder::StatusCode::DeadObject
    );
    assert!(
        started.elapsed() < NOTICED_WITHIN,
        "noticed after {:?}",
        started.elapsed()
    );
}

/// The baseline: the relay keeps the connection looking healthy, so without a ping nothing ends.
#[test]
fn without_a_ping_a_producer_gone_silent_behind_a_relay_goes_unnoticed() {
    let (_f, mut rx) = frozen_download("noping_rx", PingPolicy::Off);

    assert!(
        rx.recv_timeout(NOTICED_WITHIN)
            .expect("nothing ends the stream")
            .is_none(),
        "no item crosses a frozen relay"
    );
    assert!(!rx.is_finished());
}

/// The same from `recv_async`, whose wait and ping run on the blocking pool.
#[test]
fn an_async_consumer_notices_a_producer_gone_silent_behind_a_relay() {
    let (_f, mut rx) = frozen_download("ping_rx_async", PingPolicy::Inherit);

    let (tx, done) = mpsc::channel();
    thread::spawn(move || {
        let started = Instant::now();
        let got = runtime().block_on(rx.recv_async());
        let _ = tx.send((got, started.elapsed()));
    });
    // Bounded here, so a missing ping fails the test rather than hanging it.
    let (got, elapsed) = done
        .recv_timeout(Duration::from_secs(10))
        .expect("recv_async never returned");
    let failure = got.expect("an end").expect_err("the session ends");
    assert_eq!(
        failure.transaction_error(),
        rsbinder::StatusCode::DeadObject
    );
    assert!(elapsed < NOTICED_WITHIN, "noticed after {elapsed:?}");
}

/// A dropped `recv_async` ends its pooled wait, freeing the pool's only thread before any ping.
#[test]
fn a_dropped_recv_async_gives_back_the_thread_its_wait_held() {
    // A ping only every 20 s, so an abandoned wait would hold the thread past the bound below.
    let f = fixture_with(
        "abandon_rx",
        1,
        Setup {
            client_timeout: Some(Duration::from_secs(60)),
            ..Setup::default()
        },
    );
    let (mut rx, endpoint) = f.default_receiver();
    f.demo
        .r#subscribe(&endpoint, i32::MAX, 4, default_credits(), 30_000_000)
        .expect("subscribe");
    assert!(rx.next().expect("item 0").is_ok());
    // Pays the owed credit, so `recv_async` goes straight to its pooled wait.
    assert!(rx.try_recv().expect("still running").is_none());

    let (tx, done) = mpsc::channel();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .build()
            .expect("runtime");
        let freed = runtime.block_on(async {
            {
                let recv = rx.recv_async();
                let mut recv = std::pin::pin!(recv);
                let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                assert!(std::future::Future::poll(recv.as_mut(), &mut cx).is_pending());
            }
            tokio::task::spawn_blocking(|| ()).await
        });
        let _ = tx.send(freed.is_ok());
    });
    assert!(done
        .recv_timeout(Duration::from_secs(10))
        .expect("the abandoned wait still holds the pool's only thread"));
}

/// The service's producer parked on credit, relay frozen; only the service has a deadline.
fn parked_producer(tag: &str, ping: PingPolicy, subscribe_async: bool) -> Fixture {
    let f = fixture_with(
        tag,
        1,
        Setup {
            relay: true,
            server_reply_timeout: Some(DEADLINE),
            producer_ping: ping,
            ..Setup::default()
        },
    );
    let (rx, endpoint) = f.receiver(1, default_credits() as u32);
    // One opening credit and one item per batch; nothing is read, so no grant ever comes.
    if subscribe_async {
        f.demo.r#subscribeAsync(&endpoint, 1_000_000, 4, 1)
    } else {
        f.demo.r#subscribe(&endpoint, 1_000_000, 4, 1, 0)
    }
    .expect("subscribe");
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.svc.sent.load(Ordering::SeqCst) < 1 {
        assert!(Instant::now() < deadline, "the first batch never left");
        thread::sleep(Duration::from_millis(10));
    }
    f.freeze();
    // Kept alive but unread: dropping it would cancel the producer.
    std::mem::forget(rx);
    f
}

/// How the service's producer ended, once it has; `None` if it is still going after `wait`.
fn producer_outcome(f: &Fixture, wait: Duration) -> Option<i32> {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if f.svc.finished.load(Ordering::SeqCst) {
            return Some(f.svc.last_error.load(Ordering::SeqCst));
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

/// A producer parked on credit pings the consumer's sink, and the unanswered ping ends it.
#[test]
fn a_producer_waiting_for_credit_notices_a_consumer_gone_silent_behind_a_relay() {
    let f = parked_producer("ping_tx", PingPolicy::Inherit, false);
    assert_eq!(
        producer_outcome(&f, NOTICED_WITHIN),
        Some(i32::from(rsbinder::StatusCode::DeadObject)),
        "the unanswered ping ends the session, and the sink's death the producer's wait"
    );
}

/// The same from `send_async`, whose credit wait and ping run on the blocking pool.
#[test]
fn an_async_producer_notices_a_consumer_gone_silent_behind_a_relay() {
    let f = parked_producer("ping_tx_async", PingPolicy::Inherit, true);
    assert_eq!(
        producer_outcome(&f, NOTICED_WITHIN),
        Some(i32::from(rsbinder::StatusCode::DeadObject))
    );
}

/// The producer's baseline: with its ping off, it waits for credit through the whole bound.
#[test]
fn without_a_ping_a_producer_waits_on_a_consumer_gone_silent() {
    let f = parked_producer("noping_tx", PingPolicy::Off, false);
    assert_eq!(producer_outcome(&f, NOTICED_WITHIN), None);
}
