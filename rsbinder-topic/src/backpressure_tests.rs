// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A topic that waits for its subscribers (plan 15 §6.5): every value reaches every subscriber,
//! however far the ring goes round, and a subscription that ended is not waited for.

use std::thread;
use std::time::{Duration, Instant};

use crate::layout::MIN_RING;
use crate::{Backpressure, Bytes, Publisher, PublisherPolicy, Subscriber, SubscriberPolicy, Wake};

const SPAN: Duration = Duration::from_secs(10);

/// A topic with the smallest ring, which waits up to `timeout`.
fn waiting(timeout: Duration) -> Publisher<Bytes> {
    Publisher::new(&PublisherPolicy {
        ring_bytes: MIN_RING,
        backpressure: Some(Backpressure { timeout }),
        ..PublisherPolicy::default()
    })
    .unwrap()
}

fn subscribe(publisher: &Publisher<Bytes>, wake: Wake) -> Subscriber<Bytes> {
    let policy = SubscriberPolicy {
        wake,
        ..SubscriberPolicy::default()
    };
    Subscriber::request(&publisher.as_binder(), &policy).unwrap()
}

/// 64 bytes that carry `n`.
fn value(n: u32) -> Bytes {
    let mut bytes = vec![0; 64];
    bytes[..4].copy_from_slice(&n.to_le_bytes());
    Bytes(bytes)
}

/// Receive `count` values, sleeping a pseudo-random while of up to `pause` after each; the
/// numbers they carry.
fn receive(mut subscriber: Subscriber<Bytes>, count: u32, pause: Duration, seed: u32) -> Vec<u32> {
    let mut got = Vec::new();
    let mut state = seed | 1;
    while got.len() < count as usize {
        match subscriber.recv_timeout(SPAN) {
            Ok(Bytes(bytes)) => got.push(u32::from_le_bytes(bytes[..4].try_into().unwrap())),
            Err(error) => panic!("after {} values: {error:?}", got.len()),
        }
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        thread::sleep(pause.mul_f64(f64::from(state % 1000) / 1000.0));
    }
    assert_eq!(subscriber.stats().lost, 0);
    got
}

/// AC-15.3.8 (ring): four subscribers, two woken by the futex and two by their fds, each
/// sleeping up to a tenth of the timeout after a value, receive every one of values that go
/// round the ring many times.
#[test]
fn every_value_reaches_every_subscriber_however_often_the_ring_goes_round() {
    const VALUES: u32 = 3000;
    let publisher = waiting(SPAN);
    let wakes = [Wake::Futex, Wake::Fd, Wake::Futex, Wake::Fd];
    let subscribers: Vec<_> = wakes.map(|wake| subscribe(&publisher, wake)).into();
    thread::scope(|scope| {
        let receiving: Vec<_> = subscribers
            .into_iter()
            .enumerate()
            .map(|(n, subscriber)| {
                let pause = Duration::from_micros(200);
                scope.spawn(move || receive(subscriber, VALUES, pause, n as u32 + 7))
            })
            .collect();
        for n in 0..VALUES {
            publisher.publish(&value(n)).unwrap();
        }
        for receiving in receiving {
            assert_eq!(receiving.join().unwrap(), (0..VALUES).collect::<Vec<_>>());
        }
    });
    assert_eq!(publisher.stats().published, u64::from(VALUES));
}

/// AC-15.3.16: a batch larger than the ring announces what it wrote before each wait, so
/// subscribers asleep before it wake, read on, and make the room it waits for.
#[test]
fn a_batch_larger_than_the_ring_wakes_the_subscribers_it_waits_for() {
    const VALUES: u32 = 500;
    // Short: without the announcements every wait would time out and overwrite.
    let publisher = waiting(Duration::from_millis(500));
    let subscribers = [
        subscribe(&publisher, Wake::Futex),
        subscribe(&publisher, Wake::Fd),
    ];
    thread::scope(|scope| {
        let receiving: Vec<_> = subscribers
            .into_iter()
            .map(|subscriber| scope.spawn(move || receive(subscriber, VALUES, Duration::ZERO, 1)))
            .collect();
        // Both asleep in `recv` before the batch.
        thread::sleep(Duration::from_millis(50));
        let values: Vec<Bytes> = (0..VALUES).map(value).collect();
        assert_eq!(publisher.publish_all(&values), Ok(VALUES as usize));
        for receiving in receiving {
            assert_eq!(receiving.join().unwrap(), (0..VALUES).collect::<Vec<_>>());
        }
    });
}

/// AC-15.3.15 (a subscription that ended): a publish waiting for a subscriber a ring behind ends
/// once that subscriber drops its subscription, well before the timeout.
#[test]
fn a_wait_ends_when_the_subscriber_it_waits_for_drops_its_subscription() {
    let timeout = Duration::from_secs(5);
    let publisher = waiting(timeout);
    let behind = subscribe(&publisher, Wake::Futex);
    let began = Instant::now();
    thread::scope(|scope| {
        scope.spawn(move || {
            thread::sleep(Duration::from_millis(200));
            drop(behind);
        });
        // Two rings' worth: the values past the first ring wait for `behind` until it goes.
        for n in 0..2 * MIN_RING as u32 / 72 {
            publisher.publish(&value(n)).unwrap();
        }
    });
    let took = began.elapsed();
    assert!(took >= Duration::from_millis(200), "none waited: {took:?}");
    assert!(took < timeout - Duration::from_secs(2), "{took:?}");
}
