// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The name layer within one process (plan 15 §5.7c), through a [`Resolver`] of the test's
//! own whose registrations the test makes. The service manager's resolver is in
//! `tests/two_process.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rsbinder::{BinderResult, ExceptionCode, Interface, SIBinder, Status};

use crate::aidl::ITopic::{BnTopic, ITopic, ERROR_FULL};
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::aidl::TopicEndpoint::TopicEndpoint;
use crate::layout::MIN_RING;
use crate::{
    Acceptor, Authorize, Bytes, Closed, Disconnected, NamedSubscriber, Publisher, PublisherPolicy,
    RecvError, Resolver, Subscriber, SubscriberPolicy, Watch,
};

/// The bound of every wait a test makes, so that a broken subscriber fails rather than hangs.
const WAIT: Duration = Duration::from_secs(10);
const NAME: &str = "test.topic";

type Callback = Arc<dyn Fn(SIBinder) + Send + Sync>;

/// Names held in this process: `give` registers a binder and calls the watchers at once.
#[derive(Clone, Default)]
struct Names(Arc<Mutex<Book>>);

#[derive(Default)]
struct Book {
    given: HashMap<String, SIBinder>,
    watchers: Vec<(usize, String, Callback)>,
    next: usize,
}

impl Names {
    fn give(&self, name: &str, binder: SIBinder) {
        let mut book = self.0.lock().unwrap();
        book.given.insert(name.to_owned(), binder.clone());
        let callbacks: Vec<Callback> = book
            .watchers
            .iter()
            .filter(|(_, watched, _)| watched == name)
            .map(|(_, _, callback)| callback.clone())
            .collect();
        drop(book);
        for callback in callbacks {
            callback(binder.clone());
        }
    }

    fn watchers(&self) -> usize {
        self.0.lock().unwrap().watchers.len()
    }
}

impl Resolver for Names {
    fn advertise(&self, name: &str, topic: SIBinder) -> BinderResult<()> {
        self.give(name, topic);
        Ok(())
    }

    fn watch(
        &self,
        name: &str,
        on_registered: Box<dyn Fn(SIBinder) + Send + Sync>,
    ) -> BinderResult<Watch> {
        let callback: Callback = Arc::from(on_registered);
        let mut book = self.0.lock().unwrap();
        let id = book.next;
        book.next += 1;
        book.watchers.push((id, name.to_owned(), callback.clone()));
        let given = book.given.get(name).cloned();
        drop(book);
        if let Some(binder) = given {
            callback(binder);
        }
        Ok(Watch::new(Unwatch(self.clone(), id)))
    }
}

struct Unwatch(Names, usize);

impl Drop for Unwatch {
    fn drop(&mut self) {
        let mut book = self.0 .0.lock().unwrap();
        book.watchers.retain(|(id, _, _)| *id != self.1);
    }
}

fn policy() -> PublisherPolicy {
    PublisherPolicy {
        ring_bytes: MIN_RING,
        ..PublisherPolicy::default()
    }
}

fn bytes(byte: u8) -> Bytes {
    Bytes(vec![byte; 3])
}

fn advertise(names: &Names, policy: &PublisherPolicy) -> Publisher<Bytes> {
    Publisher::advertise_with(NAME, policy, names).unwrap()
}

fn subscribe(names: &Names) -> NamedSubscriber<Bytes> {
    NamedSubscriber::subscribe_with(NAME, &SubscriberPolicy::default(), names).unwrap()
}

/// A receive with the test's bound.
fn recv(subscriber: &mut NamedSubscriber<Bytes>) -> Result<Bytes, RecvError> {
    subscriber.recv_timeout(WAIT)
}

fn connected(subscriber: &NamedSubscriber<Bytes>) {
    assert!(subscriber.wait_connected(WAIT), "connected within {WAIT:?}");
}

#[test]
fn a_subscriber_of_a_name_connects_when_a_publisher_takes_it_later() {
    let names = Names::default();
    let mut subscriber = subscribe(&names);
    assert!(!subscriber.wait_connected(Duration::from_millis(50)));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    let publisher = advertise(&names, &policy());
    connected(&subscriber);
    publisher.publish(&bytes(1)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
}

#[test]
fn a_receive_waits_for_the_connection_and_then_for_the_record() {
    let names = Names::default();
    let mut subscriber = subscribe(&names);
    let late = names.clone();
    let publishing = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        let publisher = advertise(&late, &policy());
        // After the subscription: the first connection starts at the policy's `Latest`.
        let deadline = Instant::now() + WAIT;
        while publisher.subscriber_count() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        publisher.publish(&bytes(2)).unwrap();
        publisher
    });
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    drop(publishing.join().unwrap());
}

#[test]
fn an_ended_topic_is_one_disconnect_and_the_next_publisher_is_read_from_its_first_record() {
    let names = Names::default();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    first.publish(&bytes(1)).unwrap();
    first.publish(&bytes(2)).unwrap();
    drop(first);
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)))
    );
    // Published before the name is given: a reconnection starts at the oldest record.
    let second = Publisher::<Bytes>::new(&policy()).unwrap();
    second.publish(&bytes(7)).unwrap();
    second.publish(&bytes(8)).unwrap();
    names.give(NAME, second.as_binder());
    assert_eq!(recv(&mut subscriber), Ok(bytes(7)));
    assert_eq!(recv(&mut subscriber), Ok(bytes(8)));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
}

#[test]
fn the_same_binder_registered_again_keeps_the_connection() {
    let names = Names::default();
    let publisher = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    publisher.publish(&bytes(1)).unwrap();
    names.give(NAME, publisher.as_binder());
    publisher.publish(&bytes(2)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    assert_eq!(publisher.subscriber_count(), 1);
}

#[test]
fn another_binder_for_the_name_is_read_after_the_records_of_the_old_one() {
    let names = Names::default();
    let old = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    old.publish(&bytes(1)).unwrap();
    old.publish(&bytes(2)).unwrap();
    // The old publisher lives on, as one whose death notice is still on its way.
    let new = Publisher::<Bytes>::new(&policy()).unwrap();
    new.publish(&bytes(9)).unwrap();
    names.give(NAME, new.as_binder());
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    // Committed after the replacement was seen: not read.
    old.publish(&bytes(3)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
    old.publish(&bytes(4)).unwrap();
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    assert_eq!(old.subscriber_count(), 0);
    assert_eq!(new.subscriber_count(), 1);
}

#[test]
fn a_replacement_seen_while_waiting_ends_the_wait() {
    let names = Names::default();
    let old = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    let late = names.clone();
    let replacing = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        let new = advertise(&late, &policy());
        new.publish(&bytes(4)).unwrap();
        new
    });
    let started = Instant::now();
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert!(started.elapsed() < WAIT);
    let _new = replacing.join().unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(4)));
    drop(old);
}

#[test]
fn a_full_publisher_is_told_once_and_asked_again_until_a_place_is_free() {
    let names = Names::default();
    let full = PublisherPolicy {
        max_subscribers: 1,
        ..policy()
    };
    let publisher = advertise(&names, &full);
    let place = Subscriber::<Bytes>::request(&publisher.as_binder(), &SubscriberPolicy::default());
    let place = place.unwrap();
    let mut subscriber = subscribe(&names);
    match recv(&mut subscriber) {
        Err(RecvError::Disconnected(Disconnected::Rejected(status))) => {
            assert_eq!(status.service_specific_error(), ERROR_FULL)
        }
        other => panic!("{other:?}"),
    }
    // Retried at 1 s, 2 s: told only the first time.
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(2500)),
        Err(RecvError::Timeout)
    );
    drop(place);
    connected(&subscriber);
    publisher.publish(&bytes(5)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(5)));
}

#[test]
fn a_retry_moves_at_once_to_another_publisher_of_the_name() {
    let names = Names::default();
    let full = PublisherPolicy {
        max_subscribers: 0,
        ..policy()
    };
    let _refusing = advertise(&names, &full);
    let mut subscriber = subscribe(&names);
    assert!(matches!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Rejected(_)))
    ));
    // The first retry is 1 s away; the new binder is asked now.
    let admitting = advertise(&names, &policy());
    assert!(subscriber.wait_connected(Duration::from_millis(500)));
    admitting.publish(&bytes(4)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(4)));
}

#[test]
fn a_caller_the_policy_refuses_is_told_once_and_not_asked_again() {
    let names = Names::default();
    let asked = Arc::new(AtomicUsize::new(0));
    let count = asked.clone();
    let refusing = PublisherPolicy {
        authorize: Some(Authorize::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            false
        })),
        ..policy()
    };
    let publisher = advertise(&names, &refusing);
    let mut subscriber = subscribe(&names);
    match recv(&mut subscriber) {
        Err(RecvError::Disconnected(Disconnected::Rejected(status))) => {
            assert_eq!(status.exception_code(), ExceptionCode::Security)
        }
        other => panic!("{other:?}"),
    }
    names.give(NAME, publisher.as_binder());
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(1500)),
        Err(RecvError::Timeout)
    );
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    // Another publisher of the name is asked.
    let admitting = advertise(&names, &policy());
    connected(&subscriber);
    admitting.publish(&bytes(6)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(6)));
}

#[test]
fn a_closed_publisher_is_waited_past_without_a_notice() {
    let names = Names::default();
    let closed = advertise(&names, &policy());
    closed.close(&Status::from(ExceptionCode::None));
    let mut subscriber = subscribe(&names);
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(300)),
        Err(RecvError::Timeout)
    );
    let next = advertise(&names, &policy());
    connected(&subscriber);
    next.publish(&bytes(7)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(7)));
}

#[test]
fn wait_connected_returns_when_connected_and_times_out_without_a_publisher() {
    let names = Names::default();
    let subscriber = subscribe(&names);
    let started = Instant::now();
    assert!(!subscriber.wait_connected(Duration::from_millis(200)));
    assert!(started.elapsed() >= Duration::from_millis(200));
    let _publisher = advertise(&names, &policy());
    connected(&subscriber);
}

#[test]
fn an_interrupt_ends_the_wait_for_a_connection_and_the_wait_on_one() {
    let names = Names::default();
    let mut subscriber = subscribe(&names);
    let interrupter = subscriber.interrupter();
    let later = interrupter.clone();
    let interrupting = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        later.interrupt();
    });
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
    interrupting.join().unwrap();
    let publisher = advertise(&names, &policy());
    connected(&subscriber);
    // An interrupt before the wait ends the next one.
    interrupter.interrupt();
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
    let later = interrupter.clone();
    let interrupting = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        later.interrupt();
    });
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
    interrupting.join().unwrap();
    publisher.publish(&bytes(8)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(8)));
}

#[test]
fn an_interrupt_before_the_connection_is_taken_ends_the_wait_on_it() {
    let names = Names::default();
    let mut subscriber = subscribe(&names);
    let _publisher = advertise(&names, &policy());
    connected(&subscriber);
    subscriber.interrupter().interrupt();
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(50)),
        Err(RecvError::Timeout)
    );
}

/// An `ITopic` whose subscription call waits until the test lets it go on.
struct Hanging {
    acceptor: Acceptor,
    asked: mpsc::SyncSender<()>,
    go: Mutex<mpsc::Receiver<()>>,
    answered: Arc<AtomicUsize>,
}

impl Interface for Hanging {}

impl ITopic for Hanging {
    fn r#subscribe(&self, request: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        self.asked.send(()).unwrap();
        self.go.lock().unwrap().recv().unwrap();
        let reply = self.acceptor.accept(request);
        self.answered.fetch_add(1, Ordering::SeqCst);
        reply
    }
}

#[test]
fn a_subscriber_dropped_during_a_call_that_hangs_returns_and_the_late_reply_is_closed() {
    let names = Names::default();
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    let (asked, was_asked) = mpsc::sync_channel(1);
    let (go, gone_on) = mpsc::sync_channel(1);
    let answered = Arc::new(AtomicUsize::new(0));
    let hanging = Hanging {
        acceptor: publisher.acceptor(),
        asked,
        go: Mutex::new(gone_on),
        answered: answered.clone(),
    };
    names.give(NAME, BnTopic::new_binder(hanging).as_binder());
    let subscriber = subscribe(&names);
    was_asked.recv_timeout(WAIT).unwrap();
    let started = Instant::now();
    drop(subscriber);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(names.watchers(), 0);
    go.send(()).unwrap();
    // The thread takes the reply, finds the subscriber gone and drops the subscription.
    let deadline = Instant::now() + WAIT;
    loop {
        if answered.load(Ordering::SeqCst) == 1 && publisher.subscriber_count() == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the late subscription was not closed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// AC-15.2.33: a name subscriber's stats add up its connections.
#[test]
fn a_name_subscribers_stats_count_every_connection() {
    let names = Names::default();
    let mut subscriber = subscribe(&names);
    assert_eq!(subscriber.stats(), crate::SubscriberStats::default());
    let first = advertise(&names, &policy());
    connected(&subscriber);
    first.publish(&bytes(1)).unwrap();
    first.publish(&bytes(2)).unwrap();
    drop(first);
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    let ended = Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)));
    assert_eq!(recv(&mut subscriber), ended);
    let second = advertise(&names, &policy());
    connected(&subscriber);
    second.publish(&bytes(3)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(3)));
    let stats = subscriber.stats();
    assert_eq!((stats.received, stats.connections, stats.lost), (3, 2, 0));
}

#[test]
fn a_name_subscriber_and_its_interrupter_move_between_threads() {
    fn moved<T: Send>() {}
    fn shared<T: Send + Sync>() {}
    moved::<NamedSubscriber<Bytes>>();
    shared::<crate::Interrupter>();
    shared::<Authorize>();
}
