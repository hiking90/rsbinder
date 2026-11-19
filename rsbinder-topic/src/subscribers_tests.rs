// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A publisher's wait for its list of subscriptions to change: [`Publisher::subscribers`] and
//! [`Publisher::wait_subscribers`], with subscribers of this process. A subscriber process that
//! dies is in `tests/two_process.rs`.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rsbinder::{ParcelFileDescriptor, SIBinder};

use crate::aidl::ITopic::WAKE_FUTEX;
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::connect::{Asking, Token};
use crate::layout::{ControlPage, CLOSED, VERSION};
use crate::pool::MAX_PINS;
use crate::{Bytes, Payload, Publisher, Start, Subscriber, Subscribers};

pub(crate) const LONG: Duration = Duration::from_secs(10);

fn publisher() -> Publisher<Bytes> {
    Publisher::<Bytes>::events().build().unwrap()
}

fn subscribe(publisher: &Publisher<Bytes>) -> Subscriber<Bytes> {
    Subscriber::<Bytes>::builder()
        .start_at(Start::Next)
        .subscribe(&publisher.as_binder())
        .unwrap()
}

/// A subscription that ends as a subscriber that does not tell the publisher does: its control
/// page marked `CLOSED`, and nothing else.
struct Quiet {
    page: Arc<ControlPage>,
    _token: SIBinder,
}

impl Quiet {
    fn subscribe(publisher: &Publisher<Bytes>) -> Self {
        let (fd, page) = ControlPage::create(MAX_PINS).unwrap();
        let token = Token::detached();
        let request = SubscribeRequest {
            version: VERSION as i32,
            r#type: Bytes::type_name().to_owned(),
            token: Some(token.clone()),
            control: Some(ParcelFileDescriptor::from(fd)),
            pins: MAX_PINS as i32,
            wake: WAKE_FUTEX,
            wakeFd: None,
        };
        let _asking = Asking::new(&request);
        publisher.acceptor().accept(&request).unwrap();
        Self {
            page: Arc::new(page),
            _token: token,
        }
    }

    fn close(&self) {
        self.page
            .state()
            .fetch_or(CLOSED, std::sync::atomic::Ordering::Release);
    }
}

#[test]
fn the_generation_grows_with_each_subscription_taken_or_let_go_and_nothing_else() {
    let publisher = Publisher::<Bytes>::events()
        .max_subscribers(2)
        .build()
        .unwrap();
    let first = publisher.subscribers();
    assert_eq!(first.count, 0);
    let a = subscribe(&publisher);
    let b = subscribe(&publisher);
    let two = publisher.subscribers();
    assert_eq!((two.generation, two.count), (first.generation + 2, 2));
    // A refusal, a publish and a look change nothing.
    let refused = Subscriber::<Bytes>::builder().subscribe(&publisher.as_binder());
    assert!(refused.is_err());
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(publisher.subscribers(), two);
    assert_eq!(publisher.subscriber_count(), 2);
    drop(a);
    drop(b);
    let none = publisher.subscribers();
    assert_eq!(none.count, 0);
    assert!(none.generation > two.generation);
}

#[test]
fn a_wait_returns_at_once_when_the_list_changed_since_seen() {
    let publisher = publisher();
    let seen = publisher.subscribers().generation;
    let _subscriber = subscribe(&publisher);
    let started = Instant::now();
    let now = publisher.wait_subscribers(seen, Some(LONG));
    assert!(started.elapsed() < LONG / 2, "waited {:?}", started.elapsed());
    assert_eq!((now.generation, now.count), (seen + 1, 1));
}

/// Wait on `publisher` from another thread while `change` runs; the list it returned, and when.
pub(crate) fn woken_by(
    publisher: &Publisher<Bytes>,
    change: impl FnOnce(),
) -> (Subscribers, Duration) {
    let seen = publisher.subscribers().generation;
    let (woke, woken) = mpsc::channel();
    std::thread::scope(|scope| {
        // Bounded, so a wake that never comes fails the test instead of holding it.
        scope.spawn(|| {
            let started = Instant::now();
            let now = publisher.wait_subscribers(seen, Some(LONG));
            woke.send((now, started.elapsed())).unwrap();
        });
        // Counted under the lock, which the change takes: it lands once the wait sleeps.
        while publisher.list_sleeps() == 0 {
            std::thread::yield_now();
        }
        change();
        woken.recv().unwrap()
    })
}

#[test]
fn a_subscription_taken_wakes_a_wait() {
    let publisher = publisher();
    let mut subscriber = None;
    let (now, took) = woken_by(&publisher, || subscriber = Some(subscribe(&publisher)));
    assert!(took < LONG / 2, "not woken: {took:?}");
    assert_eq!(now.count, 1);
}

#[test]
fn a_dropped_subscriber_tells_the_publisher_and_wakes_a_wait_without_a_publish() {
    let publisher = publisher();
    let mut subscriber = Some(subscribe(&publisher));
    let (now, took) = woken_by(&publisher, || drop(subscriber.take()));
    assert!(took < LONG / 2, "not woken: {took:?}");
    assert_eq!(now.count, 0);
}

#[test]
fn a_refused_subscriber_leaves_the_list_as_it_was() {
    let publisher = Publisher::<Bytes>::events()
        .max_subscribers(1)
        .build()
        .unwrap();
    let _held = subscribe(&publisher);
    let before = publisher.subscribers();
    // Its control page is closed and the publisher told, with nothing of it in the list.
    let refused = Subscriber::<Bytes>::builder().subscribe(&publisher.as_binder());
    assert!(refused.is_err());
    assert_eq!(publisher.subscribers(), before);
}

#[test]
fn a_wait_past_its_timeout_returns_the_list_unchanged() {
    let publisher = publisher();
    let _subscriber = subscribe(&publisher);
    let seen = publisher.subscribers();
    let started = Instant::now();
    let now = publisher.wait_subscribers(seen.generation, Some(Duration::from_millis(30)));
    assert!(started.elapsed() >= Duration::from_millis(30));
    assert_eq!(now, seen);
}

#[test]
fn changes_between_two_waits_reach_the_next_as_they_left_the_list() {
    let publisher = publisher();
    let seen = publisher.subscribers().generation;
    let a = subscribe(&publisher);
    let _b = subscribe(&publisher);
    drop(a);
    let now = publisher.wait_subscribers(seen, Some(Duration::ZERO));
    assert_eq!(now.count, 1);
    assert_eq!(now, publisher.subscribers());
    assert!(now.generation >= seen + 3);
}

#[test]
fn a_subscription_that_ends_unannounced_is_seen_when_the_wait_times_out() {
    let publisher = publisher();
    let quiet = Quiet::subscribe(&publisher);
    let seen = publisher.subscribers();
    assert_eq!(seen.count, 1);
    quiet.close();
    let now = publisher.wait_subscribers(seen.generation, Some(Duration::from_millis(30)));
    assert_eq!((now.generation, now.count), (seen.generation + 1, 0));
}

#[cfg(feature = "tokio")]
#[test]
fn an_async_wait_is_woken_and_one_dropped_leaves_the_next_working() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    let publisher = publisher();
    let seen = publisher.subscribers().generation;
    let mut cx = Context::from_waker(Waker::noop());
    {
        // Dropped unfinished: nothing of it stays behind to take the next wake.
        let dropped = std::pin::pin!(publisher.wait_subscribers_async(seen));
        assert!(dropped.poll(&mut cx).is_pending());
    }
    let mut wait = std::pin::pin!(publisher.wait_subscribers_async(seen));
    assert!(wait.as_mut().poll(&mut cx).is_pending());
    let _subscriber = subscribe(&publisher);
    let Poll::Ready(now) = wait.as_mut().poll(&mut cx) else {
        panic!("the async wait was not woken");
    };
    let expected = Subscribers {
        generation: seen + 1,
        count: 1,
    };
    assert_eq!(now, expected);
}
