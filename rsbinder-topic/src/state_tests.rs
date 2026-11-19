// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! State topics within one process (plan 15 field §3.2, §3.3, §3.7): the first value, the kind a
//! subscriber learns, where `Start::Auto` starts, and receives that return the newest value.
//! The name layer's part is in `named_tests.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rsbinder::{ExceptionCode, Parcel, SIBinder, StatusCode};

use crate::aidl::TopicEndpoint::TopicEndpoint;
use crate::layout::MIN_RING;
use crate::subscriber::Settings;
use crate::{
    Bytes, Closed, Encoding, Payload, PublishError, Publisher, Record, RecvError, Start,
    Subscriber, TopicKind,
};

/// The bound of every wait a test makes, so that a broken subscriber fails rather than hangs.
const WAIT: Duration = Duration::from_secs(10);

/// A value that names its place in a sequence.
fn count(n: u32) -> Bytes {
    Bytes(n.to_le_bytes().to_vec())
}

fn counted(value: &[u8]) -> u32 {
    u32::from_le_bytes(value.try_into().expect("a counted value"))
}

fn subscribe(topic: &SIBinder) -> Subscriber<Bytes> {
    Subscriber::builder().subscribe(topic).unwrap()
}

fn subscribe_at(topic: &SIBinder, start: Start) -> Subscriber<Bytes> {
    Subscriber::builder()
        .start_at(start)
        .subscribe(topic)
        .unwrap()
}

/// A value whose encoding fails with `BadType`, as one holding a binder does.
#[derive(Debug, PartialEq)]
struct Unencodable;

impl Payload for Unencodable {
    fn type_name() -> &'static str {
        "test.Unencodable"
    }

    fn encode<'a>(&'a self, _: &mut Encoding<'a>) -> rsbinder::Result<()> {
        Err(StatusCode::BadType)
    }

    fn decode(_: &mut Record<'_>) -> rsbinder::Result<Self> {
        Ok(Self)
    }
}

#[test]
fn a_state_topic_returns_its_first_value_to_a_default_subscriber_at_once() {
    let publisher = Publisher::<Bytes>::state().build(&count(1)).unwrap();
    assert_eq!(publisher.topic_kind(), TopicKind::State);
    let mut subscriber = subscribe(&publisher.as_binder());
    assert_eq!(subscriber.topic_kind(), TopicKind::State);
    // `try_recv`: no wait.
    assert_eq!(subscriber.try_recv(), Ok(count(1)));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    publisher.publish(&count(2)).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(count(2)));
}

#[test]
fn an_event_topic_says_so_and_a_default_subscriber_starts_at_the_next_record() {
    let publisher = Publisher::<Bytes>::events().build().unwrap();
    assert_eq!(publisher.topic_kind(), TopicKind::Event);
    publisher.publish(&count(1)).unwrap();
    let mut subscriber = subscribe(&publisher.as_binder());
    assert_eq!(subscriber.topic_kind(), TopicKind::Event);
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    publisher.publish(&count(2)).unwrap();
    assert_eq!(subscriber.try_recv(), Ok(count(2)));
}

#[test]
fn a_first_value_that_does_not_fit_or_is_not_published_makes_no_topic() {
    let illegal = |made: rsbinder::BinderResult<Publisher<Bytes>>, says: &[&str]| {
        let status = made.expect_err("refused");
        assert_eq!(status.exception_code(), ExceptionCode::IllegalArgument);
        let message = status.message().unwrap_or_default().to_owned();
        for word in says {
            assert!(message.contains(word), "{message:?} names {word:?}");
        }
    };
    // The default ring is 4 KiB, whose records carry at most 2040 bytes.
    let large = Bytes(vec![0; 2041]);
    illegal(Publisher::state().build(&large), &["2041", "2040"]);
    illegal(Publisher::state().build_with(|_| Ok(())), &["nothing"]);
    illegal(
        Publisher::state().build_with(|_| Err(PublishError::NoPool)),
        &["pool"],
    );
    illegal(
        Publisher::state().build_with(|publisher| {
            publisher.publish(&count(1))?;
            publisher.close();
            Ok(())
        }),
        &["closed"],
    );
    let encoded = Publisher::<Unencodable>::state().build(&Unencodable);
    let status = encoded.expect_err("refused");
    assert_eq!(status.transaction_error(), StatusCode::BadType);
}

#[test]
fn a_first_value_larger_than_a_record_goes_into_a_slot() {
    let publisher = Publisher::<Bytes>::state()
        .lend(16 << 10)
        .build_with(|publisher| {
            let mut slot = publisher.loan_slice(8000)?;
            slot.fill(7);
            slot.publish()
        })
        .unwrap();
    let mut subscriber = subscribe(&publisher.as_binder());
    let sample = subscriber.try_recv_sample().unwrap();
    assert_eq!(sample.to_owned(), vec![7; 8000]);
}

#[test]
fn an_explicit_start_comes_before_the_kind() {
    let state = Publisher::<Bytes>::state().build(&count(1)).unwrap();
    let mut next = subscribe_at(&state.as_binder(), Start::Next);
    assert_eq!(next.try_recv(), Err(RecvError::Empty));
    state.publish(&count(2)).unwrap();
    assert_eq!(next.try_recv(), Ok(count(2)));
    // `Oldest` on a state topic: its receives return the newest value all the same.
    state.publish(&count(3)).unwrap();
    let mut oldest = subscribe_at(&state.as_binder(), Start::Oldest);
    assert_eq!(oldest.try_recv(), Ok(count(3)));

    let events = Publisher::<Bytes>::events().build().unwrap();
    events.publish(&count(1)).unwrap();
    events.publish(&count(2)).unwrap();
    let mut newest = subscribe_at(&events.as_binder(), Start::Newest);
    assert_eq!(newest.try_recv(), Ok(count(2)));
    let mut oldest = subscribe_at(&events.as_binder(), Start::Oldest);
    assert_eq!(oldest.try_recv(), Ok(count(1)));
}

/// Values enough to lap a 4 KiB ring of 16-byte records several times.
const LAPS: u32 = 1000;

#[test]
fn a_state_topic_skips_to_the_newest_value_and_counts_no_loss() {
    let state = Publisher::<Bytes>::state().build(&count(0)).unwrap();
    assert_eq!(state.ring_bytes(), MIN_RING);
    let mut subscriber = subscribe(&state.as_binder());
    let mut sampled = subscribe(&state.as_binder());
    for n in 1..=LAPS {
        state.publish(&count(n)).unwrap();
    }
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(count(LAPS)));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    assert_eq!(subscriber.stats().lost, 0);
    let sample = sampled.recv_sample_timeout(WAIT).unwrap();
    assert_eq!(counted(&sample.to_owned()), LAPS);
    drop(sample);
    assert_eq!(sampled.stats().lost, 0);

    // The same on an event topic: the records written over are lost, and said so.
    let events = Publisher::<Bytes>::events().build().unwrap();
    let mut subscriber = subscribe(&events.as_binder());
    for n in 1..=LAPS {
        events.publish(&count(n)).unwrap();
    }
    assert!(matches!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Lagged(_))
    ));
}

#[cfg(feature = "tokio")]
#[test]
fn an_async_receive_of_a_state_topic_returns_the_newest_value() {
    let state = Publisher::<Bytes>::state().build(&count(0)).unwrap();
    let mut subscriber = subscribe(&state.as_binder());
    assert_eq!(subscriber.try_recv(), Ok(count(0)));
    for n in 1..=LAPS {
        state.publish(&count(n)).unwrap();
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let got = runtime.block_on(async { tokio::time::timeout(WAIT, subscriber.recv_async()).await });
    assert_eq!(got.unwrap(), Ok(count(LAPS)));
    assert_eq!(subscriber.stats().lost, 0);
}

/// How a receive of [`races_a_publisher`] reads.
#[derive(Clone, Copy)]
enum Receive {
    Copy,
    Sample,
}

/// Receive from a thread publishing up to `last` until it comes: each value newer, none lost.
fn races_a_publisher(publisher: Publisher<Bytes>, receive: Receive, last: u32) {
    let mut subscriber = subscribe(&publisher.as_binder());
    let done = Arc::new(AtomicBool::new(false));
    let publishing = std::thread::spawn({
        let done = done.clone();
        move || {
            for n in 1..=last {
                match publisher.loan_slice(4) {
                    Ok(mut slot) => {
                        slot.copy_from_slice(&n.to_le_bytes());
                        slot.publish().unwrap();
                    }
                    Err(PublishError::NoPool) => publisher.publish(&count(n)).unwrap(),
                    Err(error) => panic!("{error}"),
                }
            }
            // Kept until the subscriber has read the last value: dropping it ends the topic.
            while !done.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });
    let mut before = None;
    loop {
        let got = match receive {
            Receive::Copy => subscriber.recv_timeout(WAIT).map(|Bytes(value)| value),
            Receive::Sample => subscriber
                .recv_sample_timeout(WAIT)
                .map(|sample| sample.to_owned()),
        };
        let got = counted(&got.expect("a value, not an error"));
        assert!(before < Some(got), "{got} after {before:?}");
        before = Some(got);
        if got == last {
            break;
        }
    }
    assert_eq!(subscriber.stats().lost, 0);
    done.store(true, Ordering::Release);
    publishing.join().unwrap();
}

#[test]
fn a_subscriber_slower_than_its_state_publisher_gets_newer_values_and_no_loss() {
    for receive in [Receive::Copy, Receive::Sample] {
        let state = Publisher::<Bytes>::state().build(&count(0)).unwrap();
        races_a_publisher(state, receive, 50_000);
    }
}

#[test]
fn a_subscriber_slower_than_its_lending_state_publisher_gets_newer_values_and_no_loss() {
    for receive in [Receive::Copy, Receive::Sample] {
        let state = Publisher::<Bytes>::state()
            .max_subscribers(2)
            .lend(64)
            .build_with(|publisher| {
                let mut slot = publisher.loan_slice(4)?;
                slot.copy_from_slice(&0u32.to_le_bytes());
                slot.publish()
            })
            .unwrap();
        races_a_publisher(state, receive, 50_000);
    }
}

#[test]
fn the_last_value_of_a_state_topic_comes_before_its_end() {
    let state = Publisher::<Bytes>::state().build(&count(1)).unwrap();
    let mut behind = subscribe(&state.as_binder());
    let mut sampled = subscribe(&state.as_binder());
    let mut caught_up = subscribe(&state.as_binder());
    state.publish(&count(2)).unwrap();
    assert_eq!(caught_up.recv_timeout(WAIT), Ok(count(2)));
    state.close();
    let ended = Err(RecvError::Closed(Closed::End));
    assert_eq!(behind.recv_timeout(WAIT), Ok(count(2)));
    assert_eq!(behind.recv_timeout(WAIT), ended);
    assert_eq!(behind.recv_timeout(WAIT), ended);
    let sample = sampled.recv_sample_timeout(WAIT).unwrap();
    assert_eq!(counted(&sample.to_owned()), 2);
    drop(sample);
    assert_eq!(sampled.recv_timeout(WAIT), ended);
    // Returned already: the end at once, and the value not again.
    assert_eq!(caught_up.recv_timeout(WAIT), ended);
}

#[test]
fn a_topic_closed_as_a_subscriber_attaches_returns_its_last_value_before_the_end() {
    let settings = Settings::default();
    let ended = Err(RecvError::Closed(Closed::End));
    let state = Publisher::<Bytes>::state().build(&count(1)).unwrap();
    let topic = state.as_binder();
    let mut subscriber = Subscriber::<Bytes>::request_starting(&topic, &settings, |_, _| {
        state.close();
        (Start::Auto, None)
    })
    .unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(count(1)));
    assert_eq!(subscriber.recv_timeout(WAIT), ended);

    let events = Publisher::<Bytes>::events().build().unwrap();
    events.publish(&count(2)).unwrap();
    let topic = events.as_binder();
    let mut subscriber = Subscriber::<Bytes>::request_starting(&topic, &settings, |_, _| {
        events.close();
        (Start::Newest, None)
    })
    .unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(count(2)));
    assert_eq!(subscriber.recv_timeout(WAIT), ended);
}

#[test]
fn the_last_value_lent_on_a_state_topic_comes_before_its_end() {
    let state = Publisher::<Bytes>::state()
        .max_subscribers(2)
        .lend(64)
        .build_with(|publisher| {
            let mut slot = publisher.loan_slice(4)?;
            slot.copy_from_slice(&1u32.to_le_bytes());
            slot.publish()
        })
        .unwrap();
    let mut subscriber = subscribe(&state.as_binder());
    let mut slot = state.loan_slice(4).unwrap();
    slot.copy_from_slice(&2u32.to_le_bytes());
    slot.publish().unwrap();
    state.close();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(count(2)));
    let ended = Err(RecvError::Closed(Closed::End));
    assert_eq!(subscriber.recv_timeout(WAIT), ended);
}

#[test]
fn an_endpoint_without_a_kind_is_of_an_event_topic() {
    // The parcelable ends before `topicKind`, which then reads as its default, as do the
    // fields after it.
    let sent = TopicEndpoint {
        topicKind: TopicKind::State.wire(),
        id: 7,
        ..Default::default()
    };
    let read = cut_from_kind(&sent);
    assert_eq!(TopicKind::from_wire(read.topicKind), Some(TopicKind::Event));
    assert_eq!(read.id, 0);
}

/// `sent` as a subscriber reads it from a publisher that writes no field from `topicKind` on.
fn cut_from_kind(sent: &TopicEndpoint) -> TopicEndpoint {
    // The fields as the parcelable writes them, each in turn.
    let mut tail = Parcel::new();
    tail.write(&sent.topicKind).unwrap();
    tail.write(&sent.id).unwrap();
    let tail = tail.data_size() as i32;
    let mut parcel = Parcel::new();
    parcel.write(&sent).unwrap();
    let end = parcel.data_size();
    parcel.set_data_position(0);
    let present: i32 = parcel.read().unwrap();
    assert_eq!(present, 1, "a parcelable that is not null");
    let start = parcel.data_position();
    let size: i32 = parcel.read().unwrap();
    assert_eq!(
        start + size as usize,
        end,
        "a parcelable sized from its size field"
    );
    parcel.set_data_position(start);
    parcel.write(&(size - tail)).unwrap();
    parcel.set_data_position(0);
    parcel.read().unwrap()
}
