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

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::rpc::{FileDescriptorTransportMode, RpcClientConfig, RpcServer, RpcSession};
use rsbinder::stream::{
    PingPolicy, Receiver, ReceiverPolicy, RingUse, Sink, SinkPolicy, StreamEndpoint, Token,
};
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
    /// Whether the service's upload receiver may take a ring from an RPC client.
    upload_ring_use: RingUse,
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
        endpoint: &StreamEndpoint<i32>,
        count: i32,
        policy: &SinkPolicy,
        delay: Duration,
        ending: Option<Status>,
    ) -> BinderResult<()> {
        let policy = SinkPolicy {
            ping: self.ping,
            ..policy.clone()
        };
        let mut producer = Sink::open_with(endpoint, &policy)?;
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
        endpoint: &StreamEndpoint<i32>,
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
        endpoint: &StreamEndpoint<i32>,
        count: i32,
        max_batch_bytes: i32,
        initial_credits: i32,
    ) -> BinderResult<()> {
        let policy = SinkPolicy {
            ping: self.ping,
            ..sink_policy(max_batch_bytes, initial_credits)
        };
        let mut producer = Sink::open_with(endpoint, &policy)?;
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
        endpoint: &StreamEndpoint<i32>,
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

    fn r#upload(&self, producer: &SIBinder, ring_bytes: i32) -> BinderResult<StreamEndpoint<i32>> {
        let (mut rx, endpoint) = Receiver::<i32>::with_policy(
            producer,
            &ReceiverPolicy {
                ring_bytes: ring_bytes.max(0) as usize,
                ring_use: self.upload_ring_use,
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
    /// Negotiate the `Unix` fd mode, so the session passes file descriptors.
    fd_unix: bool,
    upload_ring_use: RingUse,
    server_idle: Option<Duration>,
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

    /// Frozen, it holds what it has read and reads no more; fds cross with their bytes (a memfd).
    fn pump(from: UnixStream, to: UnixStream, frozen: Arc<AtomicBool>) {
        use rustix::net::{
            RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
            SendAncillaryMessage, SendFlags,
        };
        use std::io::{IoSlice, IoSliceMut};
        use std::mem::MaybeUninit;
        use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

        let mut buf = [0u8; 16 * 1024];
        let mut in_space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(64))];
        let mut out_space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(64))];
        'pump: loop {
            let mut anc = RecvAncillaryBuffer::new(&mut in_space);
            let n = match rustix::net::recvmsg(
                &from,
                &mut [IoSliceMut::new(&mut buf)],
                &mut anc,
                RecvFlags::empty(),
            ) {
                Ok(r) if r.bytes > 0 => r.bytes,
                Err(rustix::io::Errno::INTR) => continue,
                _ => break,
            };
            let fds: Vec<OwnedFd> = anc
                .drain()
                .filter_map(|msg| match msg {
                    RecvAncillaryMessage::ScmRights(fds) => Some(fds),
                    _ => None,
                })
                .flatten()
                .collect();
            while frozen.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(10));
            }
            let fds: Vec<BorrowedFd<'_>> = fds.iter().map(|fd| fd.as_fd()).collect();
            let mut sent = 0;
            while sent < n {
                let mut anc = SendAncillaryBuffer::new(&mut out_space);
                if sent == 0 && !fds.is_empty() {
                    assert!(anc.push(SendAncillaryMessage::ScmRights(&fds)));
                }
                match rustix::net::sendmsg(
                    &to,
                    &[IoSlice::new(&buf[sent..n])],
                    &mut anc,
                    SendFlags::empty(),
                ) {
                    Ok(0) => break 'pump,
                    Ok(k) => sent += k,
                    Err(rustix::io::Errno::INTR) => {}
                    Err(_) => break 'pump,
                }
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
    fn receiver(
        &self,
        credit_window: u32,
        max_opening: u32,
    ) -> (Receiver<i32>, StreamEndpoint<i32>) {
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

    fn default_receiver(&self) -> (Receiver<i32>, StreamEndpoint<i32>) {
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
        upload_ring_use: setup.upload_ring_use,
        ..DemoSvc::default()
    };
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(2);
    server.set_reply_timeout(setup.server_reply_timeout);
    server.set_idle_timeout(setup.server_idle);
    if setup.fd_unix {
        server.set_supported_fd_modes(&[FileDescriptorTransportMode::Unix]);
    }
    server
        .set_root(BnStreamDemo::new_binder(svc.clone()).as_binder())
        .expect("set_root");
    let server = server.run_background();

    let relay = setup.relay.then(|| Relay::start(&path, tag));
    let dial = relay
        .as_ref()
        .map_or(path.as_path(), |relay| relay.path.as_path());
    // `setup_unix_server` already bound the listener, so connecting now cannot race the thread.
    let mut config = RpcClientConfig::unix(dial, 2).incoming_connections(incoming);
    if setup.fd_unix {
        config = config.fd_mode(FileDescriptorTransportMode::Unix);
    }
    let client = RpcSession::setup_client_android13plus_with_config(config).expect("connect");
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

/// Dropped before its pooled wait sleeps, `recv_async` frees the pool's only thread, no ping.
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

// ---- Plan 10-7c Phase B: a ring over a Unix session ----

/// Where the ring exists; elsewhere an opted-in consumer runs on calls and the same tests check that.
const RING: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// A session that can carry a ring: a Unix socket on this host, `Unix` fds, one incoming connection.
fn ring_fixture(tag: &str, upload_ring_use: RingUse) -> Fixture {
    fixture_with(
        tag,
        1,
        Setup {
            fd_unix: true,
            upload_ring_use,
            ..Setup::default()
        },
    )
}

fn opted_in() -> ReceiverPolicy {
    ReceiverPolicy {
        ring_use: RingUse::AlsoUnixRpc,
        ..ReceiverPolicy::default()
    }
}

/// Every item to a clean end, each within 10 s: a stall fails rather than hangs.
fn collect_within(rx: &mut Receiver<i32>) -> Vec<i32> {
    let mut got = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Some(item)) => got.push(item),
            Ok(None) if rx.is_finished() => return got,
            other => panic!("stalled after {} items: {other:?}", got.len()),
        }
    }
}

/// A send that waits for room or credit fails after 10 s rather than hangs.
fn bounded_sink() -> SinkPolicy {
    SinkPolicy {
        send_timeout: Some(Duration::from_secs(10)),
        ..SinkPolicy::default()
    }
}

/// Poll `done` for up to 5 s.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// AC-C1 (AC-C3 off Linux): an opted-in download over a Unix session runs on a ring.
#[test]
fn an_opted_in_download_runs_on_a_ring_over_a_unix_session() {
    let f = ring_fixture("ring_down", RingUse::KernelOnly);
    let caps = f.client.caps();
    assert!(
        caps.contains(
            TransportCaps::FD_PASSING | TransportCaps::SAME_HOST | TransportCaps::CALLBACKS
        ),
        "{caps}"
    );
    let (mut rx, endpoint) =
        Receiver::<i32>::with_policy(&f.demo.as_binder(), &opted_in()).expect("a receiver");
    assert_eq!(endpoint.ring.is_some(), RING);
    assert_eq!(rx.uses_ring(), RING);
    f.demo
        .r#subscribe(&endpoint, 1000, 64, default_credits(), 0)
        .expect("subscribe");
    let got = collect_within(&mut rx);
    assert_eq!(got, (0..1000).collect::<Vec<_>>());
    assert!(rx.end_status().expect("ended").is_ok());
}

/// AC-C2: without the opt-in a session that could carry a ring still gets calls.
#[test]
fn a_consumer_that_does_not_opt_in_gets_calls_on_a_ring_capable_session() {
    let f = ring_fixture("ring_off", RingUse::KernelOnly);
    let (rx, endpoint) = f.default_receiver();
    assert!(endpoint.ring.is_none());
    assert!(!rx.uses_ring());
}

/// AC-C3: an opt-in on a session that passes no fds falls back to calls and still streams.
#[test]
fn an_opted_in_consumer_without_fd_passing_gets_calls() {
    let f = fixture("ring_nofd", 1);
    assert!(!f.client.caps().contains(TransportCaps::FD_PASSING));
    let (mut rx, endpoint) =
        Receiver::<i32>::with_policy(&f.demo.as_binder(), &opted_in()).expect("a receiver");
    assert!(endpoint.ring.is_none());
    assert!(!rx.uses_ring());
    f.demo
        .r#subscribe(&endpoint, 100, 64, default_credits(), 0)
        .expect("subscribe");
    let got = collect_within(&mut rx);
    assert_eq!(got, (0..100).collect::<Vec<_>>());
}

/// AC-C4: without incoming connections the opt-in changes nothing; the stream is refused as before.
#[test]
fn an_opted_in_consumer_without_callbacks_is_refused_as_before() {
    let f = fixture_with(
        "ring_nocb",
        0,
        Setup {
            fd_unix: true,
            ..Setup::default()
        },
    );
    assert!(!f.client.caps().contains(TransportCaps::CALLBACKS));
    assert_eq!(
        Receiver::<i32>::with_policy(&f.demo.as_binder(), &opted_in()).err(),
        Some(rsbinder::StatusCode::InvalidOperation)
    );
}

/// AC-C8 and AC-C7: an opted-in upload runs on the service's ring; its memfd keeps its seals.
#[test]
fn an_opted_in_upload_runs_on_a_ring_whose_memfd_keeps_its_seals() {
    let f = ring_fixture("ring_up", RingUse::AlsoUnixRpc);
    let token = Token::new();
    let endpoint = f.demo.r#upload(&token.binder(), 64 * 1024).expect("upload");
    assert_eq!(endpoint.ring.is_some(), RING);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::fd::AsFd;
        let ring = endpoint.ring.as_ref().expect("a ring");
        let desc = rsbinder::fmq::Descriptor::try_from(ring).expect("a descriptor");
        assert!(
            rsbinder::fmq::shm::shrink_sealed(desc.fds[0].as_fd()),
            "the memfd keeps F_SEAL_SHRINK across SCM_RIGHTS"
        );
    }
    let mut sink =
        Sink::<i32>::open_with(&endpoint, &bounded_sink()).expect("open the service's ring");
    for item in 0..1000 {
        sink.send(&item).expect("send");
    }
    sink.end().expect("end");
    eventually("the service's receiver must finish", || {
        f.svc.upload_finished.load(Ordering::SeqCst)
    });
    assert_eq!(f.svc.uploaded.load(Ordering::SeqCst), 1000);
    assert!(f.svc.upload_ordered.load(Ordering::SeqCst));
    assert_eq!(f.svc.upload_error.load(Ordering::SeqCst), 0);
}

/// An opted-in download through a relay, the producer idle between items, then the relay frozen.
fn frozen_ring_download(tag: &str, ping: PingPolicy) -> (Fixture, Receiver<i32>) {
    let f = fixture_with(
        tag,
        1,
        Setup {
            relay: true,
            fd_unix: true,
            client_timeout: Some(DEADLINE),
            ..Setup::default()
        },
    );
    let (mut rx, endpoint) = Receiver::<i32>::with_policy(
        &f.demo.as_binder(),
        &ReceiverPolicy {
            ring_use: RingUse::AlsoUnixRpc,
            max_opening: 1_000_000,
            ping,
            ..ReceiverPolicy::default()
        },
    )
    .expect("a receiver");
    assert_eq!(rx.uses_ring(), RING, "the memfd crossed the relay");
    // 30 s per item and spare credit: after item 0 neither end does anything on its own.
    f.demo
        .r#subscribe(&endpoint, i32::MAX, 4, 1_000_000, 30_000_000)
        .expect("subscribe");
    assert!(rx.next().expect("item 0").is_ok());
    // Off a ring this pays the owed credit, so the wait below has no grant to send.
    assert!(rx.try_recv().expect("still running").is_none());
    f.freeze();
    (f, rx)
}

/// AC-C6, consumer: a waiting ring consumer pings a silent producer; no answer ends the stream.
#[test]
fn a_waiting_ring_consumer_notices_a_producer_gone_silent_behind_a_relay() {
    let (_f, mut rx) = frozen_ring_download("ring_ping_rx", PingPolicy::Inherit);
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

/// AC-C6 baseline: without the ping the same ring consumer keeps waiting.
#[test]
fn without_a_ping_a_ring_consumer_waits_on_a_producer_gone_silent() {
    let (_f, mut rx) = frozen_ring_download("ring_noping_rx", PingPolicy::Off);
    assert_eq!(rx.recv_timeout(NOTICED_WITHIN).expect("no error"), None);
    assert!(!rx.is_finished());
}

/// An opted-in download read by nobody: the producer parks (off Linux, on credit), then a freeze.
fn parked_ring_producer(tag: &str, ping: PingPolicy) -> Fixture {
    let f = fixture_with(
        tag,
        1,
        Setup {
            relay: true,
            fd_unix: true,
            server_reply_timeout: Some(DEADLINE),
            producer_ping: ping,
            ..Setup::default()
        },
    );
    let (rx, endpoint) = Receiver::<i32>::with_policy(
        &f.demo.as_binder(),
        &ReceiverPolicy {
            ring_use: RingUse::AlsoUnixRpc,
            ring_bytes: 4096,
            credit_window: 1,
            ..ReceiverPolicy::default()
        },
    )
    .expect("a receiver");
    assert_eq!(rx.uses_ring(), RING);
    f.demo
        .r#subscribe(&endpoint, 1_000_000, 4, 1, 0)
        .expect("subscribe");
    // On the ring: (4096 - 256) / 8 records, then the producer parks for room.
    let parked_at = if RING { (4096 - 256) / 8 } else { 1 };
    eventually("the producer must fill what it may", || {
        f.svc.sent.load(Ordering::SeqCst) >= parked_at
    });
    f.freeze();
    // Kept alive but unread: dropping it would cancel the producer.
    std::mem::forget(rx);
    f
}

/// AC-C6, producer: one parked on a full ring pings the consumer's sink; no answer ends it.
#[test]
fn a_ring_producer_parked_for_room_notices_a_consumer_gone_silent_behind_a_relay() {
    let f = parked_ring_producer("ring_ping_tx", PingPolicy::Inherit);
    assert_eq!(
        producer_outcome(&f, NOTICED_WITHIN),
        Some(i32::from(rsbinder::StatusCode::DeadObject))
    );
}

/// AC-C6 baseline: without the ping the parked ring producer keeps waiting.
#[test]
fn without_a_ping_a_ring_producer_waits_on_a_consumer_gone_silent() {
    let f = parked_ring_producer("ring_noping_tx", PingPolicy::Off);
    assert_eq!(producer_outcome(&f, NOTICED_WITHIN), None);
}

/// AC-C5: a closed session ends an RPC ring stream both ways, after what was in the ring.
#[test]
fn a_closed_session_ends_an_rpc_ring_stream_on_both_sides() {
    let f = ring_fixture("ring_dead", RingUse::KernelOnly);
    let (mut rx, endpoint) =
        Receiver::<i32>::with_policy(&f.demo.as_binder(), &opted_in()).expect("a receiver");
    assert_eq!(rx.uses_ring(), RING);
    // 20 ms an item: the producer is between items, not parked, when the session goes.
    f.demo
        .r#subscribe(&endpoint, i32::MAX, 64, default_credits(), 20_000)
        .expect("subscribe");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).expect("ok"),
        Some(0)
    );

    f.client.close_session();

    let failure = loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Some(_)) => continue,
            Ok(None) if rx.is_finished() => panic!("a clean end the producer never sent"),
            Ok(None) => panic!("the lost session left the consumer waiting"),
            Err(status) => break status,
        }
    };
    assert_eq!(
        failure.transaction_error(),
        rsbinder::StatusCode::DeadObject
    );
    let dead = i32::from(rsbinder::StatusCode::DeadObject);
    eventually("the producer must see the consumer's death", || {
        f.svc.last_error.load(Ordering::SeqCst) == dead
    });
}

// ---- AC-C13: a ring stream and the server's idle timeout (ring only: calls batch items) ----

#[cfg(any(target_os = "linux", target_os = "android"))]
mod idle {
    use super::*;

    /// The server's idle timeout: a session with no activity for between this and twice it ends.
    const IDLE: Duration = Duration::from_millis(200);

    /// A ring-capable session whose server has an idle timeout and takes uploads on a ring.
    fn idle_fixture(tag: &str, idle: Duration, server_reply_timeout: Option<Duration>) -> Fixture {
        fixture_with(
            tag,
            1,
            Setup {
                fd_unix: true,
                upload_ring_use: RingUse::AlsoUnixRpc,
                server_idle: Some(idle),
                server_reply_timeout,
                ..Setup::default()
            },
        )
    }

    /// The server produces `count` items `gap` apart; the items, or how the stream failed.
    fn spaced_download(f: &Fixture, count: i32, gap: Duration) -> Result<Vec<i32>, Status> {
        let (mut rx, endpoint) =
            Receiver::<i32>::with_policy(&f.demo.as_binder(), &opted_in()).expect("a receiver");
        assert_eq!(rx.uses_ring(), RING);
        f.demo
            .r#subscribe(
                &endpoint,
                count,
                64,
                default_credits(),
                gap.as_micros() as i32,
            )
            .expect("subscribe");
        let mut got = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Some(item)) => got.push(item),
                Ok(None) if rx.is_finished() => return Ok(got),
                Ok(None) => panic!("no item for 10 s after {}", got.len()),
                Err(status) => return Err(status),
            }
        }
    }

    /// The client produces `count` items `gap` apart into the server's ring; the server's verdict.
    fn spaced_upload(f: &Fixture, count: i32, gap: Duration) -> (i32, i32) {
        let token = Token::new();
        let endpoint = f.demo.r#upload(&token.binder(), 64 * 1024).expect("upload");
        assert_eq!(endpoint.ring.is_some(), RING);
        let mut sink = Sink::<i32>::open(&endpoint).expect("open");
        for item in 0..count {
            if sink.send(&item).is_err() {
                break;
            }
            thread::sleep(gap);
        }
        let _ = sink.end();
        eventually("the server's receiver must finish", || {
            f.svc.upload_finished.load(Ordering::SeqCst)
        });
        (
            f.svc.uploaded.load(Ordering::SeqCst),
            f.svc.upload_error.load(Ordering::SeqCst),
        )
    }

    /// AC-C13 (1): items flowing on the ring are activity, though no byte crosses the socket.
    #[test]
    fn a_flowing_ring_stream_outlives_the_servers_idle_timeout() {
        let f = idle_fixture("idle_flow_dl", IDLE, None);
        let started = Instant::now();
        let got = spaced_download(&f, 2500, Duration::from_millis(1)).expect("a clean end");
        assert_eq!(got, (0..2500).collect::<Vec<_>>());
        assert!(started.elapsed() > 4 * IDLE, "ran {:?}", started.elapsed());

        let f = idle_fixture("idle_flow_up", IDLE, None);
        assert_eq!(spaced_upload(&f, 2500, Duration::from_millis(1)), (2500, 0));
    }

    /// AC-C13 (2): a stream slower than a record now and then, but faster than the timeout, stays.
    #[test]
    fn a_slow_ring_stream_inside_the_idle_timeout_stays_alive() {
        let idle = Duration::from_secs(1);
        let f = idle_fixture("idle_slow_dl", idle, None);
        let got = spaced_download(&f, 8, Duration::from_millis(300)).expect("a clean end");
        assert_eq!(got, (0..8).collect::<Vec<_>>());

        let f = idle_fixture("idle_slow_up", idle, None);
        assert_eq!(spaced_upload(&f, 8, Duration::from_millis(300)), (8, 0));
    }

    /// AC-C13 (3): a client cannot hold a server's thread by opening a ring and going quiet.
    #[test]
    fn a_stalled_ring_stream_is_ended_by_the_idle_timeout() {
        let dead = i32::from(rsbinder::StatusCode::DeadObject);

        // The server produces into a small ring nobody reads, and parks.
        let f = idle_fixture("idle_stall_dl", IDLE, None);
        let (rx, endpoint) = Receiver::<i32>::with_policy(
            &f.demo.as_binder(),
            &ReceiverPolicy {
                ring_use: RingUse::AlsoUnixRpc,
                ring_bytes: 4096,
                credit_window: 1,
                ..ReceiverPolicy::default()
            },
        )
        .expect("a receiver");
        assert_eq!(rx.uses_ring(), RING);
        f.demo
            .r#subscribe(&endpoint, 1_000_000, 4, 1, 0)
            .expect("subscribe");
        assert_eq!(producer_outcome(&f, Duration::from_secs(5)), Some(dead));
        drop(rx);

        // The server consumes from a client that wrote one item and stopped.
        let f = idle_fixture("idle_stall_up", IDLE, None);
        let token = Token::new();
        let endpoint = f.demo.r#upload(&token.binder(), 64 * 1024).expect("upload");
        assert_eq!(endpoint.ring.is_some(), RING);
        let mut sink = Sink::<i32>::open(&endpoint).expect("open");
        sink.send(&0).expect("send");
        eventually("the idle session must end the server's receiver", || {
            f.svc.upload_finished.load(Ordering::SeqCst)
        });
        assert_eq!(f.svc.upload_error.load(Ordering::SeqCst), dead);
        drop(sink);
    }

    /// AC-C13 (4): a stalled ring stream that pings is not idle: each ping ends a wait, and each re-park counts.
    #[test]
    fn a_stalled_ring_stream_that_pings_survives_the_idle_timeout() {
        let reply = Some(Duration::from_millis(300));

        let f = idle_fixture("idle_ping_dl", IDLE, reply);
        let (rx, endpoint) = Receiver::<i32>::with_policy(
            &f.demo.as_binder(),
            &ReceiverPolicy {
                ring_use: RingUse::AlsoUnixRpc,
                ring_bytes: 4096,
                credit_window: 1,
                ..ReceiverPolicy::default()
            },
        )
        .expect("a receiver");
        assert_eq!(rx.uses_ring(), RING);
        f.demo
            .r#subscribe(&endpoint, 1_000_000, 4, 1, 0)
            .expect("subscribe");
        assert_eq!(producer_outcome(&f, Duration::from_millis(1500)), None);
        drop(rx);

        let f = idle_fixture("idle_ping_up", IDLE, reply);
        let token = Token::new();
        let endpoint = f.demo.r#upload(&token.binder(), 64 * 1024).expect("upload");
        assert_eq!(endpoint.ring.is_some(), RING);
        let mut sink = Sink::<i32>::open(&endpoint).expect("open");
        sink.send(&0).expect("send");
        thread::sleep(Duration::from_millis(1500));
        assert!(
            !f.svc.upload_finished.load(Ordering::SeqCst),
            "the server's receiver pings the client and stays"
        );
        sink.end().expect("end");
        eventually("the upload ends once the client ends it", || {
            f.svc.upload_finished.load(Ordering::SeqCst)
        });
        assert_eq!(f.svc.upload_error.load(Ordering::SeqCst), 0);
    }
}

// ---- AC-C5: a ring over RPC between two processes ----

#[cfg(any(target_os = "linux", target_os = "android"))]
mod two_process {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, ChildStdout, Command, Stdio};

    /// `stream_probe serve-rpc-ring` in a process of its own; killed and reaped on drop.
    struct Server {
        child: Child,
        _stdout: BufReader<ChildStdout>,
    }

    impl Server {
        fn kill(&mut self) {
            // A child under another uid (`su`, plan 10-7c B6) refuses the kill; a wait would hang.
            if self.child.kill().is_ok() {
                let _ = self.child.wait();
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.kill();
        }
    }

    /// An abstract socket: no file for a client in another domain to be allowed to write.
    fn serve(tag: &str) -> (Server, RpcSession, Strong<dyn IStreamDemo>) {
        let name = format!("rsb_stream_2p_{tag}_{}", std::process::id());
        // `STREAM_PROBE_BIN` on a device, where the build-time path does not exist.
        let probe = std::env::var("STREAM_PROBE_BIN")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_stream_probe").to_string());
        let mut child = Command::new(probe)
            .arg("serve-rpc-ring")
            .arg(format!("@{name}"))
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn stream_probe");
        // Kept open: the child's stdout must not become a broken pipe.
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut line = String::new();
        stdout.read_line(&mut line).expect("the child's first line");
        assert!(line.starts_with("SERVING"), "stream_probe said {line:?}");
        let server = Server {
            child,
            _stdout: stdout,
        };
        let client = RpcSession::setup_client_android13plus_with_config(
            RpcClientConfig::unix_abstract(name.as_bytes(), 2)
                .incoming_connections(1)
                .fd_mode(FileDescriptorTransportMode::Unix),
        )
        .expect("connect");
        let demo = <dyn IStreamDemo as FromIBinder>::try_from(client.get_root().expect("root"))
            .expect("cast the root to IStreamDemo");
        (server, client, demo)
    }

    /// A ring over RPC both ways between two processes: the memfd crosses in an argument and a reply.
    #[test]
    fn a_ring_over_rpc_crosses_two_processes() {
        let (_server, _client, demo) = serve("cross");

        let (mut rx, endpoint) =
            Receiver::<i32>::with_policy(&demo.as_binder(), &opted_in()).expect("a receiver");
        assert!(rx.uses_ring());
        demo.r#subscribe(&endpoint, 200_000, 64, default_credits(), 0)
            .expect("subscribe");
        assert!(
            collect_within(&mut rx).into_iter().eq(0..200_000),
            "200 000 items in order"
        );
        assert!(rx.end_status().expect("ended").is_ok());

        let token = Token::new();
        let endpoint = demo.r#upload(&token.binder(), 64 * 1024).expect("upload");
        assert!(endpoint.ring.is_some(), "the child's receiver took a ring");
        let mut sink =
            Sink::<i32>::open_with(&endpoint, &bounded_sink()).expect("open the child's ring");
        for item in 0..50_000 {
            sink.send(&item).expect("send");
        }
        sink.end().expect("end");
        eventually("the child's receiver must finish", || {
            demo.r#uploadFinished().expect("uploadFinished")
        });
        assert_eq!(demo.r#uploaded().expect("uploaded"), 50_000);
        assert!(demo.r#uploadOrdered().expect("uploadOrdered"));
        assert_eq!(demo.r#uploadError().expect("uploadError"), 0);
    }

    /// The producer's process dies: the consumer gets what was in the ring, then `DeadObject`.
    #[test]
    fn a_killed_rpc_ring_producer_ends_the_consumer_after_its_records() {
        let (mut server, _client, demo) = serve("producer_dies");
        let (mut rx, endpoint) =
            Receiver::<i32>::with_policy(&demo.as_binder(), &opted_in()).expect("a receiver");
        assert!(rx.uses_ring());
        demo.r#subscribe(&endpoint, i32::MAX, 64, default_credits(), 1000)
            .expect("subscribe");
        for expected in 0..10 {
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(10)).expect("ok"),
                Some(expected)
            );
        }
        server.kill();
        let mut expected = 10;
        let failure = loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Some(item)) => {
                    assert_eq!(item, expected, "records before the death come in order");
                    expected += 1;
                }
                Ok(None) if rx.is_finished() => panic!("a clean end the producer never sent"),
                Ok(None) => panic!("the dead producer left the consumer waiting"),
                Err(status) => break status,
            }
        };
        assert_eq!(
            failure.transaction_error(),
            rsbinder::StatusCode::DeadObject
        );
    }

    /// The consumer's process dies: the producer's writes fail with `DeadObject` once it sees that.
    #[test]
    fn a_killed_rpc_ring_consumer_ends_the_producer() {
        let (mut server, _client, demo) = serve("consumer_dies");
        let token = Token::new();
        let endpoint = demo.r#upload(&token.binder(), 4096).expect("upload");
        assert!(endpoint.ring.is_some());
        let mut sink =
            Sink::<i32>::open_with(&endpoint, &bounded_sink()).expect("open the child's ring");
        for item in 0..100 {
            sink.send(&item).expect("send");
        }
        server.kill();
        let deadline = Instant::now() + Duration::from_secs(10);
        let failure = loop {
            assert!(
                Instant::now() < deadline,
                "the producer never saw the death"
            );
            if let Err(e) = sink.send(&0) {
                break e;
            }
        };
        assert_eq!(failure, rsbinder::StatusCode::DeadObject);
    }
}
