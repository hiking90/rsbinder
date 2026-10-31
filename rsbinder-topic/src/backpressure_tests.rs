// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A topic that waits for its subscribers (plan 15 §6.5): every value reaches every subscriber,
//! however far the ring goes round, and a subscription that ended is not waited for.

use std::num::NonZeroUsize;
use std::thread;
use std::time::{Duration, Instant};

use crate::layout::MIN_RING;
use crate::{
    Backpressure, Bytes, Closed, PoolPolicy, Publisher, PublisherPolicy, RecvError, Subscriber,
    SubscriberPolicy, Trusted, Wake,
};

const SPAN: Duration = Duration::from_secs(10);

/// A topic with the smallest ring, which waits up to `timeout`.
fn waiting(timeout: Duration) -> Publisher<Bytes> {
    within(timeout, None)
}

/// [`waiting`], for a subscriber more than `window` records behind as well.
fn within(timeout: Duration, window: Option<NonZeroUsize>) -> Publisher<Bytes> {
    Publisher::new(&PublisherPolicy {
        ring_bytes: MIN_RING,
        backpressure: Some(Backpressure {
            publish_timeout: timeout,
            window,
        }),
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

/// The number `value` put in `bytes`.
fn number(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

/// Receive `count` values, sleeping a pseudo-random while up to `pause` after each; their numbers.
fn receive(subscriber: Subscriber<Bytes>, count: u32, pause: Duration, seed: u32) -> Vec<u32> {
    receive_as(subscriber, count, pause, seed, false)
}

/// [`receive`], each value as a sample where it lies with `samples`.
fn receive_as(
    mut subscriber: Subscriber<Bytes>,
    count: u32,
    pause: Duration,
    seed: u32,
    samples: bool,
) -> Vec<u32> {
    let mut got = Vec::new();
    let mut state = seed | 1;
    while got.len() < count as usize {
        let received = if samples {
            subscriber
                .recv_sample_timeout(SPAN)
                .map(|sample| number(&sample.to_owned()))
        } else {
            subscriber
                .recv_timeout(SPAN)
                .map(|Bytes(bytes)| number(&bytes))
        };
        match received {
            Ok(n) => got.push(n),
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

/// AC-15.3.8 (ring): two futex and two fd subscribers that pause get every value of many laps.
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

/// AC-15.3.16: a batch larger than the ring announces before each wait, so sleepers make its room.
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

/// Publish two rings' worth from `from` on: a subscriber that reads none holds one of them up.
fn overrun(publisher: &Publisher<Bytes>, from: u32) -> u32 {
    let end = from + 2 * MIN_RING as u32 / 72;
    for n in from..end {
        publisher.publish(&value(n)).unwrap();
    }
    end
}

/// AC-15.3.18: a local subscriber that holds a publish past the timeout is taken out, no deadlock.
#[test]
fn a_subscriber_that_holds_a_publish_up_past_the_timeout_is_taken_out() {
    let timeout = Duration::from_millis(100);
    let publisher = waiting(timeout);
    let mut idle = subscribe(&publisher, Wake::Futex);
    let keeper = subscribe(&publisher, Wake::Fd);
    let began = Instant::now();
    let end = thread::scope(|scope| {
        let receiving =
            scope.spawn(|| receive(keeper, 2 * MIN_RING as u32 / 72, Duration::ZERO, 3));
        let end = overrun(&publisher, 0);
        assert_eq!(receiving.join().unwrap(), (0..end).collect::<Vec<_>>());
        end
    });
    let took = began.elapsed();
    assert!(took >= timeout, "{took:?}");
    assert!(took < timeout + Duration::from_secs(1), "{took:?}");
    let stats = publisher.stats();
    assert_eq!(stats.evicted, 1);
    let flags: Vec<bool> = stats.subscribers.iter().map(|info| info.evicted).collect();
    assert_eq!(flags.iter().filter(|&&out| out).count(), 1, "{flags:?}");
    let evicted = Err(RecvError::Closed(Closed::Evicted));
    // Told already: the values it never read are not returned, and nothing waits.
    let before = Instant::now();
    assert_eq!(idle.recv_timeout(SPAN), evicted);
    assert_eq!(idle.try_recv(), evicted);
    assert!(before.elapsed() < Duration::from_millis(100));
    // Taken out once: later values wait for nobody.
    let began = Instant::now();
    overrun(&publisher, end);
    assert!(began.elapsed() < timeout, "{:?}", began.elapsed());
    assert_eq!(publisher.stats().evicted, 1);
}

/// AC-15.3.20: a timeout of zero takes out at once the subscriber in the way, and only that one.
#[test]
fn a_zero_timeout_takes_out_at_once_only_the_subscriber_in_the_way() {
    let publisher = waiting(Duration::ZERO);
    let idle = subscribe(&publisher, Wake::Futex);
    let mut keeper = subscribe(&publisher, Wake::Futex);
    for n in 0..2 * MIN_RING as u32 / 72 {
        let began = Instant::now();
        publisher.publish(&value(n)).unwrap();
        assert!(began.elapsed() < Duration::from_millis(50), "value {n}");
        assert_eq!(keeper.try_recv(), Ok(value(n)));
    }
    assert_eq!(publisher.stats().evicted, 1);
    assert_eq!(keeper.stats().lost, 0);
    drop(idle);
}

/// AC-15.3.17: the end waits for room as a value does: a dropped publisher leaves every value.
#[test]
fn a_publisher_dropped_after_its_last_value_leaves_every_value_then_the_end() {
    let publisher = waiting(SPAN);
    let mut subscriber = subscribe(&publisher, Wake::Futex);
    let values = 2 * MIN_RING as u32 / 72;
    thread::scope(|scope| {
        let receiving = scope.spawn(move || {
            let mut got = Vec::new();
            loop {
                thread::sleep(Duration::from_micros(500));
                match subscriber.recv_timeout(SPAN) {
                    Ok(Bytes(bytes)) => {
                        got.push(u32::from_le_bytes(bytes[..4].try_into().unwrap()))
                    }
                    Err(RecvError::Closed(Closed::End)) => return got,
                    Err(error) => panic!("after {} values: {error:?}", got.len()),
                }
            }
        });
        for n in 0..values {
            publisher.publish(&value(n)).unwrap();
        }
        drop(publisher);
        assert_eq!(receiving.join().unwrap(), (0..values).collect::<Vec<_>>());
    });
}

/// AC-15.3.19: an fd subscription taken out ends every wait at once: no poll, `Closed(Evicted)`.
#[test]
fn a_subscriber_taken_out_ends_its_waits_at_once() {
    let publisher = waiting(Duration::ZERO);
    let mut idle = subscribe(&publisher, Wake::Fd);
    overrun(&publisher, 0);
    assert_eq!(publisher.stats().evicted, 1);
    assert!(!crate::Wait::prepare_wait(&mut idle));
    let evicted = Err(RecvError::Closed(Closed::Evicted));
    let began = Instant::now();
    assert_eq!(idle.recv(), evicted);
    #[cfg(feature = "tokio")]
    {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert_eq!(runtime.block_on(idle.recv_async()), evicted);
    }
    let took = began.elapsed();
    assert!(took < Duration::from_millis(100), "{took:?}");
}

/// AC-15.3.15 (ended): a publish waiting for a subscriber ends once it drops, before the timeout.
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

const SLOT: usize = 4096;

/// `max_subscribers`, the smallest ring, `max_subscribers + 3` slots (depth 1), waits of `timeout`.
fn pooled(max_subscribers: usize, timeout: Duration) -> Publisher<Bytes> {
    pooled_within(max_subscribers, timeout, None)
}

/// [`pooled`], for a subscriber more than `window` records behind as well.
fn pooled_within(
    max_subscribers: usize,
    timeout: Duration,
    window: Option<NonZeroUsize>,
) -> Publisher<Bytes> {
    Publisher::new(&PublisherPolicy {
        ring_bytes: MIN_RING,
        max_subscribers,
        max_per_uid: max_subscribers,
        pool: Some(PoolPolicy {
            slot_bytes: SLOT,
            depth: 1,
            max_loans: 1,
            huge_pages: false,
        }),
        backpressure: Some(Backpressure {
            publish_timeout: timeout,
            window,
        }),
        ..PublisherPolicy::default()
    })
    .unwrap()
}

/// Lend a slot, write `value(n)` into it and publish it; the slot's number.
fn lend(publisher: &Publisher<Bytes>, n: u32) -> u32 {
    let Bytes(bytes) = value(n);
    let mut loan = publisher.loan_slice(bytes.len()).unwrap();
    loan.copy_from_slice(&bytes);
    let slot = loan.slot();
    loan.publish().unwrap();
    slot
}

/// AC-15.3.8 (loan): futex and fd subscribers, copying or in place, get every value of many laps.
#[test]
fn every_loaned_value_reaches_every_subscriber_however_often_the_pool_goes_round() {
    const VALUES: u32 = 2000;
    let publisher = pooled(4, SPAN);
    let wakes = [Wake::Futex, Wake::Fd, Wake::Futex, Wake::Fd];
    let subscribers: Vec<_> = wakes.map(|wake| subscribe(&publisher, wake)).into();
    thread::scope(|scope| {
        let receiving: Vec<_> = subscribers
            .into_iter()
            .enumerate()
            .map(|(n, subscriber)| {
                let pause = Duration::from_micros(200);
                let seed = n as u32 + 11;
                scope.spawn(move || receive_as(subscriber, VALUES, pause, seed, n >= 2))
            })
            .collect();
        for n in 0..VALUES {
            lend(&publisher, n);
        }
        for receiving in receiving {
            assert_eq!(receiving.join().unwrap(), (0..VALUES).collect::<Vec<_>>());
        }
    });
    let stats = publisher.stats();
    assert_eq!((stats.published, stats.evicted), (u64::from(VALUES), 0));
}

/// AC-15.3.15 (pool): a lend waits for a pin as for a read, until the sample lets go of its slot.
#[test]
fn a_lend_waits_for_a_pin_to_let_go_of_the_one_slot_read_past() {
    let timeout = Duration::from_secs(5);
    let publisher = pooled(1, timeout);
    let mut subscriber = subscribe(&publisher, Wake::Futex);
    // Four slots: the first value held as a sample, three nobody has read.
    let first = lend(&publisher, 0);
    let (held, release) = std::sync::mpsc::channel();
    let began = thread::scope(|scope| {
        // Returned, so that the subscription lives on past the sample, still waited for.
        let holding = scope.spawn(move || {
            let sample = subscriber.recv_sample_timeout(SPAN).unwrap();
            held.send(()).unwrap();
            thread::sleep(Duration::from_millis(200));
            drop(sample);
            subscriber
        });
        release.recv().unwrap();
        for n in 1..4 {
            lend(&publisher, n);
        }
        let began = Instant::now();
        assert_eq!(lend(&publisher, 4), first, "the slot the sample let go of");
        let subscriber = holding.join().unwrap();
        assert_eq!(subscriber.stats().received, 1);
        began
    });
    let took = began.elapsed();
    assert!(took >= Duration::from_millis(100), "none waited: {took:?}");
    assert!(took < timeout - Duration::from_secs(2), "{took:?}");
    assert_eq!(publisher.stats().evicted, 0);
}

/// AC-15.3.15 (pool): past the timeout a lend takes out only the subscriber furthest behind.
#[test]
fn a_lend_past_the_timeout_takes_out_only_the_subscriber_furthest_behind() {
    let timeout = Duration::from_millis(100);
    let publisher = pooled(2, timeout);
    let mut idle = subscribe(&publisher, Wake::Futex);
    let mut reader = subscribe(&publisher, Wake::Futex);
    for n in 0..5 {
        lend(&publisher, n);
    }
    assert_eq!(reader.try_recv(), Ok(value(0)));
    assert_eq!(reader.try_recv(), Ok(value(1)));
    let began = Instant::now();
    lend(&publisher, 5);
    let took = began.elapsed();
    assert!(took >= timeout, "{took:?}");
    assert!(took < timeout + Duration::from_secs(1), "{took:?}");
    let stats = publisher.stats();
    assert_eq!(stats.evicted, 1);
    let flags: Vec<bool> = stats.subscribers.iter().map(|info| info.evicted).collect();
    assert_eq!(flags, [true, false]);
    for n in 2..6 {
        assert_eq!(reader.try_recv(), Ok(value(n)));
    }
    assert_eq!(reader.stats().lost, 0);
    assert_eq!(idle.try_recv(), Err(RecvError::Closed(Closed::Evicted)));
}

/// AC-15.3.10: a subscriber taken out keeps the slot of the sample it holds until it lets go.
#[test]
fn a_sample_held_by_a_subscriber_taken_out_keeps_its_slot() {
    let timeout = Duration::from_millis(100);
    let publisher = pooled(2, timeout);
    let subscribe_trusted = |publisher: &Publisher<Bytes>| -> Subscriber<Bytes, Trusted> {
        // SAFETY: the publisher is this crate's, in this process.
        unsafe { subscribe(publisher, Wake::Futex).trust_publisher() }
    };
    let mut first = subscribe_trusted(&publisher);
    let mut second = subscribe_trusted(&publisher);
    let a = lend(&publisher, 0);
    let held = first.try_recv_sample().unwrap();
    assert_eq!(second.try_recv(), Ok(value(0)));
    let b = lend(&publisher, 1);
    let also = second.try_recv_sample().unwrap();
    // Three slots nobody has read past.
    for n in 2..5 {
        lend(&publisher, n);
    }
    let began = Instant::now();
    let slot = lend(&publisher, 5);
    let took = began.elapsed();
    assert!(took >= timeout, "{took:?}");
    assert!(
        ![a, b].contains(&slot),
        "slot {slot}: pinned by a subscriber taken out, the only slots read past"
    );
    assert_eq!(publisher.stats().evicted, 2);
    assert_eq!(&*held, &value(0).0[..]);
    assert_eq!(&*also, &value(1).0[..]);
    drop((held, also));
    let evicted = Err(RecvError::Closed(Closed::Evicted));
    assert_eq!(first.try_recv(), evicted);
    assert_eq!(second.try_recv(), evicted);
}

/// A window of `w` (1, 2, 3) holds up the publish that leaves a subscriber `w + 1` records behind.
#[test]
fn a_publish_that_would_leave_a_subscriber_past_the_window_waits_for_it() {
    let timeout = Duration::from_millis(100);
    for window in 1..=3 {
        let publisher = within(timeout, NonZeroUsize::new(window as usize));
        let mut subscriber = subscribe(&publisher, Wake::Futex);
        let quick = |n: u32| {
            let began = Instant::now();
            publisher.publish(&value(n)).unwrap();
            assert!(began.elapsed() < timeout / 2, "window {window}, value {n}");
        };
        for n in 0..window {
            quick(n);
        }
        // One read lets one more go.
        assert_eq!(subscriber.try_recv(), Ok(value(0)));
        quick(window);
        assert_eq!(publisher.stats().evicted, 0, "window {window}");
        let began = Instant::now();
        publisher.publish(&value(window + 1)).unwrap();
        let took = began.elapsed();
        assert!(took >= timeout, "window {window}: {took:?}");
        assert_eq!(publisher.stats().evicted, 1, "window {window}");
        let evicted = Err(RecvError::Closed(Closed::Evicted));
        assert_eq!(subscriber.recv_timeout(SPAN), evicted);
    }
}

/// With a window of 1 each publish waits for a futex and an fd subscriber; both get every value.
#[test]
fn every_value_reaches_every_subscriber_through_a_window_of_one() {
    const VALUES: u32 = 1000;
    let publisher = within(SPAN, NonZeroUsize::new(1));
    let subscribers = [
        subscribe(&publisher, Wake::Futex),
        subscribe(&publisher, Wake::Fd),
    ];
    thread::scope(|scope| {
        let receiving: Vec<_> = subscribers
            .into_iter()
            .enumerate()
            .map(|(n, subscriber)| {
                let pause = Duration::from_micros(100);
                scope.spawn(move || receive(subscriber, VALUES, pause, n as u32 + 5))
            })
            .collect();
        for n in 0..VALUES {
            publisher.publish(&value(n)).unwrap();
        }
        for receiving in receiving {
            assert_eq!(receiving.join().unwrap(), (0..VALUES).collect::<Vec<_>>());
        }
    });
    assert_eq!(publisher.stats().evicted, 0);
}

/// A lend waits for its record's window, not only for a slot: with 2, a free slot lets none go.
#[test]
fn a_lend_past_the_window_waits_though_a_slot_is_free() {
    let timeout = Duration::from_millis(100);
    let publisher = pooled_within(1, timeout, NonZeroUsize::new(2));
    let mut subscriber = subscribe(&publisher, Wake::Futex);
    lend(&publisher, 0);
    lend(&publisher, 1);
    assert_eq!(subscriber.try_recv(), Ok(value(0)));
    let began = Instant::now();
    lend(&publisher, 2);
    assert!(began.elapsed() < timeout / 2, "{:?}", began.elapsed());
    let Bytes(bytes) = value(3);
    let began = Instant::now();
    let mut loan = publisher.loan_slice(bytes.len()).unwrap();
    let took = began.elapsed();
    assert!(took >= timeout, "{took:?}");
    loan.copy_from_slice(&bytes);
    let began = Instant::now();
    loan.publish().unwrap();
    assert!(began.elapsed() < timeout / 2, "{:?}", began.elapsed());
    assert_eq!(publisher.stats().evicted, 1);
    let evicted = Err(RecvError::Closed(Closed::Evicted));
    assert_eq!(subscriber.recv_timeout(SPAN), evicted);
}

/// A loaned value counts as read once its sample drops: the lag covers its record until then.
#[test]
fn a_held_sample_leaves_its_record_unread_until_dropped() {
    let publisher = pooled(1, SPAN);
    let mut subscriber = subscribe(&publisher, Wake::Futex);
    lend(&publisher, 0);
    let lag = |publisher: &Publisher<Bytes>| publisher.stats().subscribers[0].lag_bytes;
    let sample = subscriber.try_recv_sample().unwrap();
    assert!(lag(&publisher) > 0);
    drop(sample);
    assert_eq!(lag(&publisher), 0);
}

/// A held sample of the newest value leaves its own record unread alone, not those it skipped.
#[test]
fn a_held_newest_sample_leaves_only_its_own_record_unread() {
    let publisher = pooled(1, SPAN);
    let mut subscriber = subscribe(&publisher, Wake::Futex);
    let lag = |publisher: &Publisher<Bytes>| publisher.stats().subscribers[0].lag_bytes;
    lend(&publisher, 0);
    let record = lag(&publisher);
    lend(&publisher, 1);
    lend(&publisher, 2);
    assert_eq!(lag(&publisher), 3 * record);
    let sample = subscriber.try_latest_sample().unwrap();
    assert_eq!(number(&sample.to_owned()), 2);
    assert_eq!(lag(&publisher), record);
    drop(sample);
    assert_eq!(lag(&publisher), 0);
    assert_eq!(subscriber.stats().lost, 0);
}

/// With a window of 1 a lend waits for a held sample of the value before it to be dropped.
#[test]
fn a_lend_inside_the_window_waits_for_a_held_sample_to_drop() {
    let timeout = Duration::from_secs(5);
    let publisher = pooled_within(1, timeout, NonZeroUsize::new(1));
    let mut subscriber = subscribe(&publisher, Wake::Futex);
    lend(&publisher, 0);
    let (held, release) = std::sync::mpsc::channel();
    let took = thread::scope(|scope| {
        let holding = scope.spawn(move || {
            let sample = subscriber.recv_sample_timeout(SPAN).unwrap();
            held.send(()).unwrap();
            thread::sleep(Duration::from_millis(200));
            drop(sample);
            subscriber
        });
        release.recv().unwrap();
        let began = Instant::now();
        lend(&publisher, 1);
        let took = began.elapsed();
        let mut subscriber = holding.join().unwrap();
        assert_eq!(subscriber.try_recv(), Ok(value(1)));
        took
    });
    assert!(took >= Duration::from_millis(100), "none waited: {took:?}");
    assert!(took < timeout - Duration::from_secs(2), "{took:?}");
    assert_eq!(publisher.stats().evicted, 0);
}
