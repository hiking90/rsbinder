// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The subscription call within one process: a [`Subscriber`] of a [`Publisher`] in the same
//! process reaches it without binder, so these need no binder device. The two-process paths are
//! in `tests/two_process.rs`.
//!
//! A test that counts this process's memfds runs again in a child process of its own
//! ([`alone`]), where no other test makes or drops one meanwhile.

use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::panic::AssertUnwindSafe;
use std::process::Command;
use std::sync::{Arc, Mutex};

use rsbinder::fmq::shm::{self, CreateOptions, Seals};
use rsbinder::fmq::Grantor;
use rsbinder::{
    BinderResult, CallingContext, ExceptionCode, Interface, ParcelFileDescriptor, SIBinder, Status,
    StatusCode,
};

use crate::aidl::ITopic::{BnTopic, ITopic, ERROR_CLOSED, ERROR_FULL, ERROR_VERSION};
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::aidl::TopicEndpoint::TopicEndpoint;
use crate::connect::Token;
use crate::layout::{self, ControlView, CLOSED, HEADER_OFFSET, MIN_RING, NO_FUTURE_WRITE, VERSION};
use crate::{
    Acceptor, Bytes, Closed, Payload, PublishError, Publisher, PublisherPolicy, RecvError, Start,
    Subscriber, SubscriberPolicy,
};

const ALONE: &str = "RSBINDER_TOPIC_ALONE";

/// Run test `name` again in a child process with no other test; `true` in that child.
fn alone(name: &str) -> bool {
    if std::env::var_os(ALONE).is_some() {
        return true;
    }
    let exe = std::env::current_exe().unwrap();
    let status = Command::new(exe)
        .args([name, "--exact", "--test-threads=1", "--nocapture"])
        .env(ALONE, "1")
        .status()
        .unwrap();
    assert!(status.success(), "{name} in a process of its own");
    false
}

/// This process's memfds: the fds open on one, and the mappings of one.
fn memfds() -> (usize, usize) {
    let fds = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| target.to_string_lossy().starts_with("/memfd:"))
        .count();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    (
        fds,
        maps.lines().filter(|line| line.contains("/memfd:")).count(),
    )
}

fn policy() -> PublisherPolicy {
    PublisherPolicy {
        ring_bytes: MIN_RING,
        ..PublisherPolicy::default()
    }
}

fn subscribe<T: Payload>(topic: &SIBinder, start: Start) -> BinderResult<Subscriber<T>> {
    let policy = SubscriberPolicy {
        start,
        ..SubscriberPolicy::default()
    };
    Subscriber::request(topic, &policy)
}

fn bytes(byte: u8) -> Bytes {
    Bytes(vec![byte; 3])
}

/// A sealed memfd of `size` zero bytes with `words` written in.
fn sealed(size: u64, words: &[(u64, &[u8; 4])], seals: Seals) -> OwnedFd {
    let options = CreateOptions::new("rsbinder-topic-test").populate(true);
    let file = File::from(shm::create(size, &options).unwrap());
    for (at, word) in words {
        file.write_at(*word, *at).unwrap();
    }
    let fd = OwnedFd::from(file);
    shm::add_seals(fd.as_fd(), seals).unwrap();
    fd
}

/// A control page as `ControlPage::create(0)` writes it, but for the words given.
fn control(magic: &[u8; 4], version: u32, pins: u32, size: u64) -> OwnedFd {
    let words = [
        (0, magic),
        (4, &version.to_ne_bytes()),
        (12, &pins.to_ne_bytes()),
    ];
    sealed(
        size,
        &words,
        Seals::GROW | Seals::SHRINK | Seals::FUTURE_WRITE,
    )
}

/// A request as `Subscriber::request` makes it, with `control` and `token` given.
fn request(control: OwnedFd, token: SIBinder) -> SubscribeRequest {
    SubscribeRequest {
        version: VERSION as i32,
        r#type: Bytes::type_name().to_owned(),
        token: Some(token),
        control: Some(ParcelFileDescriptor::from(control)),
        pins: 0,
    }
}

fn token() -> SIBinder {
    rsbinder::Binder::new(Token).as_binder()
}

fn service_specific(result: BinderResult<impl Sized>) -> i32 {
    match result {
        Err(status) if status.exception_code() == ExceptionCode::ServiceSpecific => {
            status.service_specific_error()
        }
        Err(status) => panic!("{status}"),
        Ok(_) => panic!("accepted"),
    }
}

fn code(result: BinderResult<impl Sized>) -> StatusCode {
    match result {
        Err(status) => status.transaction_error(),
        Ok(_) => panic!("accepted"),
    }
}

#[test]
fn a_subscriber_in_the_publishers_process_receives_and_each_end_sees_the_other_go() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let topic = publisher.as_binder();
    let mut subscriber = subscribe::<Bytes>(&topic, Start::Latest).unwrap();
    assert_eq!(publisher.subscriber_count(), 1);
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    publisher.publish(&bytes(1)).unwrap();
    assert_eq!(publisher.publish_all(&[bytes(2), bytes(3)]), Ok(2));
    for byte in 1..=3 {
        assert_eq!(subscriber.try_recv(), Ok(bytes(byte)));
    }
    // A dropped subscriber is gone from the count; no death notification is involved.
    drop(subscriber);
    assert_eq!(publisher.subscriber_count(), 0);

    let mut subscriber = subscribe::<Bytes>(&topic, Start::Latest).unwrap();
    publisher.publish(&bytes(4)).unwrap();
    // The binder outlives the publisher, as one the service manager holds does.
    drop(publisher);
    assert_eq!(subscriber.try_recv(), Ok(bytes(4)));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Closed(Closed::End)));
    let late = subscribe::<Bytes>(&topic, Start::Latest);
    assert_eq!(service_specific(late), ERROR_CLOSED);
}

#[test]
fn each_start_takes_the_record_it_names_first() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let topic = publisher.as_binder();
    let mut last = subscribe::<Bytes>(&topic, Start::Last).unwrap();
    publisher
        .publish_all(&[bytes(0), bytes(1), bytes(2)])
        .unwrap();
    // `Last` on a topic with no record yet starts where `Latest` would.
    assert_eq!(last.try_recv(), Ok(bytes(0)));

    let mut latest = subscribe::<Bytes>(&topic, Start::Latest).unwrap();
    let mut last = subscribe::<Bytes>(&topic, Start::Last).unwrap();
    let mut oldest = subscribe::<Bytes>(&topic, Start::Oldest).unwrap();
    assert_eq!(latest.try_recv(), Err(RecvError::Empty));
    assert_eq!(last.try_recv(), Ok(bytes(2)));
    assert_eq!(oldest.try_recv(), Ok(bytes(0)));
    publisher.publish(&bytes(3)).unwrap();
    assert_eq!(latest.try_recv(), Ok(bytes(3)));
    assert_eq!(last.try_recv(), Ok(bytes(3)));
    assert_eq!(oldest.try_recv(), Ok(bytes(1)));
}

#[test]
fn a_request_the_publisher_cannot_take_is_refused_for_what_is_wrong_with_it() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let acceptor = publisher.acceptor();
    let page = || control(b"RSTC", VERSION, 0, 0x80);

    let mut other = request(page(), token());
    other.version = VERSION as i32 + 1;
    assert_eq!(service_specific(acceptor.accept(&other)), ERROR_VERSION);
    let other = subscribe::<crate::typed_tests::Sample>(&publisher.as_binder(), Start::Latest);
    assert_eq!(code(other), StatusCode::BadType);
    let mut other = request(page(), token());
    other.pins = 1;
    assert_eq!(code(acceptor.accept(&other)), StatusCode::BadValue);
    // A binder of this process that came in no call of this thread's: the publisher's own.
    let foreign = request(page(), publisher.as_binder());
    assert_eq!(code(acceptor.accept(&foreign)), StatusCode::BadValue);
    let foreign = request(page(), token());
    assert_eq!(code(acceptor.accept(&foreign)), StatusCode::BadValue);
    assert_eq!(publisher.subscriber_count(), 0);
}

#[test]
fn a_control_page_that_is_not_what_the_request_says_is_refused() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let acceptor = publisher.acceptor();
    let seals = Seals::GROW | Seals::SHRINK | Seals::FUTURE_WRITE;
    let null = OwnedFd::from(File::open("/dev/null").unwrap());
    let unsealed = shm::create(0x80, &CreateOptions::new("unsealed").populate(true)).unwrap();
    for (fd, why) in [
        (null, "not a memfd"),
        (unsealed, "no write seal"),
        (sealed(0x80, &[], seals), "no magic"),
        (control(b"RSTP", VERSION, 0, 0x80), "the topic's magic"),
        (control(b"RSTC", VERSION + 1, 0, 0x80), "another version"),
        (control(b"RSTC", VERSION, 1, 0x80), "another pin count"),
        (control(b"RSTC", VERSION, 0, 0x88), "another size"),
    ] {
        // The request is made as `Subscriber::request` makes it, from this thread.
        let token = token();
        let _asking = crate::connect::Asking::new(&token);
        let refused = acceptor.accept(&request(fd, token.clone()));
        assert_eq!(code(refused), StatusCode::BadValue, "{why}");
    }
    let token = token();
    let _asking = crate::connect::Asking::new(&token);
    let page = control(b"RSTC", VERSION, 0, 0x80);
    assert!(acceptor.accept(&request(page, token.clone())).is_ok());
    assert_eq!(publisher.subscriber_count(), 1);
}

#[test]
fn a_full_publisher_refuses_until_a_subscriber_goes() {
    let policy = PublisherPolicy {
        max_subscribers: 2,
        ..policy()
    };
    let publisher = Publisher::<Bytes>::new(&policy).unwrap();
    let topic = publisher.as_binder();
    let first = subscribe::<Bytes>(&topic, Start::Latest).unwrap();
    let _second = subscribe::<Bytes>(&topic, Start::Latest).unwrap();
    let third = subscribe::<Bytes>(&topic, Start::Latest);
    assert_eq!(service_specific(third), ERROR_FULL);
    drop(first);
    // The refused one left nothing; the dropped one is taken out before the count is checked.
    assert!(subscribe::<Bytes>(&topic, Start::Latest).is_ok());

    // Every subscription here is this process's uid.
    let policy = PublisherPolicy {
        max_per_uid: 1,
        ..policy
    };
    let publisher = Publisher::<Bytes>::new(&policy).unwrap();
    let _first = subscribe::<Bytes>(&publisher.as_binder(), Start::Latest).unwrap();
    let second = subscribe::<Bytes>(&publisher.as_binder(), Start::Latest);
    assert_eq!(service_specific(second), ERROR_FULL);
}

#[test]
fn a_closed_publisher_takes_no_subscription() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let mut subscriber = subscribe::<Bytes>(&publisher.as_binder(), Start::Latest).unwrap();
    publisher.close(&Status::new_service_specific_error(9, None));
    let late = subscribe::<Bytes>(&publisher.as_binder(), Start::Latest);
    assert_eq!(service_specific(late), ERROR_CLOSED);
    assert!(matches!(
        subscriber.try_recv(),
        Err(RecvError::Closed(Closed::Status(status))) if status.service_specific_error() == 9
    ));
    assert_eq!(publisher.publish(&bytes(1)), Err(PublishError::Closed));
}

/// An `ITopic` of the application's: admits one uid, through an [`Acceptor`].
struct Guarded {
    uid: u32,
    acceptor: Acceptor,
}

impl Interface for Guarded {}

impl ITopic for Guarded {
    fn r#subscribe(&self, request: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        if CallingContext::default().uid != self.uid {
            return Err(Status::from(ExceptionCode::Security));
        }
        self.acceptor.accept(request)
    }
}

#[test]
fn an_itopic_of_the_application_refuses_or_admits_through_its_acceptor() {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    // Outside a transaction, this process's own.
    let uid = CallingContext::default().uid;
    let guard = |uid| {
        let acceptor = publisher.acceptor();
        BnTopic::new_binder(Guarded { uid, acceptor }).as_binder()
    };
    let refused = subscribe::<Bytes>(&guard(uid.wrapping_add(1)), Start::Latest);
    assert_eq!(
        refused.err().map(|status| status.exception_code()),
        Some(ExceptionCode::Security)
    );
    let mut admitted = subscribe::<Bytes>(&guard(uid), Start::Latest).unwrap();
    publisher.publish(&bytes(5)).unwrap();
    assert_eq!(admitted.try_recv(), Ok(bytes(5)));
    let topic = guard(uid);
    drop(publisher);
    assert_eq!(
        service_specific(subscribe::<Bytes>(&topic, Start::Latest)),
        ERROR_CLOSED
    );
}

/// An `ITopic` that answers with whatever endpoint it is given, keeping the control page.
struct Forged {
    endpoint: Box<dyn Fn() -> TopicEndpoint + Send + Sync>,
    control: Arc<Mutex<Option<ControlView>>>,
}

impl Interface for Forged {}

impl ITopic for Forged {
    fn r#subscribe(&self, request: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        let fd = request.control.as_ref().unwrap().as_fd();
        *self.control.lock().unwrap() = Some(ControlView::map(fd, 0).unwrap());
        Ok((self.endpoint)())
    }
}

/// An endpoint as a publisher of `capacity` bytes of ring sends it, over `fd`.
fn endpoint(fd: OwnedFd, capacity: u64) -> TopicEndpoint {
    TopicEndpoint {
        version: VERSION as i32,
        ring: layout::descriptor(fd, capacity).try_into().unwrap(),
        headerOffset: HEADER_OFFSET as i32,
        start: 0,
        startIndex: 0,
    }
}

#[test]
fn a_reply_the_subscriber_cannot_use_is_refused_and_its_control_page_closed() {
    let (good, _stored) = layout::create_topic(MIN_RING).unwrap();
    let good = Arc::new(good);
    let capacity = MIN_RING as u64;
    let seals = Seals::GROW | Seals::SHRINK | Seals::FUTURE_WRITE;
    let dup = move |fd: &Arc<OwnedFd>| fd.try_clone().unwrap();
    type Make = Box<dyn Fn() -> TopicEndpoint + Send + Sync>;
    // What the subscriber refuses with: an `ITopic` code, or `None` for `BadValue`.
    let cases: Vec<(Make, Option<i32>, &str)> = vec![
        (
            Box::new({
                let good = good.clone();
                move || TopicEndpoint {
                    version: VERSION as i32 + 1,
                    ..endpoint(dup(&good), capacity)
                }
            }),
            Some(ERROR_VERSION),
            "another version",
        ),
        (
            Box::new({
                let good = good.clone();
                move || TopicEndpoint {
                    headerOffset: 0,
                    ..endpoint(dup(&good), capacity)
                }
            }),
            None,
            "another header offset",
        ),
        (
            Box::new({
                let good = good.clone();
                move || {
                    let mut wrong = endpoint(dup(&good), capacity);
                    wrong.ring.grantors[2].offset = 0x48;
                    wrong
                }
            }),
            None,
            "grantors elsewhere",
        ),
        (
            Box::new(move || endpoint(sealed(0x40 + capacity, &[], seals), capacity)),
            None,
            "no magic",
        ),
        (
            Box::new(move || {
                let options = CreateOptions::new("unsealed").populate(true);
                endpoint(shm::create(0x40 + capacity, &options).unwrap(), capacity)
            }),
            None,
            "no write seal",
        ),
        (
            Box::new(|| {
                let (large, _stored) = layout::create_topic(2 * MIN_RING).unwrap();
                endpoint(large, 2 * MIN_RING as u64)
            }),
            None,
            "a ring above the subscriber's bound",
        ),
    ];
    let policy = SubscriberPolicy {
        max_ring_bytes: MIN_RING,
        ..SubscriberPolicy::default()
    };
    for (make, refused, why) in cases {
        let control = Arc::new(Mutex::new(None));
        let forged = Forged {
            endpoint: make,
            control: control.clone(),
        };
        let topic = BnTopic::new_binder(forged).as_binder();
        let refusal = Subscriber::<Bytes>::request(&topic, &policy).map(drop);
        match refused {
            Some(error) => assert_eq!(service_specific(refusal), error, "{why}"),
            None => assert_eq!(code(refusal), StatusCode::BadValue, "{why}"),
        }
        let view = control.lock().unwrap().take().unwrap();
        assert_eq!(view.state().load() & CLOSED, CLOSED, "{why}");
    }
    let forged = Forged {
        endpoint: Box::new(move || endpoint(dup(&good), capacity)),
        control: Arc::new(Mutex::new(None)),
    };
    let topic = BnTopic::new_binder(forged).as_binder();
    assert!(Subscriber::<Bytes>::request(&topic, &policy).is_ok());
}

/// A value whose encoding panics.
struct Explosive;

impl Payload for Explosive {
    fn type_name() -> &'static str {
        "explosive"
    }

    fn encode(&self, _: &mut Vec<u8>) -> rsbinder::Result<()> {
        panic!("encode");
    }

    fn decode(_: &[u8]) -> rsbinder::Result<Self> {
        Ok(Self)
    }
}

#[test]
fn a_panic_in_encode_leaves_the_publisher_working() {
    let publisher = Arc::new(Publisher::<Explosive>::new(&policy()).unwrap());
    let topic = publisher.as_binder();
    let panicked = std::thread::spawn({
        let publisher = publisher.clone();
        move || publisher.publish(&Explosive)
    })
    .join();
    assert!(panicked.is_err());
    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| publisher.publish_all([&Explosive])));
    assert!(caught.is_err());
    // The lock was never taken with the encoding running, so nothing is poisoned.
    let _subscriber = subscribe::<Explosive>(&topic, Start::Latest).unwrap();
    assert_eq!(publisher.publish_all([]), Ok(0));
    assert_eq!(publisher.subscriber_count(), 1);
}

#[test]
fn without_the_future_write_seal_nothing_is_made_and_nothing_is_left() {
    if !alone("connect_tests::without_the_future_write_seal_nothing_is_made_and_nothing_is_left") {
        return;
    }
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let before = memfds();
    NO_FUTURE_WRITE.set(true);
    assert_eq!(
        Publisher::<Bytes>::new(&policy()).err(),
        Some(rsbinder::fmq::Error::Unsupported)
    );
    let refused = subscribe::<Bytes>(&publisher.as_binder(), Start::Latest);
    assert_eq!(
        refused.err().map(|status| status.exception_code()),
        Some(ExceptionCode::UnsupportedOperation)
    );
    NO_FUTURE_WRITE.set(false);
    assert_eq!(memfds(), before);
    assert_eq!(publisher.subscriber_count(), 0);
}

#[test]
fn a_refused_or_ended_subscription_leaves_no_memfd_behind() {
    if !alone("connect_tests::a_refused_or_ended_subscription_leaves_no_memfd_behind") {
        return;
    }
    let policy = PublisherPolicy {
        max_subscribers: 1,
        ..policy()
    };
    let publisher = Publisher::<Bytes>::new(&policy).unwrap();
    let topic = publisher.as_binder();
    let before = memfds();
    let held = subscribe::<Bytes>(&topic, Start::Latest).unwrap();
    // Each side maps the other's object; the subscriber also maps its own control page.
    let (fds, maps) = before;
    assert_eq!(memfds(), (fds, maps + 3));
    let full = subscribe::<Bytes>(&topic, Start::Latest);
    assert_eq!(service_specific(full), ERROR_FULL);
    let wrong = subscribe::<crate::typed_tests::Sample>(&topic, Start::Latest);
    assert_eq!(code(wrong), StatusCode::BadType);
    assert_eq!(memfds(), (fds, maps + 3));
    drop(held);
    assert_eq!(publisher.subscriber_count(), 0);
    assert_eq!(memfds(), before);
}

#[test]
fn an_rpc_binder_is_not_subscribed_to() {
    use rsbinder::rpc::{RpcServer, RpcSession};

    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let name = format!("rsbinder-topic-rpc-{}", std::process::id());
    let server = RpcServer::setup_unix_server_abstract(name.as_bytes()).unwrap();
    server.set_root(publisher.as_binder()).unwrap();
    std::thread::spawn(move || server.run());
    let session = RpcSession::setup_unix_client_abstract(name.as_bytes()).unwrap();
    let topic = session.get_root().unwrap();
    assert!(topic.is_remote() && topic.as_proxy().is_none());
    let refused = subscribe::<Bytes>(&topic, Start::Latest);
    assert_eq!(code(refused), StatusCode::InvalidOperation);
    assert_eq!(publisher.subscriber_count(), 0);
}

#[test]
fn a_publisher_is_shared_between_threads_and_a_subscriber_moved_to_one() {
    fn shared<T: Send + Sync>() {}
    fn moved<T: Send>() {}
    shared::<Publisher<Bytes>>();
    shared::<Acceptor>();
    moved::<Subscriber<Bytes>>();
}

#[test]
fn grantors_name_the_unsynchronized_queue_of_bytes() {
    let at = |offset, extent| Grantor {
        fd_index: 0,
        offset,
        extent,
    };
    assert_eq!(layout::grantors(4096), [at(0, 8), at(8, 8), at(0x40, 4096)]);
}
