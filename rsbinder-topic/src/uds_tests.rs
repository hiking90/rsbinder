// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The plain entry point of `uds` within one process: an RPC proxy is one in the same process
//! too, so the whole subscription runs without a binder device. The two-process paths are in
//! `tests/uds.rs`.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rsbinder::rpc::{RpcServer, RpcServerHandle};
use rsbinder::{ExceptionCode, ParcelFileDescriptor, StatusCode, Strong};

use crate::aidl::ITopic::{ITopic, WAKE_FUTEX};
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::connect::Token;
use crate::connect_tests::{alone, memfds};
use crate::layout::{ControlPage, MIN_RING, VERSION};
use crate::pool::MAX_PINS;
use crate::uds::{self, Endpoint, Options, SessionGuard};
use crate::{
    Authorize, Backpressure, Bytes, Closed, Payload, Publisher, PublisherPolicy, RecvError,
    SubscriberPolicy, Wake,
};

const WAIT: Duration = Duration::from_secs(10);

fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {WAIT:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn bytes(byte: u8) -> Bytes {
    Bytes(vec![byte; 3])
}

fn policy() -> PublisherPolicy {
    PublisherPolicy {
        ring_bytes: MIN_RING,
        ..PublisherPolicy::default()
    }
}

fn path(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rsb_topic_uds_{tag}_{pid}_{serial}.sock"))
}

/// A server of `publisher` at a path of its own.
fn serve(publisher: &Publisher<Bytes>, tag: &str) -> (PathBuf, Arc<RpcServer>, RpcServerHandle) {
    let path = path(tag);
    let server = RpcServer::setup_unix_server(&path).unwrap();
    let handle = uds::serve(&server, publisher.as_binder()).unwrap();
    (path, server, handle)
}

fn subscriber_policy() -> SubscriberPolicy {
    SubscriberPolicy::default()
}

#[test]
fn a_subscriber_reads_over_a_socket_at_a_path() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let (path, _server, _handle) = serve(&publisher, "path");
    let mut subscriber =
        uds::subscribe::<Bytes>(Endpoint::Path(path), &subscriber_policy()).unwrap();
    assert_eq!(publisher.subscriber_count(), 1);
    publisher.publish(&bytes(1)).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(bytes(1)));
    let [info] = <[_; 1]>::try_from(publisher.stats().subscribers).unwrap();
    assert_eq!(info.uid, rustix::process::geteuid().as_raw());
    assert_eq!(info.pid, std::process::id() as i32);
}

#[test]
fn a_subscriber_reads_over_an_abstract_socket() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let name = format!("rsb_topic_uds_abstract_{}", std::process::id()).into_bytes();
    let server = RpcServer::setup_unix_server_abstract(&name).unwrap();
    let _handle = uds::serve(&server, publisher.as_binder()).unwrap();
    let mut subscriber =
        uds::subscribe::<Bytes>(Endpoint::Abstract(name), &subscriber_policy()).unwrap();
    publisher.publish(&bytes(2)).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(bytes(2)));
}

fn o_path(file: &std::path::Path) -> OwnedFd {
    use rustix::fs::{open, Mode, OFlags};
    open(file, OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).unwrap()
}

#[test]
fn a_subscriber_reads_through_a_descriptor_of_an_unlinked_file() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let (path, _server, _handle) = serve(&publisher, "fd");
    let address = o_path(&path);
    std::fs::remove_file(&path).unwrap();
    assert!(uds::subscribe::<Bytes>(Endpoint::Path(path), &subscriber_policy()).is_err());
    let mut subscriber =
        uds::subscribe::<Bytes>(Endpoint::Fd(address), &subscriber_policy()).unwrap();
    publisher.publish(&bytes(3)).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(bytes(3)));
}

#[test]
fn a_subscriber_on_fd_wakes_is_woken_over_a_socket_pair_it_passed() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let (path, _server, _handle) = serve(&publisher, "wake");
    let policy = SubscriberPolicy {
        wake: Wake::Fd,
        ..subscriber_policy()
    };
    let mut subscriber = uds::subscribe::<Bytes>(Endpoint::Path(path), &policy).unwrap();
    std::thread::scope(|scope| {
        let waited = scope.spawn(|| subscriber.recv_timeout(WAIT));
        std::thread::sleep(Duration::from_millis(50));
        publisher.publish(&bytes(4)).unwrap();
        assert_eq!(waited.join().unwrap(), Ok(bytes(4)));
    });
}

#[test]
fn a_dropped_subscriber_leaves_nothing_at_either_end() {
    if !alone("uds_tests::a_dropped_subscriber_leaves_nothing_at_either_end") {
        return;
    }
    let tasks = || std::fs::read_dir("/proc/self/task").unwrap().count();
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let (path, _server, _handle) = serve(&publisher, "gone");
    let before = (memfds(), tasks());
    let subscriber = uds::subscribe::<Bytes>(Endpoint::Path(path), &subscriber_policy()).unwrap();
    assert_eq!(publisher.subscriber_count(), 1);
    assert_ne!(memfds(), before.0);
    drop(subscriber);
    eventually("the subscription's end", || {
        publisher.subscriber_count() == 0
    });
    eventually("the session's threads and the mappings", || {
        (memfds(), tasks()) == before
    });
}

#[test]
fn a_publisher_that_ends_writes_its_end_record_over_the_socket() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let (path, _server, _handle) = serve(&publisher, "end");
    let mut subscriber =
        uds::subscribe::<Bytes>(Endpoint::Path(path), &subscriber_policy()).unwrap();
    publisher.publish(&bytes(5)).unwrap();
    drop(publisher);
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(bytes(5)));
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Closed(Closed::End))
    );
}

#[test]
fn a_server_that_ends_is_the_publishers_death() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let (path, _server, handle) = serve(&publisher, "died");
    let mut subscriber =
        uds::subscribe::<Bytes>(Endpoint::Path(path), &subscriber_policy()).unwrap();
    publisher.publish(&bytes(6)).unwrap();
    handle.stop_and_join().unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(bytes(6)));
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Closed(Closed::PublisherDied))
    );
}

/// `ISubscription.evicted` is oneway: it needs the session's incoming connection.
#[test]
fn a_subscriber_taken_out_hears_of_it_over_the_socket() {
    let policy = PublisherPolicy {
        backpressure: Some(Backpressure {
            publish_timeout: Duration::from_millis(50),
            ..Backpressure::default()
        }),
        ..policy()
    };
    let publisher = Publisher::<Bytes>::new(&policy).unwrap();
    let (path, _server, _handle) = serve(&publisher, "evict");
    let mut subscriber =
        uds::subscribe::<Bytes>(Endpoint::Path(path), &subscriber_policy()).unwrap();
    let value = Bytes(vec![0; 1024]);
    while !publisher.stats().subscribers.iter().any(|sub| sub.evicted) {
        publisher.publish(&value).unwrap();
    }
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "the subscriber is not told");
        match subscriber.recv_timeout(Duration::from_millis(100)) {
            Err(RecvError::Closed(Closed::Evicted)) => break,
            Ok(_) | Err(RecvError::Lagged(_) | RecvError::Timeout) => {}
            Err(other) => panic!("{other}"),
        }
    }
}

#[test]
fn the_publishers_uid_is_checked_when_it_is_asked_for() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let (path, _server, _handle) = serve(&publisher, "uid");
    let own = rustix::process::geteuid().as_raw();
    let options = |expect_uid| Options {
        expect_uid: Some(expect_uid),
        ..Options::default()
    };
    let subscribe = |expect_uid| {
        uds::subscribe_with::<Bytes>(
            Endpoint::Path(path.clone()),
            &subscriber_policy(),
            &options(expect_uid),
        )
    };
    let held = subscribe(own).unwrap();
    let refused = subscribe(own + 1).err().unwrap();
    assert_eq!(refused.exception_code(), ExceptionCode::Security);
    assert_eq!(publisher.subscriber_count(), 1);
    drop(held);
}

/// A listener that never accepts: the kernel completes a connect, and the handshake goes unheard.
#[test]
fn a_silent_listener_is_given_up_on_at_the_timeout() {
    let path = path("silent");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let options = Options {
        timeout: Some(Duration::from_millis(100)),
        ..Options::default()
    };
    let start = Instant::now();
    let refused =
        uds::subscribe_with::<Bytes>(Endpoint::Path(path.clone()), &subscriber_policy(), &options);
    assert_eq!(
        refused.err().map(|status| status.transaction_error()),
        Some(StatusCode::TimedOut)
    );
    assert!(start.elapsed() < WAIT);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_connect_to_no_listener_says_so() {
    let refused = uds::subscribe::<Bytes>(Endpoint::Path(path("none")), &subscriber_policy());
    assert_eq!(
        refused.err().map(|status| status.transaction_error()),
        Some(StatusCode::NameNotFound)
    );
}

/// The setup's deadline bounds the subscription call, and a later call has none.
#[test]
fn the_sessions_deadline_ends_with_the_setup() {
    let slow = Arc::new(AtomicBool::new(false));
    let hold = slow.clone();
    let policy = PublisherPolicy {
        authorize: Some(Authorize::new(move |_| {
            if hold.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(300));
            }
            true
        })),
        ..policy()
    };
    let publisher = Publisher::<Bytes>::new(&policy).unwrap();
    let (path, _server, _handle) = serve(&publisher, "deadline");
    let options = Options {
        timeout: Some(Duration::from_millis(100)),
        ..Options::default()
    };
    let subscribe = || {
        uds::subscribe_with::<Bytes>(Endpoint::Path(path.clone()), &subscriber_policy(), &options)
    };
    let subscriber = subscribe().unwrap();

    slow.store(true, Ordering::Relaxed);
    let refused = subscribe().err().unwrap();
    assert_eq!(refused.transaction_error(), StatusCode::TimedOut);

    let session = subscriber.session_guard::<SessionGuard>().unwrap();
    let topic: Strong<dyn ITopic> = session.0.get_root().unwrap().into_interface().unwrap();
    let (fd, _page) = ControlPage::create(MAX_PINS).unwrap();
    let request = SubscribeRequest {
        version: VERSION as i32,
        r#type: Bytes::type_name().to_owned(),
        token: Some(Token::detached()),
        control: Some(ParcelFileDescriptor::from(fd)),
        pins: MAX_PINS as i32,
        wake: WAKE_FUTEX,
        wakeFd: None,
    };
    topic.r#subscribe(&request).unwrap();
    assert!(!session.0.is_ended());
}

#[test]
fn a_binder_that_is_not_of_this_process_is_not_served() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let (path, _server, _handle) = serve(&publisher, "root");
    let session = uds::connect(Endpoint::Path(path), &Options::default()).unwrap();
    let remote = session.0.get_root().unwrap();
    let other = RpcServer::setup_unix_server(self::path("other")).unwrap();
    assert_eq!(
        uds::serve(&other, remote)
            .err()
            .map(|status| status.transaction_error()),
        Some(StatusCode::InvalidOperation)
    );
    drop(publisher.as_binder());
}
