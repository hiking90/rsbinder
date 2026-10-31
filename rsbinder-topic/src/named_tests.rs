// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The name layer within one process (plan 15 §5.7c), through a [`Resolver`] of the test's
//! own whose registrations the test makes. The service manager's resolver is in
//! `tests/two_process.rs`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rsbinder::{BinderResult, DeathRecipient, ExceptionCode, Interface, SIBinder, Status};

use crate::aidl::ITopic::{BnTopic, ITopic, ERROR_FULL};
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::aidl::TopicEndpoint::TopicEndpoint;
use crate::layout::{self, Stored, HEADER_OFFSET, MIN_RING, VERSION};
use crate::named::{
    BEFORE_DRAIN_ENDS, BEFORE_INTERRUPT, BEFORE_LOOKING, BEFORE_REPLACE, BETWEEN_LOOKS,
};
use crate::ring::{Kind, Stores, Writer, BEFORE_JUDGING};
use rsbinder::fmq::shm::Mapping;
use std::os::fd::{AsRawFd, OwnedFd};

use crate::{
    wait_any, Acceptor, Advertisement, Authorize, Backpressure, Bytes, Closed, Disconnected,
    NamedState, NamedSubscriber, Publisher, PublisherPolicy, RecvError, Resolver, Start,
    Subscriber, SubscriberPolicy, Wait, Wake, Watch,
};

/// The bound of every wait a test makes, so that a broken subscriber fails rather than hangs.
const WAIT: Duration = Duration::from_secs(10);
const NAME: &str = "test.topic";

type Callback = Arc<dyn Fn(SIBinder) + Send + Sync>;
type Hook = Arc<dyn Fn() + Send + Sync>;

/// Names held in this process: `give` registers a binder and calls the watchers at once.
#[derive(Clone, Default)]
struct Names(Arc<Mutex<Book>>);

#[derive(Default)]
struct Book {
    given: HashMap<String, SIBinder>,
    watchers: Vec<(usize, String, Callback)>,
    next: usize,
    /// With [`Names::looking`]: the lookups the watches were asked.
    lookups: Option<usize>,
    /// Run by a lookup before it reads the name.
    before_lookup: Option<Hook>,
    /// Run by `watch` after its call with the binder the name has.
    after_first_call: Option<Hook>,
}

impl Names {
    /// Names whose watches have a lookup (`Watch::with_lookup`) that reads `given`.
    fn looking() -> Self {
        let names = Self::default();
        names.0.lock().unwrap().lookups = Some(0);
        names
    }

    /// Register `binder` as a notice lost on the way would: no watcher is called.
    fn withhold(&self, name: &str, binder: SIBinder) {
        let mut book = self.0.lock().unwrap();
        book.given.insert(name.to_owned(), binder);
    }

    fn before_lookup(&self, hook: impl Fn() + Send + Sync + 'static) {
        self.0.lock().unwrap().before_lookup = Some(Arc::new(hook));
    }

    fn after_first_call(&self, hook: impl Fn() + Send + Sync + 'static) {
        self.0.lock().unwrap().after_first_call = Some(Arc::new(hook));
    }

    fn lookups(&self) -> usize {
        self.0.lock().unwrap().lookups.expect("names that look")
    }

    fn look(&self, name: &str) -> Option<SIBinder> {
        let hook = self.0.lock().unwrap().before_lookup.clone();
        if let Some(hook) = hook {
            hook();
        }
        let mut book = self.0.lock().unwrap();
        *book.lookups.as_mut().expect("names that look") += 1;
        book.given.get(name).cloned()
    }

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
    fn advertise(&self, name: &str, topic: SIBinder) -> BinderResult<Advertisement> {
        self.give(name, topic);
        Ok(Advertisement::none())
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
        let looks = book.lookups.is_some();
        let after_first_call = book.after_first_call.clone();
        drop(book);
        if let Some(binder) = given {
            callback(binder);
        }
        if let Some(hook) = after_first_call {
            hook();
        }
        let unwatch = Unwatch(self.clone(), id);
        if !looks {
            return Ok(Watch::new(unwatch));
        }
        let (names, name) = (self.clone(), name.to_owned());
        Ok(Watch::with_lookup(unwatch, move || Ok(names.look(&name))))
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
    assert_eq!(subscriber.remembered(), 0, "an ended topic's publisher");
    // Published before the name is given: a reconnection starts at the oldest record.
    let second = Publisher::<Bytes>::new(&policy()).unwrap();
    second.publish(&bytes(7)).unwrap();
    second.publish(&bytes(8)).unwrap();
    names.give(NAME, second.as_binder());
    assert_eq!(recv(&mut subscriber), Ok(bytes(7)));
    assert_eq!(recv(&mut subscriber), Ok(bytes(8)));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    assert_eq!(subscriber.remembered(), 1);
}

/// A topic ended with a status other than ok refuses every later subscription too: forgotten.
#[test]
fn a_topic_ended_with_an_error_status_forgets_its_publisher() {
    let names = Names::default();
    let publisher = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    publisher.close(&Status::from(ExceptionCode::IllegalState));
    assert!(matches!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Ended(
            Closed::Status(_)
        )))
    ));
    assert_eq!(subscriber.remembered(), 0, "an ended topic's publisher");
}

/// It lives on and may take the name again, so a connection to it would start at `Latest`.
#[test]
fn a_publisher_whose_memory_failed_a_check_stays_remembered() {
    let names = Names::default();
    let (memory, stored) = layout::create_topic(MIN_RING).unwrap();
    names.give(NAME, BnTopic::new_binder(ByHand(memory)).as_binder());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    assert_eq!(subscriber.remembered(), 1);
    // Off the record grid: a `write` no publisher that follows the protocol stores.
    stored.store_write(4);
    assert_eq!(
        subscriber.try_recv(),
        Err(RecvError::Disconnected(Disconnected::Ended(
            Closed::Corrupted
        )))
    );
    assert_eq!(subscriber.remembered(), 1);
    // Nothing is asked again while the name has that publisher: the state says so at each ask.
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    assert_eq!(subscriber.state(), NamedState::Corrupted);
    assert_eq!(subscriber.state(), NamedState::Corrupted);
}

#[test]
fn a_dead_resolver_closes_the_subscriber_once_its_connection_ends() {
    let names = Names::default();
    let publisher = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    subscriber.resolver_dies();
    // The connection goes on.
    assert_eq!(subscriber.state(), NamedState::Connected);
    publisher.publish(&bytes(1)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    publisher.close(&Status::from(ExceptionCode::None));
    let ended = Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)));
    assert_eq!(recv(&mut subscriber), ended);
    let closed = Err(RecvError::Closed(Closed::ResolverDied));
    assert_eq!(recv(&mut subscriber), closed);
    assert_eq!(subscriber.try_recv(), closed);
    assert_eq!(subscriber.state(), NamedState::Closed(Closed::ResolverDied));
    let asked = Instant::now();
    assert!(!subscriber.wait_connected(WAIT));
    assert!(asked.elapsed() < WAIT, "returned at once");
}

#[test]
fn a_dead_resolver_closes_a_subscriber_without_a_connection_at_once() {
    let names = Names::default();
    let mut subscriber = subscribe_on(&names, NAME, Wake::Fd);
    subscriber.resolver_dies();
    assert!(!subscriber.prepare_wait(), "a receive returns now");
    let closed = Err(RecvError::Closed(Closed::ResolverDied));
    assert_eq!(recv(&mut subscriber), closed);
    // A publisher that comes later is not connected to.
    let publisher = advertise(&names, &policy());
    publisher.publish(&bytes(2)).unwrap();
    assert!(!subscriber.wait_connected(Duration::from_millis(300)));
    assert_eq!(recv(&mut subscriber), closed);
}

/// It lives on, so it stays remembered: given the name again, it is read from `Latest`.
#[test]
fn a_publisher_that_shut_its_wake_socket_down_is_connected_to_again_at_latest() {
    let names = Names::default();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe_on(&names, NAME, Wake::Fd);
    connected(&subscriber);
    first.publish(&bytes(1)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    let local = subscriber.connection_local().expect("a connection");
    let sender = local.words().fd.as_ref().unwrap().sender();
    // The publisher's end shut down: the polled end reads end of file, which a wait takes as death.
    rustix::net::shutdown(sender, rustix::net::Shutdown::Write).unwrap();
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Ended(
            Closed::PublisherDied
        )))
    );
    assert_eq!(subscriber.remembered(), 1);
    names.give(NAME, first.as_binder());
    connected(&subscriber);
    first.publish(&bytes(2)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
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

/// The name given to another binder and back to the one connected: nothing was replaced.
#[test]
fn a_name_given_back_to_the_connected_publisher_keeps_the_connection() {
    let names = Names::default();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    // Taken by a receive: a replacement interrupts the connection's wait.
    first.publish(&bytes(1)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    let other = Publisher::<Bytes>::new(&policy()).unwrap();
    names.give(NAME, other.as_binder());
    first.publish(&bytes(2)).unwrap();
    // Seen as a replacement: read up to what is committed now.
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    names.give(NAME, first.as_binder());
    first.publish(&bytes(3)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(3)));
    // The replacement's interrupt of the wait is not the caller's.
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(100)),
        Err(RecvError::Timeout)
    );
    // The caller's, made while the name was away, is.
    names.give(NAME, other.as_binder());
    subscriber.interrupter().interrupt();
    names.give(NAME, first.as_binder());
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(100)),
        Err(RecvError::Timeout)
    );
    assert_eq!(first.subscriber_count(), 1);
    assert_eq!(other.subscriber_count(), 0);
}

/// The name back to a publisher after its drain ended: read on from the reconnection.
#[test]
fn a_publisher_connected_to_again_returns_no_record_the_first_connection_returned() {
    connected_to_again(Start::Latest);
}

/// As above, from a start that would return the records the first connection returned.
#[test]
fn a_publisher_connected_to_again_starts_at_latest_whatever_the_policy_start() {
    connected_to_again(Start::Last);
    connected_to_again(Start::Oldest);
}

fn connected_to_again(start: Start) {
    let names = Names::default();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe_at(&names, Wake::Futex, start);
    connected(&subscriber);
    first.publish(&bytes(1)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    let other = Publisher::<Bytes>::new(&policy()).unwrap();
    other.publish(&bytes(5)).unwrap();
    names.give(NAME, other.as_binder());
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    // A publisher not connected to before: read from its oldest record, here its newest too.
    assert_eq!(recv(&mut subscriber), Ok(bytes(5)));
    names.give(NAME, first.as_binder());
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    connected(&subscriber);
    first.publish(&bytes(2)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    assert_eq!(first.subscriber_count(), 1);
    assert_eq!(other.subscriber_count(), 0);
}

/// A publisher whose topic memory the test writes, subscribed to at position 0 and index 0.
struct ByHand(OwnedFd);

impl Interface for ByHand {}

impl ITopic for ByHand {
    fn r#subscribe(&self, _: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        let ring = layout::descriptor(self.0.try_clone().unwrap(), MIN_RING as u64);
        Ok(TopicEndpoint {
            version: VERSION as i32,
            ring: ring.try_into().unwrap(),
            headerOffset: HEADER_OFFSET as i32,
            start: 0,
            startIndex: 0,
            pool: None,
        })
    }
}

/// The header of an item with no payload: a record of 8 bytes.
fn empty_item(index: u32) -> [u8; 8] {
    let mut header = [0; 8];
    header[4..].copy_from_slice(&index.to_le_bytes());
    header
}

/// `ByHand` memory as its publisher's [`Writer`] stores it, while a test stores beside it.
struct Shared(Rc<Stored<Mapping>>);

impl Stores for Shared {
    fn capacity(&self) -> u64 {
        self.0.capacity()
    }

    fn store_tail(&self, position: u64) {
        self.0.store_tail(position);
    }

    fn store_region_end(&self, position: u64) {
        self.0.store_region_end(position);
    }

    fn store_frontier(&self, position: u64) {
        self.0.store_frontier(position);
    }

    fn store_write(&self, position: u64) {
        self.0.store_write(position);
    }

    fn store_last(&self, position: u64) {
        self.0.store_last(position);
    }

    fn store_half(&self, position: u64) {
        self.0.store_half(position);
    }

    fn bump_seq(&self) -> u32 {
        self.0.bump_seq()
    }

    fn wake_seq(&self) {
        self.0.wake_seq();
    }

    fn store_ring(&self, position: u64, src: &[u8]) {
        self.0.store_ring(position, src);
    }
}

/// The old publisher of `ByHand` memory: items with no payload, item `n` at `8 * n`.
type Old = Rc<RefCell<Writer<Shared>>>;

/// Publish `records` items with no payload.
fn publish_items(old: &Old, records: u64) {
    for _ in 0..records {
        old.borrow_mut().put(Kind::Item, &[]).unwrap();
    }
}

/// Publish until `frontier` is past `position` by a ring: the record there is written over.
fn lap(old: &Old, position: u64) {
    let past = position + MIN_RING as u64 + 8;
    let written = old.borrow().position();
    publish_items(old, past.saturating_sub(written) / 8);
}

/// A name subscriber of `ByHand` memory, its old publisher, and the new one that took the name.
struct Draining {
    subscriber: NamedSubscriber<Bytes>,
    stored: Rc<Stored<Mapping>>,
    writer: Old,
    old: SIBinder,
    _new: Publisher<Bytes>,
}

/// Items 0 to `records - 1`, a new publisher for the name, item 0 read: the drain ends after them.
fn draining(names: &Names, records: u64) -> Draining {
    let (memory, stored) = layout::create_topic(MIN_RING).unwrap();
    let stored = Rc::new(stored);
    let writer = Rc::new(RefCell::new(Writer::new(Shared(stored.clone()))));
    publish_items(&writer, records);
    let old = BnTopic::new_binder(ByHand(memory)).as_binder();
    names.give(NAME, old.clone());
    let mut subscriber = subscribe(names);
    connected(&subscriber);
    let new = Publisher::<Bytes>::new(&policy()).unwrap();
    new.publish(&bytes(9)).unwrap();
    names.give(NAME, new.as_binder());
    assert_eq!(subscriber.try_recv(), Ok(Bytes(Vec::new())));
    Draining {
        subscriber,
        stored,
        writer,
        old,
        _new: new,
    }
}

/// Items 0 to 3 at 0 to 32, a new publisher for the name, item 0 read: the drain ends at 32.
fn draining_four(names: &Names) -> Draining {
    draining(names, 4)
}

/// A receive, after which a drain's reader is not past the drain's end.
fn drained(subscriber: &mut NamedSubscriber<Bytes>) -> Result<Bytes, RecvError> {
    let received = subscriber.try_recv();
    if let Some((position, end)) = subscriber.drain_position() {
        assert!(position <= end, "read to {position}, past the end at {end}");
    }
    received
}

/// Every value received from now on, until the next result that is no value.
fn values(subscriber: &mut NamedSubscriber<Bytes>) -> (u64, Result<Bytes, RecvError>) {
    let mut returned = 0;
    loop {
        match drained(subscriber) {
            Ok(_) => returned += 1,
            other => return (returned, other),
        }
    }
}

/// Run `hook` on this thread before each judgment of the ring, as a publisher writing then would.
fn judging(hook: impl FnMut() + 'static) {
    BEFORE_JUDGING.set(Some(Box::new(hook)));
}

/// While `on` holds: lap the reader at `from` at the next judgment, then write a record at each.
fn chasing(writer: &Old, from: u64, on: &Rc<Cell<bool>>) {
    let (writer, on, first) = (writer.clone(), on.clone(), Cell::new(true));
    judging(move || {
        if !on.get() {
            return;
        }
        match first.replace(false) {
            true => lap(&writer, from),
            false => publish_items(&writer, 1),
        }
    });
}

/// [`chasing`] with `half` kept at `tail`, as a broken layout may: a lap moves a drain one record.
fn chasing_with_half_at_tail(drain: &Draining, from: u64, on: &Rc<Cell<bool>>) {
    let (writer, stored, on, first) = (
        drain.writer.clone(),
        drain.stored.clone(),
        on.clone(),
        Cell::new(true),
    );
    judging(move || {
        if !on.get() {
            return;
        }
        match first.replace(false) {
            true => lap(&writer, from),
            false => publish_items(&writer, 1),
        }
        // Items of 8 bytes fill the ring: `tail` is a ring behind `write`.
        stored.store_half(writer.borrow().position() - MIN_RING as u64);
    });
}

/// Lapped a ring past item 5, item 4 at the end written over too: the lower bound, one, then on.
#[test]
fn a_drain_lapped_at_every_look_reports_a_loss_and_goes_on_to_the_new_publisher() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    lap(&drain.writer, 40);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(recv(&mut drain.subscriber), Ok(bytes(9)));
    assert_eq!(drain.subscriber.stats().lost, 1);
}

/// Lapped by a ring past item 2: the drain resumes at `tail`, item 3, which is read; 1 and 2 lost.
#[test]
fn a_drain_lapped_short_of_its_end_counts_every_record_it_did_not_return() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    lap(&drain.writer, 16);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(2)));
    assert_eq!(drained(&mut drain.subscriber), Ok(Bytes(Vec::new())));
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(recv(&mut drain.subscriber), Ok(bytes(9)));
    assert_eq!(drain.subscriber.stats().lost, 2);
}

/// Items 0 to 499, the drain resumed at `tail`, item 2, then two receives each overtaken: `half`.
fn turned_to_half(names: &Names) -> Draining {
    let mut drain = draining(names, 500);
    lap(&drain.writer, 8);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    // Written over once: at `tail` again, item 3.
    publish_items(&drain.writer, 1);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    publish_items(&drain.writer, 1);
    // `half` is item 260, the first at or past 4128 - 2048.
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(257)));
    drain
}

/// An old publisher that writes a record before each receive: the drain reads on from `half`.
#[test]
fn a_drain_whose_resume_at_tail_is_overtaken_reads_on_from_half() {
    let names = Names::default();
    let mut drain = turned_to_half(&names);
    let mut returned = 0;
    loop {
        publish_items(&drain.writer, 1);
        match drained(&mut drain.subscriber) {
            Ok(_) => returned += 1,
            Err(RecvError::Disconnected(Disconnected::Replaced)) => break,
            other => panic!("{other:?}"),
        }
    }
    // Items 260 to 499, the rest before the end; 1 to 259 counted.
    assert_eq!((returned, drain.subscriber.stats().lost), (240, 259));
    assert_eq!(recv(&mut drain.subscriber), Ok(bytes(9)));
}

/// One record between the receives after a resume at `tail`, then none: read on from `tail`.
#[test]
fn a_drain_overtaken_once_at_tail_reads_every_record_left_once_the_publisher_stops() {
    let names = Names::default();
    let mut drain = draining(&names, 500);
    lap(&drain.writer, 8);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    publish_items(&drain.writer, 1);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    // Items 3 to 499; 1 and 2 counted.
    let (returned, then) = values(&mut drain.subscriber);
    assert_eq!(returned, 497);
    assert_eq!(then, Err(RecvError::Disconnected(Disconnected::Replaced)));
    assert_eq!(drain.subscriber.stats().lost, 2);
}

/// A drain turned to `half`, the name back and taken again: the second drain resumes at `tail`.
#[test]
fn a_second_drain_of_one_connection_resumes_at_tail_again() {
    let names = Names::default();
    let mut drain = turned_to_half(&names);
    names.give(NAME, drain.old.clone());
    // Live: items 260 to 515, at 4128.
    let (returned, then) = values(&mut drain.subscriber);
    assert_eq!((returned, then), (256, Err(RecvError::Empty)));
    lap(&drain.writer, 4128);
    names.give(NAME, drain._new.as_binder());
    // At `tail`, 4136: item 516 lost, not those up to `half`; then every one to the end, 1028.
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    let (returned, then) = values(&mut drain.subscriber);
    assert_eq!(returned, 512);
    assert_eq!(then, Err(RecvError::Disconnected(Disconnected::Replaced)));
    assert_eq!(drain.subscriber.stats().lost, 260);
}

/// Item 1 overtaken while copied, by a ring past the end: its header moves the drain nowhere.
#[test]
fn a_drain_lapped_past_its_end_counts_a_loss() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    let writer = drain.writer.clone();
    let mut once = Some(());
    judging(move || {
        if once.take().is_some() {
            lap(&writer, 40);
        }
    });
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    assert_eq!(drain.subscriber.drain_position(), Some((8, 32)));
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.stats().lost, 1);
}

/// The drain counted items 1 to 3, then the name came back: item 4 is next, with no loss again.
#[test]
fn a_drain_that_counted_its_loss_does_not_count_it_again_when_the_name_comes_back() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    lap(&drain.writer, 24);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(3)));
    names.give(NAME, drain.old.clone());
    let (returned, then) = values(&mut drain.subscriber);
    assert_eq!(then, Err(RecvError::Empty));
    // Item 4 and every one after it.
    let published = u64::from(drain.writer.borrow().next_index());
    assert_eq!(returned, published - 4);
    assert_eq!(drain.subscriber.stats().lost, 3);
}

/// The drain counted one, item 4 overwritten too, then the name came back: the rest once.
#[test]
fn a_drain_that_counted_a_lower_bound_counts_only_the_rest_when_the_name_comes_back() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    lap(&drain.writer, 40);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    names.give(NAME, drain.old.clone());
    // Resumed at `half`: what lies before it is lost, item 1 counted once.
    let Err(RecvError::Lagged(rest)) = drained(&mut drain.subscriber) else {
        panic!("the rest was not counted");
    };
    let (returned, then) = values(&mut drain.subscriber);
    assert_eq!(then, Err(RecvError::Empty));
    let published = u64::from(drain.writer.borrow().next_index());
    assert_eq!(
        1 + returned + 1 + rest,
        published,
        "item 0, the rest read, the losses"
    );
    assert_eq!(drain.subscriber.stats().lost, 1 + rest);
}

/// Run `hook` on this thread once, as a drain ends, before the lock that decides how.
fn as_the_drain_ends(hook: impl FnOnce() + 'static) {
    let mut hook = Some(hook);
    BEFORE_DRAIN_ENDS.set(Some(Box::new(move || {
        if let Some(hook) = hook.take() {
            hook();
        }
    })));
}

/// Give the name back to `binder` as the drain ends, past the last look of its receive.
fn back_as_the_drain_ends(names: &Names, binder: SIBinder) {
    let names = names.clone();
    as_the_drain_ends(move || names.give(NAME, binder));
}

/// Read to its end, then the name back before the drain ends: the connection stays, live.
#[test]
fn a_name_that_comes_back_as_its_drain_ends_keeps_the_connection() {
    let names = Names::default();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe_on(&names, NAME, Wake::Fd);
    connected(&subscriber);
    first.publish(&bytes(1)).unwrap();
    let other = Publisher::<Bytes>::new(&policy()).unwrap();
    other.publish(&bytes(9)).unwrap();
    names.give(NAME, other.as_binder());
    assert_eq!(subscriber.try_recv(), Ok(bytes(1)));
    back_as_the_drain_ends(&names, first.as_binder());
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    assert!(subscriber.prepare_wait(), "a replacement left to return at");
    subscriber.finish_wait();
    first.publish(&bytes(2)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    assert_eq!(first.subscriber_count(), 1);
    assert_eq!(other.subscriber_count(), 0);
    // Taken again: a drain of its own, to what is committed now.
    first.publish(&bytes(3)).unwrap();
    names.give(NAME, other.as_binder());
    assert_eq!(recv(&mut subscriber), Ok(bytes(3)));
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
    assert_eq!(subscriber.stats().lost, 0);
}

/// Items 1 to 3 counted, the name back before the drain ends, item 4 lapped: a live reader's lap.
#[test]
fn a_drain_that_counted_its_loss_keeps_the_connection_when_the_name_comes_back_as_it_ends() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    lap(&drain.writer, 24);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(3)));
    back_as_the_drain_ends(&names, drain.old.clone());
    // Past the drain's end, where a live reader resumes and a drain's would not.
    lap(&drain.writer, 32);
    let Err(RecvError::Lagged(rest)) = drained(&mut drain.subscriber) else {
        panic!("the lap past item 4 was not counted");
    };
    let (returned, then) = values(&mut drain.subscriber);
    assert_eq!(then, Err(RecvError::Empty));
    let published = u64::from(drain.writer.borrow().next_index());
    assert_eq!(
        4 + rest + returned,
        published,
        "items 0 to 3, the losses after them, the rest read"
    );
    assert_eq!(drain.subscriber.stats().lost, 3 + rest);
}

/// `try_latest` skips what it does not return, in a drain lapped at every look too.
#[test]
fn a_latest_drain_lapped_at_every_look_ends_without_a_loss() {
    let (memory, stored) = layout::create_topic(MIN_RING).unwrap();
    stored.store_ring(0, &[0xff; 8]);
    stored.store_frontier(!7);
    stored.store_write(8);
    let names = Names::default();
    names.give(NAME, BnTopic::new_binder(ByHand(memory)).as_binder());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    assert_eq!(subscriber.try_latest(), Err(RecvError::Empty));
    let new = Publisher::<Bytes>::new(&policy()).unwrap();
    new.publish(&bytes(9)).unwrap();
    names.give(NAME, new.as_binder());
    assert_eq!(
        subscriber.try_latest(),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
    assert_eq!(subscriber.stats().lost, 0);
}

/// Run `hook` on this thread between a drain's look that gave up lapped and the look after it.
fn between_looks(hook: impl FnMut() + 'static) {
    BETWEEN_LOOKS.set(Some(Box::new(hook)));
}

/// Sixteen items, a drain from item 1 chased, `half` at `tail`, through the looks `on` holds.
fn chased_drain(names: &Names, on: &Rc<Cell<bool>>) -> Draining {
    let drain = draining(names, 16);
    chasing_with_half_at_tail(&drain, 8, on);
    drain
}

/// Lapped at each of its five resumes, the first look gives up at item 5; the next reads on.
#[test]
fn a_drain_lapped_at_one_look_looks_again_and_reads_on() {
    let names = Names::default();
    let on = Rc::new(Cell::new(true));
    let mut drain = chased_drain(&names, &on);
    {
        let on = on.clone();
        between_looks(move || on.set(false));
    }
    // Resumed at `tail`, item 6: items 1 to 5 lost; 6 to 15 read.
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(5)));
    let (returned, then) = values(&mut drain.subscriber);
    assert_eq!(returned, 10);
    assert_eq!(then, Err(RecvError::Disconnected(Disconnected::Replaced)));
    assert_eq!(drain.subscriber.stats().lost, 5);
}

/// Overtaken twice where it resumed at `tail`, `half` past its end: it ends, the chase stopped.
#[test]
fn a_drain_turned_to_half_past_its_end_ends_though_the_chase_stopped() {
    let names = Names::default();
    let on = Rc::new(Cell::new(true));
    let mut drain = draining(&names, 16);
    chasing(&drain.writer, 8, &on);
    {
        let on = on.clone();
        between_looks(move || on.set(false));
    }
    // Items 1 to 15, counted by item 16 at the end, which stays whole.
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(15)));
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.stats().lost, 15);
}

/// A drain lapped at two looks ends at the second: a third, which would read on, is not made.
#[test]
fn a_drain_lapped_at_two_looks_ends_at_the_second() {
    let names = Names::default();
    let (on, between) = (Rc::new(Cell::new(true)), Rc::new(Cell::new(0)));
    let mut drain = chased_drain(&names, &on);
    {
        let (on, between) = (on.clone(), between.clone());
        between_looks(move || {
            between.set(between.get() + 1);
            on.set(between.get() < 2);
        });
    }
    // Items 1 to 15, counted by item 16 at the end, which stays whole.
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(15)));
    assert_eq!(between.get(), 1);
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.stats().lost, 15);
}

/// A drain after the name came back counts its lapped looks from none, not from the last drain's.
#[test]
fn a_second_drain_of_one_connection_counts_its_lapped_looks_anew() {
    let names = Names::default();
    let on = Rc::new(Cell::new(true));
    let mut drain = chased_drain(&names, &on);
    {
        let on = on.clone();
        between_looks(move || on.set(false));
    }
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(5)));
    // Back to the old one: item 6 is a record of the connection, not of a drain.
    names.give(NAME, drain.old.clone());
    assert_eq!(drained(&mut drain.subscriber), Ok(Bytes(Vec::new())));
    names.give(NAME, drain._new.as_binder());
    // The second drain, at item 7: lapped at its first look as the first was, then read on.
    on.set(true);
    chasing_with_half_at_tail(&drain, 56, &on);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(5)));
    let (_, then) = values(&mut drain.subscriber);
    assert_eq!(then, Err(RecvError::Disconnected(Disconnected::Replaced)));
    assert_eq!(drain.subscriber.stats().lost, 10);
}

/// Lapped at each resume onto item 5, the last before the end: items 1 to 5, each counted once.
#[test]
fn a_drain_lapped_onto_its_last_record_counts_each_record_before_its_end_once() {
    let names = Names::default();
    let on = Rc::new(Cell::new(true));
    let mut drain = draining(&names, 6);
    chasing(&drain.writer, 8, &on);
    {
        let on = on.clone();
        between_looks(move || on.set(false));
    }
    // Item 6, at the end, stays whole: the count is exact.
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(5)));
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.stats().lost, 5);
}

/// The old publisher's process died while the drain could not read on: what it missed is counted.
#[test]
fn a_drain_whose_old_publisher_died_counts_what_it_missed() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    lap(&drain.writer, 24);
    let death = drain.subscriber.connection_death().expect("a connection");
    death.binder_died(&SIBinder::downgrade(&drain.old));
    // Each look gives up, which the death turns into `Closed`: still a look that gave up.
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(3)));
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.remembered(), 0, "a dead publisher");
    assert_eq!(recv(&mut drain.subscriber), Ok(bytes(9)));
    assert_eq!(drain.subscriber.stats().lost, 3);
}

/// A drain with no record left to look at forgets a publisher whose death notice ran.
#[test]
fn a_drain_at_its_end_forgets_a_publisher_whose_death_notice_ran() {
    let names = Names::default();
    let mut drain = draining(&names, 1);
    let death = drain.subscriber.connection_death().expect("a connection");
    death.binder_died(&SIBinder::downgrade(&drain.old));
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.remembered(), 0, "a dead publisher");
}

/// The same drain with `DIED` raised alone, as a wake socket that hung up raises it: remembered.
#[test]
fn a_drain_whose_old_publisher_hung_up_keeps_it_remembered() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    lap(&drain.writer, 24);
    let local = drain.subscriber.connection_local().expect("a connection");
    local.raise(crate::wait::DIED);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(3)));
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.remembered(), 1);
}

/// The old topic ended before the name was taken: its drain reads that end, which forgets it.
#[test]
fn a_drain_that_reads_the_end_of_the_old_topic_forgets_its_publisher() {
    let names = Names::default();
    let old = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    old.publish(&bytes(1)).unwrap();
    drop(old);
    let new = Publisher::<Bytes>::new(&policy()).unwrap();
    new.publish(&bytes(9)).unwrap();
    names.give(NAME, new.as_binder());
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(subscriber.remembered(), 0, "an ended topic's publisher");
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
    assert_eq!(subscriber.remembered(), 1);
}

/// A drain that finds the old memory failing a check does not forget it: it lives on.
#[test]
fn a_drain_whose_old_publisher_failed_a_check_keeps_it_remembered() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    // Item 1 longer than the ring: a length no publisher that follows the protocol writes.
    let mut header = empty_item(1);
    header[..4].copy_from_slice(&(u32::MAX >> 2).to_le_bytes());
    drain.stored.store_ring(8, &header);
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.remembered(), 1);
}

/// Taken out between two looks of a drain, the first of which gave up: taken out, not drained.
#[test]
fn a_drain_taken_out_between_its_looks_is_taken_out() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    lap(&drain.writer, 40);
    let local = drain
        .subscriber
        .connection_local()
        .expect("the connection taken");
    between_looks(move || {
        local.raise(crate::wait::EVICTED);
    });
    let evicted = Err(RecvError::Closed(Closed::Evicted));
    assert_eq!(drained(&mut drain.subscriber), evicted);
    assert_eq!(drained(&mut drain.subscriber), evicted);
    assert_eq!(drain.subscriber.stats().lost, 0);
}

/// A second drain while the record a lower bound counted is still owed: it is not counted again.
#[test]
fn a_second_drain_owing_a_record_a_lower_bound_counted_counts_it_no_more() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    lap(&drain.writer, 40);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    names.give(NAME, drain.old.clone());
    // From now on the old publisher writes a ring before each judgment: no look reads on.
    let writer = drain.writer.clone();
    judging(move || publish_items(&writer, MIN_RING as u64 / 8 + 1));
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Empty));
    names.give(NAME, drain._new.as_binder());
    // The record at the second drain's end is written over too: a lower bound, of none.
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.stats().lost, 1);
}

/// A publisher that rewrites the index of the record the drain stays on: it ends at the third.
#[test]
fn a_drain_held_in_place_by_a_publisher_that_breaks_the_protocol_ends() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    let stored = &drain.stored;
    stored.store_ring(8, &empty_item(100));
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(99)));
    stored.store_ring(8, &empty_item(200));
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(100)));
    stored.store_ring(8, &empty_item(300));
    // The end not written: one more, the lower bound.
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    // Written now: the drain, which counted its losses, ends without counting again.
    stored.store_ring(32, &empty_item(4));
    stored.store_frontier(40);
    stored.store_write(40);
    stored.store_last(32);
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(recv(&mut drain.subscriber), Ok(bytes(9)));
}

/// Each receive held in place followed by one that reads: the drain goes on to its end.
#[test]
fn a_drain_that_reads_after_each_receive_held_in_place_goes_on() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    let stored = &drain.stored;
    for (position, index, lost) in [(8, 50, 49), (16, 60, 9), (24, 70, 9)] {
        stored.store_ring(position, &empty_item(index));
        assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(lost)));
        assert_eq!(drained(&mut drain.subscriber), Ok(Bytes(Vec::new())));
    }
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.stats().lost, 67);
}

/// Lapped by a ring past item 3, `tail` at the drain's end: items 1 to 3 are lost, not item 4.
#[test]
fn a_drain_that_would_resume_past_its_end_counts_only_the_records_before_it() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    // Item 4 on, committed after the replacement was seen; item 4 stays whole.
    lap(&drain.writer, 24);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(3)));
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.stats().lost, 3);
}

/// A record of a kind this reader steps over, right before the drain's end: none after it is read.
#[test]
fn a_drain_that_steps_to_its_end_reads_no_record_after_it() {
    let (memory, stored) = layout::create_topic(MIN_RING).unwrap();
    stored.store_ring(0, &empty_item(0));
    stored.store_ring(8, &empty_item(1));
    // Kind 3, of a later layout, with no payload.
    stored.store_ring(16, &[0, 0, 0, 0xc0, 2, 0, 0, 0]);
    stored.store_frontier(24);
    stored.store_last(16);
    stored.store_write(24);
    let names = Names::default();
    names.give(NAME, BnTopic::new_binder(ByHand(memory)).as_binder());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    let new = Publisher::<Bytes>::new(&policy()).unwrap();
    new.publish(&bytes(9)).unwrap();
    names.give(NAME, new.as_binder());
    assert_eq!(subscriber.try_recv(), Ok(Bytes(Vec::new())));
    // Item 3, committed after the drain's end was taken.
    stored.store_ring(24, &empty_item(3));
    stored.store_frontier(32);
    stored.store_write(32);
    stored.store_last(24);
    assert_eq!(drained(&mut subscriber), Ok(Bytes(Vec::new())));
    assert_eq!(
        drained(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
}

/// Item 3, the last before the end, overtaken while copied, and `half` past the end: one lost.
#[test]
fn a_drain_whose_last_record_is_overtaken_while_copied_counts_it_alone() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    assert_eq!(drained(&mut drain.subscriber), Ok(Bytes(Vec::new())));
    assert_eq!(drained(&mut drain.subscriber), Ok(Bytes(Vec::new())));
    let stored = &drain.stored;
    // Items 4 and 5, after the end; item 4 overwritten too, as the `frontier` says.
    stored.store_ring(32, &empty_item(4));
    stored.store_ring(40, &empty_item(5));
    stored.store_frontier(40 + stored.capacity());
    stored.store_write(48);
    stored.store_tail(40);
    stored.store_last(40);
    stored.store_half(40);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.stats().lost, 1);
    assert_eq!(recv(&mut drain.subscriber), Ok(bytes(9)));
}

/// Item 1 overtaken while copied, and `half` at item 2: item 1 alone is lost, the rest are read.
#[test]
fn a_drain_record_overtaken_while_copied_is_lost_alone_and_the_drain_reads_on() {
    let names = Names::default();
    let mut drain = draining_four(&names);
    let stored = &drain.stored;
    stored.store_frontier(16 + stored.capacity());
    stored.store_tail(16);
    stored.store_half(16);
    assert_eq!(drained(&mut drain.subscriber), Err(RecvError::Lagged(1)));
    assert_eq!(drained(&mut drain.subscriber), Ok(Bytes(Vec::new())));
    assert_eq!(drained(&mut drain.subscriber), Ok(Bytes(Vec::new())));
    assert_eq!(
        drained(&mut drain.subscriber),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(drain.subscriber.stats().lost, 1);
}

/// A latest drain that steps over records past its end until it gives up: it looks no more.
#[test]
fn a_latest_drain_that_gave_up_past_its_end_looks_no_more() {
    let (memory, stored) = layout::create_topic(MIN_RING).unwrap();
    for index in 0..3 {
        stored.store_ring(8 * u64::from(index), &empty_item(index));
    }
    stored.store_frontier(24);
    // Not the newest: the first latest look returns item 1 and leaves item 2 short of the end.
    stored.store_last(8);
    stored.store_write(24);
    let names = Names::default();
    names.give(NAME, BnTopic::new_binder(ByHand(memory)).as_binder());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    let new = Publisher::<Bytes>::new(&policy()).unwrap();
    new.publish(&bytes(9)).unwrap();
    names.give(NAME, new.as_binder());
    assert_eq!(subscriber.try_latest(), Ok(Bytes(Vec::new())));
    // After the end: records of kind 3 at 24 to 56, which a look steps over, then item 8.
    for at in (24..64).step_by(8) {
        stored.store_ring(at, &[0, 0, 0, 0xc0, 0, 0, 0, 0]);
    }
    stored.store_ring(64, &empty_item(8));
    stored.store_frontier(72);
    stored.store_write(72);
    stored.store_last(24);
    assert_eq!(
        subscriber.try_latest(),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
}

/// An old publisher that laps the drain between receives: each record before the end, once.
#[test]
fn a_drain_lapped_between_receives_returns_or_counts_each_record_before_its_end_once() {
    let value = |seq: u64| Bytes(seq.to_le_bytes().to_vec());
    let seq = |bytes: Bytes| u64::from_le_bytes(bytes.0.try_into().unwrap());
    // Records of 16 bytes: the ring holds this many, and the end of the drain is after them.
    let ring = MIN_RING as u64 / 16;
    for burst in [1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 128, 129, 200] {
        let names = Names::default();
        let old = advertise(&names, &policy());
        let mut subscriber = subscribe(&names);
        connected(&subscriber);
        for at in 0..ring {
            old.publish(&value(at)).unwrap();
        }
        let new = Publisher::<Bytes>::new(&policy()).unwrap();
        new.publish(&value(u64::MAX)).unwrap();
        names.give(NAME, new.as_binder());
        // The receive that takes the end, at `ring` records.
        let mut returned = vec![seq(subscriber.try_recv().unwrap())];
        let (mut lost, mut next) = (0, ring);
        loop {
            // Up to a ring past the end: the record there stays whole.
            for _ in 0..burst {
                if next < 2 * ring {
                    old.publish(&value(next)).unwrap();
                    next += 1;
                }
            }
            match drained(&mut subscriber) {
                Ok(bytes) => returned.push(seq(bytes)),
                Err(RecvError::Lagged(records)) => lost += records,
                Err(RecvError::Disconnected(Disconnected::Replaced)) => break,
                other => panic!("burst {burst}: {other:?}"),
            }
            // A publisher that follows the protocol moves the drain at each receive.
            assert_eq!(subscriber.drain_stalls(), 0, "burst {burst}");
        }
        if burst == 1 {
            // Never lapped: every record before the end is read.
            assert_eq!((returned.len() as u64, lost), (ring, 0));
        }
        assert!(
            returned.windows(2).all(|two| two[0] < two[1]),
            "burst {burst}"
        );
        assert!(
            returned.iter().all(|at| *at < ring),
            "burst {burst}: {returned:?}"
        );
        assert_eq!(returned.len() as u64 + lost, ring, "burst {burst}");
        assert_eq!(recv(&mut subscriber).map(seq), Ok(u64::MAX));
    }
}

/// `registered` for another binder, held past its unlock while the name comes back: no interrupt.
#[test]
fn a_replacement_whose_wake_lands_after_the_name_came_back_ends_no_receive() {
    let names = Names::default();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    first.publish(&bytes(1)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    let other = Publisher::<Bytes>::new(&policy()).unwrap();
    let (paused, held) = (mpsc::channel(), mpsc::channel::<()>());
    let replacing = {
        let (names, binder) = (names.clone(), other.as_binder());
        let (paused, held) = (paused.0, held.1);
        std::thread::spawn(move || {
            BEFORE_REPLACE.set(Some(Box::new(move || {
                paused.send(()).unwrap();
                held.recv().unwrap();
            })));
            names.give(NAME, binder);
        })
    };
    paused.1.recv_timeout(WAIT).unwrap();
    names.give(NAME, first.as_binder());
    // This receive's step takes the replacement back before its wake lands.
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    held.0.send(()).unwrap();
    replacing.join().unwrap();
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(100)),
        Err(RecvError::Timeout)
    );
    first.publish(&bytes(2)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    subscriber.interrupter().interrupt();
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
    assert_eq!(first.subscriber_count(), 1);
    assert_eq!(other.subscriber_count(), 0);
}

/// A replacement's wake that lands after the name came back, as `registered` lets it: no interrupt.
#[test]
fn a_replacements_wake_after_the_name_came_back_is_not_the_callers_interrupt() {
    let names = Names::default();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    first.publish(&bytes(1)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    // Another binder's `registered`, past its unlock, after the name came back and `step` ran.
    let wake = subscriber.connection_wake().expect("the connection taken");
    wake.notice();
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(100)),
        Err(RecvError::Timeout)
    );
    first.publish(&bytes(2)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    // The caller's interrupt still ends the wait.
    subscriber.interrupter().interrupt();
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
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
    let refused = |state| {
        let security = |s: &Status| s.exception_code() == ExceptionCode::Security;
        matches!(state, NamedState::Refused(s) if security(&s))
    };
    assert!(refused(subscriber.state()), "{:?}", subscriber.state());
    names.give(NAME, publisher.as_binder());
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(1500)),
        Err(RecvError::Timeout)
    );
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    // Told once by the receive, at every ask by the state.
    assert!(refused(subscriber.state()), "{:?}", subscriber.state());
    // Another publisher of the name is asked.
    let admitting = advertise(&names, &policy());
    connected(&subscriber);
    assert_eq!(subscriber.state(), NamedState::Connected);
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
    // An ended publisher is not a refusal of this subscriber: its successor will register.
    assert_eq!(subscriber.state(), NamedState::Waiting);
    let next = advertise(&names, &policy());
    connected(&subscriber);
    next.publish(&bytes(7)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(7)));
}

/// An `ITopic` that counts its subscription calls and hands them to a publisher's acceptor.
struct Counted {
    acceptor: Acceptor,
    asked: Arc<AtomicUsize>,
}

impl Interface for Counted {}

impl ITopic for Counted {
    fn r#subscribe(&self, request: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        ASKED_HERE.set(true);
        self.acceptor.accept(request)
    }
}

thread_local! {
    /// Set on the thread a `Counted` call ran on: a call within this process runs on the caller's.
    #[cfg_attr(
        target_os = "android",
        expect(
            clippy::missing_const_for_thread_local,
            reason = "the initializer is `const`; Android's expansion of the macro hides that"
        )
    )]
    static ASKED_HERE: Cell<bool> = const { Cell::new(false) };
}

/// A closed publisher's binder, which refuses with `ERROR_CLOSED`, and its subscription calls.
fn closed_counted() -> (Publisher<Bytes>, SIBinder, Arc<AtomicUsize>) {
    let publisher = Publisher::<Bytes>::new(&policy()).unwrap();
    publisher.close(&Status::from(ExceptionCode::None));
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = Counted {
        acceptor: publisher.acceptor(),
        asked: asked.clone(),
    };
    (publisher, BnTopic::new_binder(counted).as_binder(), asked)
}

/// Wait until `asked` reaches `calls`, then long enough for a call more to show.
fn settled(asked: &AtomicUsize, calls: usize) {
    let deadline = Instant::now() + WAIT;
    while asked.load(Ordering::SeqCst) < calls {
        assert!(Instant::now() < deadline, "asked {calls} times");
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(asked.load(Ordering::SeqCst), calls);
}

/// Wait until the lookups reach `calls`, then long enough for a lookup more to show.
fn looked(names: &Names, calls: usize) {
    let deadline = Instant::now() + WAIT;
    while names.lookups() < calls {
        assert!(Instant::now() < deadline, "looked up {calls} times");
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(names.lookups(), calls);
}

/// AC-15.5.6 (1): a notice of an ended publisher, the notice after it lost.
#[test]
fn a_refusal_as_ended_asks_the_lookup_and_connects_to_the_binder_the_name_has() {
    let names = Names::looking();
    let (_closed, binder, asked) = closed_counted();
    names.give(NAME, binder);
    let next = Publisher::<Bytes>::new(&policy()).unwrap();
    // Not given before the watch's first call: that call is the ended publisher.
    let (lost, given) = (names.clone(), next.as_binder());
    names.before_lookup(move || lost.withhold(NAME, given.clone()));
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    next.publish(&bytes(4)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(4)));
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(names.lookups(), 1);
}

/// The same refusal before the watch is returned, while the subscriber has no lookup yet.
#[test]
fn a_refusal_as_ended_before_the_watch_is_returned_asks_the_lookup_once_it_is() {
    let names = Names::looking();
    let (_closed, binder, asked) = closed_counted();
    names.give(NAME, binder);
    let next = Publisher::<Bytes>::new(&policy()).unwrap();
    let (lost, given) = (names.clone(), next.as_binder());
    // The refusal comes within the watch: by the margin of `settled`, before it returns.
    names.after_first_call(move || {
        settled(&asked, 1);
        lost.withhold(NAME, given.clone());
    });
    let refused_here = Arc::new(Mutex::new(None));
    let seen = refused_here.clone();
    names.before_lookup(move || *seen.lock().unwrap() = Some(ASKED_HERE.get()));
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    next.publish(&bytes(4)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(4)));
    assert_eq!(names.lookups(), 1);
    // Asked by the thread `subscribe_with` starts, not the one refused: the path under test.
    assert_eq!(*refused_here.lock().unwrap(), Some(false));
}

/// A notice of the refused binder during the lookup makes the answer older: asked once more.
#[test]
fn a_notice_of_a_refused_binder_during_the_lookup_asks_once_more() {
    let names = Names::looking();
    let (_closed, refused, _) = closed_counted();
    names.give(NAME, refused.clone());
    let first = Publisher::<Bytes>::new(&policy()).unwrap();
    let second = Publisher::<Bytes>::new(&policy()).unwrap();
    second.publish(&bytes(9)).unwrap();
    let (hook, calls) = (names.clone(), AtomicUsize::new(0));
    let (to_first, to_second) = (first.as_binder(), second.as_binder());
    names.before_lookup(move || match calls.fetch_add(1, Ordering::SeqCst) {
        // After the closed publisher's refusal: the name has `first`.
        0 => hook.withhold(NAME, to_first.clone()),
        // After `first` ends: the closed one given again, then `second` with its notice lost.
        1 => {
            hook.give(NAME, refused.clone());
            hook.withhold(NAME, to_second.clone());
        }
        _ => {}
    });
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    first.publish(&bytes(1)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    drop(first);
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)))
    );
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
    assert_eq!(names.lookups(), 3);
}

/// A notice of the refused binder itself during the lookup is a notice too: asked once more.
#[test]
fn a_notice_that_gives_the_name_back_to_the_refused_binder_during_the_lookup_asks_once_more() {
    let names = Names::looking();
    let (_closed, refused, _) = closed_counted();
    names.give(NAME, refused.clone());
    let former = Publisher::<Bytes>::new(&policy()).unwrap();
    let (hook, calls) = (names.clone(), AtomicUsize::new(0));
    let answer = former.as_binder();
    names.before_lookup(move || match calls.fetch_add(1, Ordering::SeqCst) {
        // The answer is `former`, read before the closed binder was given the name again.
        0 => {
            hook.give(NAME, refused.clone());
            hook.withhold(NAME, answer.clone());
        }
        // The name has the closed binder now.
        _ => hook.withhold(NAME, refused.clone()),
    });
    let mut subscriber = subscribe(&names);
    looked(&names, 2);
    assert!(!subscriber.wait_connected(Duration::ZERO));
    assert_eq!(former.subscriber_count(), 0);
    let next = advertise(&names, &policy());
    connected(&subscriber);
    next.publish(&bytes(6)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(6)));
}

/// AC-15.5.6 (4): an answer that is the refused binder is not asked again.
#[test]
fn a_lookup_that_answers_the_refused_binder_leaves_the_subscriber_waiting() {
    let names = Names::looking();
    let (_closed, binder, asked) = closed_counted();
    names.give(NAME, binder);
    let mut subscriber = subscribe(&names);
    settled(&asked, 1);
    looked(&names, 1);
    assert!(!subscriber.wait_connected(Duration::ZERO));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    let next = advertise(&names, &policy());
    connected(&subscriber);
    next.publish(&bytes(6)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(6)));
}

/// AC-15.5.6 (4): an answer refused in turn asks no more, so two ended binders make no loop.
#[test]
fn a_lookup_answer_refused_as_ended_is_not_looked_past() {
    let names = Names::looking();
    let (_first, first, first_asked) = closed_counted();
    let (_second, second, second_asked) = closed_counted();
    names.give(NAME, first.clone());
    // Each lookup answers the binder the last one did not: a loop would ask both forever.
    let (flip, answers) = (names.clone(), Arc::new(AtomicUsize::new(0)));
    names.before_lookup(move || {
        let answer = [&second, &first][answers.fetch_add(1, Ordering::SeqCst) % 2];
        flip.withhold(NAME, answer.clone());
    });
    let subscriber = subscribe(&names);
    settled(&second_asked, 1);
    assert_eq!(first_asked.load(Ordering::SeqCst), 1);
    assert_eq!(names.lookups(), 1);
    assert!(!subscriber.wait_connected(Duration::ZERO));
}

/// AC-15.5.6 (2): the end of the connection's publisher, the notice of the next one lost.
#[test]
fn an_ended_publisher_with_no_binder_after_it_asks_the_lookup() {
    let names = Names::looking();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    let second = Publisher::<Bytes>::new(&policy()).unwrap();
    second.publish(&bytes(9)).unwrap();
    names.withhold(NAME, second.as_binder());
    first.publish(&bytes(1)).unwrap();
    drop(first);
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)))
    );
    // A publisher not connected to before: read from its oldest record.
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
    assert_eq!(names.lookups(), 1);
}

/// The death of the connection's publisher is an end too.
#[test]
fn a_dead_publisher_with_no_binder_after_it_asks_the_lookup() {
    let names = Names::looking();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    let second = Publisher::<Bytes>::new(&policy()).unwrap();
    second.publish(&bytes(9)).unwrap();
    names.withhold(NAME, second.as_binder());
    let death = subscriber.connection_death().expect("a connection");
    death.binder_died(&SIBinder::downgrade(&first.as_binder()));
    assert!(matches!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Ended(_)))
    ));
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
    assert_eq!(names.lookups(), 1);
}

/// A lookup answering the ended publisher, as the service manager's may, leaves subscribers idle.
#[test]
fn a_lookup_that_answers_the_ended_publisher_leaves_the_subscriber_waiting() {
    let names = Names::looking();
    let first = Publisher::<Bytes>::new(&policy()).unwrap();
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = Counted {
        acceptor: first.acceptor(),
        asked: asked.clone(),
    };
    names.give(NAME, BnTopic::new_binder(counted).as_binder());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    drop(first);
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)))
    );
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(300)),
        Err(RecvError::Timeout)
    );
    looked(&names, 1);
    // The answer is not asked: it is the publisher whose end the connection read.
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    let next = advertise(&names, &policy());
    next.publish(&bytes(3)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(3)));
}

/// Without a lookup (`Watch::new`) the subscriber waits for the next notice of the name.
#[test]
fn without_a_lookup_an_ended_publisher_waits_for_the_next_notice() {
    let names = Names::default();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    let second = Publisher::<Bytes>::new(&policy()).unwrap();
    second.publish(&bytes(9)).unwrap();
    names.withhold(NAME, second.as_binder());
    drop(first);
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)))
    );
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(300)),
        Err(RecvError::Timeout)
    );
    assert_eq!(second.subscriber_count(), 0);
    names.give(NAME, second.as_binder());
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
}

/// A notice between an end and the lookup is taken, though the lookup would answer the end.
#[test]
fn a_notice_before_the_lookup_is_asked_is_taken_without_it() {
    let names = Names::looking();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    let second = Publisher::<Bytes>::new(&policy()).unwrap();
    second.publish(&bytes(9)).unwrap();
    let (stale, ended) = (names.clone(), first.as_binder());
    names.before_lookup(move || stale.withhold(NAME, ended.clone()));
    // Past the unlock in which the end decided to look, before the thread that looks.
    let (late, given) = (names.clone(), second.as_binder());
    BEFORE_LOOKING.set(Some(Box::new(move || late.give(NAME, given.clone()))));
    drop(first);
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)))
    );
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
    assert_eq!(names.lookups(), 0);
}

/// AC-15.5.6 (3): a notice that comes while the lookup runs is newer than its answer.
#[test]
fn a_notice_during_the_lookup_is_taken_over_its_answer() {
    let names = Names::looking();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    let answer = Publisher::<Bytes>::new(&policy()).unwrap();
    let notice = Publisher::<Bytes>::new(&policy()).unwrap();
    answer.publish(&bytes(8)).unwrap();
    notice.publish(&bytes(5)).unwrap();
    names.withhold(NAME, answer.as_binder());
    let (late, given, answered) = (names.clone(), notice.as_binder(), answer.as_binder());
    names.before_lookup(move || {
        late.give(NAME, given.clone());
        late.withhold(NAME, answered.clone());
    });
    drop(first);
    assert_eq!(
        recv(&mut subscriber),
        Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)))
    );
    assert_eq!(recv(&mut subscriber), Ok(bytes(5)));
    assert_eq!(names.lookups(), 1);
    assert_eq!(notice.subscriber_count(), 1);
    assert_eq!(answer.subscriber_count(), 0);
}

/// A resolver whose advertisement looks at the topic as it is dropped: did it end before?
#[derive(Clone, Default)]
struct Ending(Arc<Mutex<Vec<Result<Bytes, RecvError>>>>);

struct Looked {
    /// In a `Mutex`, as an advertisement is `Sync` and a subscriber is not.
    subscriber: Mutex<Subscriber<Bytes>>,
    seen: Ending,
}

impl Drop for Looked {
    fn drop(&mut self) {
        let seen = self.subscriber.get_mut().unwrap().try_recv();
        self.seen.0.lock().unwrap().push(seen);
    }
}

impl Resolver for Ending {
    fn advertise(&self, _: &str, topic: SIBinder) -> BinderResult<Advertisement> {
        let subscriber = Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default())?;
        Ok(Advertisement::new(Looked {
            subscriber: Mutex::new(subscriber),
            seen: self.clone(),
        }))
    }

    fn watch(&self, _: &str, _: Box<dyn Fn(SIBinder) + Send + Sync>) -> BinderResult<Watch> {
        Ok(Watch::new(()))
    }
}

/// AC-15.5.6: the advertisement is dropped before the end record, by `close` and by `Drop`.
#[test]
fn a_publisher_drops_its_advertisement_before_its_end_record() {
    let ending = Ending::default();
    let closed = Publisher::<Bytes>::advertise_with(NAME, &policy(), &ending).unwrap();
    closed.close(&Status::from(ExceptionCode::None));
    assert_eq!(*ending.0.lock().unwrap(), [Err(RecvError::Empty)]);
    // Closing again, and the drop after a close, have no advertisement left to drop.
    closed.close(&Status::from(ExceptionCode::None));
    drop(closed);
    assert_eq!(ending.0.lock().unwrap().len(), 1);
    let dropped = Publisher::<Bytes>::advertise_with(NAME, &policy(), &ending).unwrap();
    drop(dropped);
    assert_eq!(
        *ending.0.lock().unwrap(),
        [Err(RecvError::Empty), Err(RecvError::Empty)]
    );
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

fn subscribe_on(names: &Names, name: &str, wake: Wake) -> NamedSubscriber<Bytes> {
    let policy = SubscriberPolicy {
        wake,
        ..SubscriberPolicy::default()
    };
    NamedSubscriber::subscribe_with(name, &policy, names).unwrap()
}

fn subscribe_at(names: &Names, wake: Wake, start: Start) -> NamedSubscriber<Bytes> {
    let policy = SubscriberPolicy {
        wake,
        start,
        ..SubscriberPolicy::default()
    };
    NamedSubscriber::subscribe_with(NAME, &policy, names).unwrap()
}

fn raw_fds(subscriber: &NamedSubscriber<Bytes>) -> i32 {
    subscriber.wait_fd().unwrap().as_raw_fd()
}

/// One thread, a poll loop of `wait_any`: receive into `got` until `done` holds of it.
fn poll_until(
    subscribers: &mut [NamedSubscriber<Bytes>],
    got: &mut [Vec<Result<Bytes, RecvError>>],
    done: impl Fn(&[Vec<Result<Bytes, RecvError>>]) -> bool,
) {
    let deadline = Instant::now() + WAIT;
    while !done(got) {
        assert!(Instant::now() < deadline, "received only {got:?}");
        let mut waits: Vec<&mut dyn Wait> = subscribers
            .iter_mut()
            .map(|subscriber| subscriber as &mut dyn Wait)
            .collect();
        // Up to the deadline: a wake that does not come fails the test, as no later poll finds it.
        let left = deadline.saturating_duration_since(Instant::now());
        wait_any(&mut waits, Some(left)).unwrap();
        for (subscriber, got) in subscribers.iter_mut().zip(got.iter_mut()) {
            loop {
                match subscriber.try_recv() {
                    Err(RecvError::Empty) => break,
                    other => got.push(other),
                }
            }
        }
    }
}

/// AC-15.2.36: three name subscribers on one poll loop keep their fds through a new publisher.
#[test]
fn name_subscribers_on_one_poll_loop_keep_their_fds_through_a_new_publisher() {
    let names = Names::default();
    let topics = ["test.a", "test.b", "test.c"];
    let publishers: Vec<_> = topics
        .iter()
        .map(|name| Publisher::<Bytes>::advertise_with(name, &policy(), &names).unwrap())
        .collect();
    let mut subscribers: Vec<_> = topics
        .iter()
        .map(|name| subscribe_on(&names, name, Wake::Fd))
        .collect();
    for subscriber in &subscribers {
        connected(subscriber);
    }
    let fds: Vec<_> = subscribers.iter().map(raw_fds).collect();
    let mut got = vec![Vec::new(), Vec::new(), Vec::new()];
    for publisher in &publishers {
        publisher.publish(&bytes(1)).unwrap();
    }
    poll_until(&mut subscribers, &mut got, |got| {
        got.iter().all(|got| got.len() == 1)
    });
    let mut publishers = publishers.into_iter();
    let (a, b, c) = (publishers.next(), publishers.next(), publishers.next());
    drop(b);
    poll_until(&mut subscribers, &mut got, |got| got[1].len() == 2);
    // Published before the name is given: the reconnection reads it.
    let next = Publisher::<Bytes>::new(&policy()).unwrap();
    next.publish(&bytes(7)).unwrap();
    names.give("test.b", next.as_binder());
    poll_until(&mut subscribers, &mut got, |got| got[1].len() == 3);
    for publisher in [a.as_ref(), Some(&next), c.as_ref()].into_iter().flatten() {
        publisher.publish(&bytes(2)).unwrap();
    }
    poll_until(&mut subscribers, &mut got, |got| {
        got.iter().all(|got| got.last() == Some(&Ok(bytes(2))))
    });
    let ended = Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)));
    assert_eq!(got[0], [Ok(bytes(1)), Ok(bytes(2))]);
    assert_eq!(got[1], [Ok(bytes(1)), ended, Ok(bytes(7)), Ok(bytes(2))]);
    assert_eq!(got[2], [Ok(bytes(1)), Ok(bytes(2))]);
    let after: Vec<_> = subscribers.iter().map(raw_fds).collect();
    assert_eq!(after, fds);
}

/// A poll that sleeps ends at a commit of the connection, the first one and the next.
#[test]
fn a_poll_that_sleeps_ends_at_a_commit_through_a_reconnection() {
    let names = Names::default();
    let mut subscriber = subscribe_on(&names, NAME, Wake::Fd);
    for round in 0..2u8 {
        let publisher = Arc::new(advertise(&names, &policy()));
        // Took the connection: the next receive finds nothing, the poll then sleeps.
        connected(&subscriber);
        assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
        let later = publisher.clone();
        let publishing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            later.publish(&bytes(round)).unwrap();
        });
        // A poll that ends for an earlier change finds nothing; the one after waits for the commit.
        let started = Instant::now();
        let got = loop {
            let left = WAIT.saturating_sub(started.elapsed());
            assert!(!left.is_zero(), "round {round}: no commit ended the poll");
            let mut waits: [&mut dyn Wait; 1] = [&mut subscriber];
            wait_any(&mut waits, Some(left)).unwrap();
            match subscriber.try_recv() {
                Err(RecvError::Empty) => {}
                other => break other,
            }
        };
        publishing.join().unwrap();
        assert_eq!(got, Ok(bytes(round)));
        // The commit came at 50 ms; a poll that it did not end sleeps to the bound.
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "round {round}: {took:?}");
        drop(publisher);
        let ended = Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)));
        assert_eq!(recv(&mut subscriber), ended);
    }
}

#[test]
fn an_interrupt_ends_a_poll_with_no_connection() {
    let names = Names::default();
    let mut subscriber = subscribe_on(&names, NAME, Wake::Fd);
    let interrupter = subscriber.interrupter();
    let interrupting = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        interrupter.interrupt();
    });
    let started = Instant::now();
    assert!(subscriber.prepare_wait());
    let mut waits: [&mut dyn Wait; 1] = [&mut subscriber];
    assert!(wait_any(&mut waits, Some(WAIT)).unwrap());
    assert!(started.elapsed() < WAIT);
    subscriber.finish_wait();
    interrupting.join().unwrap();
    // A receive that does not wait leaves the interrupt to the next one that does.
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
}

/// `wait_any` of one name subscriber, bounded by `timeout`.
fn waited(subscriber: &mut NamedSubscriber<Bytes>, timeout: Duration) -> bool {
    let mut waits: [&mut dyn Wait; 1] = [subscriber];
    wait_any(&mut waits, Some(timeout)).unwrap()
}

/// `connected`, and the fd readable: the connection thread's eventfd wake, which comes after.
fn announced(subscriber: &NamedSubscriber<Bytes>) {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    connected(subscriber);
    let fd = subscriber.wait_fd().unwrap();
    let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
    let timeout = Timespec::try_from(WAIT).unwrap();
    let ready = poll(&mut fds, Some(&timeout)).unwrap();
    assert_eq!(ready, 1, "announced within {WAIT:?}");
}

/// Raised after a poll returned, its wakes drained: the next call returns at once, then a sleep.
fn an_interrupt_whose_wakes_a_finish_wait_drains_ends_the_next_poll(connection: bool) {
    let names = Names::default();
    let mut subscriber = subscribe_on(&names, NAME, Wake::Fd);
    let _publisher = connection.then(|| advertise(&names, &policy()));
    if connection {
        announced(&subscriber);
        // Takes the connection.
        assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    }
    assert!(subscriber.prepare_wait());
    subscriber.interrupter().interrupt();
    subscriber.finish_wait();
    let started = Instant::now();
    assert!(waited(&mut subscriber, WAIT));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!waited(&mut subscriber, Duration::from_millis(20)));
    let short = Duration::from_millis(50);
    assert_eq!(subscriber.recv_timeout(short), Err(RecvError::Cancelled));
    assert!(!waited(&mut subscriber, Duration::from_millis(20)));
}

#[test]
fn an_interrupt_whose_wakes_a_finish_wait_drains_ends_the_next_poll_with_no_connection() {
    an_interrupt_whose_wakes_a_finish_wait_drains_ends_the_next_poll(false);
}

#[test]
fn an_interrupt_whose_wakes_a_finish_wait_drains_ends_the_next_poll_of_a_connection() {
    an_interrupt_whose_wakes_a_finish_wait_drains_ends_the_next_poll(true);
}

/// Interrupted before its connection is taken, by a notice: polls end for kick and eventfd only.
#[test]
fn an_interrupt_before_the_connection_is_taken_kicks_one_poll() {
    let names = Names::default();
    let mut subscriber = subscribe_on(&names, NAME, Wake::Fd);
    let _publisher = advertise(&names, &policy());
    // The connection's eventfd wake lands now, not after a poll counted below drained the fd.
    announced(&subscriber);
    subscriber.interrupter().interrupt();
    // Takes the connection, and does not wait.
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    let mut early = 0;
    while waited(&mut subscriber, Duration::from_millis(20)) {
        early += 1;
        assert!(
            early <= 2,
            "more polls ended than the interrupt kicked and woke"
        );
    }
    let short = Duration::from_millis(50);
    assert_eq!(subscriber.recv_timeout(short), Err(RecvError::Cancelled));
}

/// A receive that returns an interrupt takes its kick too: no poll after it ends for it.
#[test]
fn a_name_subscribers_receive_cancelled_by_an_interrupt_leaves_no_poll_to_end_for_it() {
    let names = Names::default();
    let mut subscriber = subscribe_on(&names, NAME, Wake::Fd);
    let _publisher = advertise(&names, &policy());
    announced(&subscriber);
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    subscriber.interrupter().interrupt();
    let short = Duration::from_millis(50);
    assert_eq!(subscriber.recv_timeout(short), Err(RecvError::Cancelled));
    // Drained by this call's `finish_wait`: a wake the interrupt sent ends one poll at most.
    let early = waited(&mut subscriber, Duration::from_millis(20));
    assert!(!early || !waited(&mut subscriber, Duration::from_millis(20)));
    assert_eq!(subscriber.recv_timeout(short), Err(RecvError::Timeout));
}

/// An interrupt whose wake of the connection lands after a receive returned it: one `Cancelled`.
#[test]
fn an_interrupt_whose_connections_wake_lands_late_ends_one_receive() {
    let names = Names::default();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    first.publish(&bytes(1)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    let other = Publisher::<Bytes>::new(&policy()).unwrap();
    let (paused, held) = (mpsc::channel(), mpsc::channel::<()>());
    let interrupting = {
        let interrupter = subscriber.interrupter();
        let (paused, held) = (paused.0, held.1);
        std::thread::spawn(move || {
            BEFORE_INTERRUPT.set(Some(Box::new(move || {
                paused.send(()).unwrap();
                held.recv().unwrap();
            })));
            interrupter.interrupt();
        })
    };
    paused.1.recv_timeout(WAIT).unwrap();
    // Away and back: the replacement's wake ends a wait, which returns the interrupt, then none.
    names.give(NAME, other.as_binder());
    names.give(NAME, first.as_binder());
    let short = Duration::from_millis(100);
    assert_eq!(subscriber.recv_timeout(short), Err(RecvError::Cancelled));
    assert_eq!(subscriber.recv_timeout(short), Err(RecvError::Timeout));
    held.0.send(()).unwrap();
    interrupting.join().unwrap();
    // The interrupt's wake of the connection, now: returned once already, it ends no receive.
    assert_eq!(subscriber.recv_timeout(short), Err(RecvError::Timeout));
    first.publish(&bytes(2)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    assert_eq!(first.subscriber_count(), 1);
}

/// The receive whose connection's fd the epoll instance refuses: `Rejected`, from `join`.
fn refused_to_join(subscriber: &mut NamedSubscriber<Bytes>) {
    crate::named::NO_JOIN.set(true);
    let refused = subscriber.try_recv();
    crate::named::NO_JOIN.set(false);
    assert!(
        matches!(
            refused,
            Err(RecvError::Disconnected(Disconnected::Rejected(_)))
        ),
        "{refused:?}"
    );
}

/// A connection its epoll instance refuses moves to the replacement, not ended as replaced.
#[test]
fn a_connection_that_fails_to_join_leaves_no_replacement_behind() {
    let names = Names::default();
    let first = advertise(&names, &policy());
    let mut subscriber = subscribe_on(&names, NAME, Wake::Fd);
    // Made and not yet taken: the next publisher of the name is a replacement of it.
    connected(&subscriber);
    let second = Publisher::<Bytes>::new(&policy()).unwrap();
    let (asked, was_asked) = mpsc::sync_channel(1);
    let (go, gone_on) = mpsc::sync_channel(1);
    let hanging = Hanging {
        acceptor: second.acceptor(),
        asked,
        go: Mutex::new(gone_on),
        answered: Arc::new(AtomicUsize::new(0)),
    };
    names.give(NAME, BnTopic::new_binder(hanging).as_binder());
    // No connection joined before: the replacement is read from the policy's `Latest`.
    second.publish(&bytes(8)).unwrap();
    refused_to_join(&mut subscriber);
    // The first retry is 1 s away; the replacement is asked now, and held there.
    was_asked.recv_timeout(WAIT).unwrap();
    assert!(
        subscriber.prepare_wait(),
        "no replacement left to return at"
    );
    subscriber.finish_wait();
    go.send(()).unwrap();
    assert!(subscriber.wait_connected(Duration::from_millis(500)));
    second.publish(&bytes(1)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    second.publish(&bytes(2)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(2)));
    assert_eq!(second.subscriber_count(), 1);
    drop(first);
}

/// A connection its epoll instance refuses, with no other publisher, is asked for again later.
#[test]
fn a_connection_that_fails_to_join_is_asked_for_again_after_a_retry_interval() {
    let names = Names::default();
    let publisher = advertise(&names, &policy());
    // Before the subscription: a retry that no join preceded starts at the policy's `Oldest`.
    publisher.publish(&bytes(9)).unwrap();
    let mut subscriber = subscribe_at(&names, Wake::Fd, Start::Oldest);
    connected(&subscriber);
    refused_to_join(&mut subscriber);
    let refused = Instant::now();
    assert!(
        subscriber.prepare_wait(),
        "nothing to receive, yet a poll returns"
    );
    subscriber.finish_wait();
    connected(&subscriber);
    let waited = refused.elapsed();
    assert!(
        waited >= Duration::from_millis(500),
        "asked again after {waited:?}"
    );
    publisher.publish(&bytes(1)).unwrap();
    assert_eq!(recv(&mut subscriber), Ok(bytes(9)));
    assert_eq!(recv(&mut subscriber), Ok(bytes(1)));
}

#[test]
#[should_panic(expected = "Wake::Futex")]
fn a_futex_name_subscriber_has_no_wait_to_prepare() {
    let names = Names::default();
    let mut subscriber = subscribe(&names);
    assert!(subscriber.wait_fd().is_none());
    subscriber.prepare_wait();
}

#[test]
#[should_panic(expected = "finish_wait: a subscription made with `Wake::Futex`")]
fn a_futex_name_subscriber_has_no_wait_to_finish() {
    let names = Names::default();
    let mut subscriber = subscribe(&names);
    subscriber.finish_wait();
}

/// AC-15.2.36: `recv_async` through a change of publisher, on either wake.
#[cfg(feature = "tokio")]
mod reactor {
    use super::*;

    fn through_a_new_publisher(wake: Wake) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let names = Names::default();
        let first = advertise(&names, &policy());
        let mut subscriber = subscribe_on(&names, NAME, wake);
        connected(&subscriber);
        let (told, received) = mpsc::channel();
        let receiving = runtime.spawn(async move {
            let mut got = Vec::new();
            while got.len() < 4 {
                let next = tokio::time::timeout(WAIT, subscriber.recv_async()).await;
                got.push(next.expect("a receive that did not end"));
                let _ = told.send(got.len());
            }
            got
        });
        first.publish(&bytes(1)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        drop(first);
        // The end returned: the receive waits with no connection, then the next publisher's.
        while received.recv_timeout(WAIT).unwrap() < 2 {}
        let second = advertise(&names, &policy());
        second.publish(&bytes(2)).unwrap();
        second.publish(&bytes(3)).unwrap();
        let got = runtime.block_on(receiving).unwrap();
        let ended = Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)));
        assert_eq!(got, [Ok(bytes(1)), ended, Ok(bytes(2)), Ok(bytes(3))]);
    }

    #[test]
    fn an_async_receive_goes_on_with_the_next_publisher_on_the_futex() {
        through_a_new_publisher(Wake::Futex);
    }

    #[test]
    fn an_async_receive_goes_on_with_the_next_publisher_on_the_fd() {
        through_a_new_publisher(Wake::Fd);
    }

    #[test]
    fn an_interrupt_ends_an_async_wait_for_a_connection() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let names = Names::default();
        let mut subscriber = subscribe_on(&names, NAME, Wake::Fd);
        let interrupter = subscriber.interrupter();
        let receiving =
            runtime.spawn(async move { tokio::time::timeout(WAIT, subscriber.recv_async()).await });
        std::thread::sleep(Duration::from_millis(50));
        interrupter.interrupt();
        let got = runtime.block_on(receiving).unwrap();
        assert_eq!(
            got.expect("the interrupt ended the wait"),
            Err(RecvError::Cancelled)
        );
    }
}

/// A policy that waits up to 100 ms for its subscribers.
fn waiting() -> PublisherPolicy {
    PublisherPolicy {
        backpressure: Some(Backpressure {
            timeout: Duration::from_millis(100),
            ..Backpressure::default()
        }),
        ..policy()
    }
}

/// Two rings' worth of values, which a subscriber that reads none holds up past the timeout.
fn overrun(publisher: &Publisher<Bytes>) {
    for n in 0..2 * MIN_RING / 16 {
        publisher.publish(&bytes(n as u8)).unwrap();
    }
}

/// AC-15.3.13: a name subscriber taken out returns `Closed(Evicted)` and connects no more.
#[test]
fn a_subscriber_of_a_name_taken_out_connects_to_no_publisher_again() {
    for wake in [Wake::Futex, Wake::Fd] {
        let names = Names::default();
        let first = advertise(&names, &waiting());
        let woken = SubscriberPolicy {
            wake,
            ..SubscriberPolicy::default()
        };
        let mut subscriber = NamedSubscriber::subscribe_with(NAME, &woken, &names).unwrap();
        connected(&subscriber);
        overrun(&first);
        assert_eq!(first.stats().evicted, 1, "{wake:?}");
        let evicted = Err(RecvError::Closed(Closed::Evicted));
        assert_eq!(recv(&mut subscriber), evicted, "{wake:?}");
        if wake == Wake::Fd {
            assert!(!subscriber.prepare_wait(), "a receive returns at once");
        }
        drop(first);
        let second = advertise(&names, &policy());
        assert!(!subscriber.wait_connected(Duration::from_millis(200)));
        assert!(second.stats().subscribers.is_empty(), "{wake:?}");
        assert_eq!(subscriber.try_recv(), evicted, "{wake:?}");
    }
}

/// AC-15.3.13: taken out while draining a replaced publisher, it stays out and does not move.
#[test]
fn a_subscriber_of_a_name_taken_out_while_it_drains_stays_out() {
    let names = Names::default();
    let first = advertise(&names, &waiting());
    let mut subscriber = subscribe(&names);
    connected(&subscriber);
    first.publish(&bytes(0)).unwrap();
    // Taken: the connection is the subscriber's, and the next receive drains it.
    assert_eq!(recv(&mut subscriber), Ok(bytes(0)));
    overrun(&first);
    assert_eq!(first.stats().evicted, 1);
    let second = advertise(&names, &policy());
    let evicted = Err(RecvError::Closed(Closed::Evicted));
    assert_eq!(recv(&mut subscriber), evicted);
    std::thread::sleep(Duration::from_millis(200));
    assert!(second.stats().subscribers.is_empty());
    assert_eq!(subscriber.try_recv(), evicted);
}

#[test]
fn a_name_subscriber_and_its_interrupter_move_between_threads() {
    fn moved<T: Send>() {}
    fn shared<T: Send + Sync>() {}
    moved::<NamedSubscriber<Bytes>>();
    shared::<crate::Interrupter>();
    shared::<Authorize>();
}
