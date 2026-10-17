// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A topic that waits for its subscribers (plan 15 §6.5): every value reaches every subscriber,
//! however far the ring goes round, and a subscription that ended is not waited for.

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

/// The number `value` put in `bytes`.
fn number(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

/// Receive `count` values, sleeping a pseudo-random while of up to `pause` after each; the
/// numbers they carry.
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

/// Publish two rings' worth from `from` on: a subscriber that reads none holds one of them up.
fn overrun(publisher: &Publisher<Bytes>, from: u32) -> u32 {
    let end = from + 2 * MIN_RING as u32 / 72;
    for n in from..end {
        publisher.publish(&value(n)).unwrap();
    }
    end
}

/// AC-15.3.18: a subscriber of the publisher's own process that holds a publish up past the
/// timeout is taken out, its notice handled on the publishing thread without a deadlock, while
/// a subscriber that keeps up is left alone and loses nothing.
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

/// AC-15.3.20: with a timeout of zero a publish takes out at once the subscriber in its way,
/// and only that one.
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

/// AC-15.3.17: the end record waits for room as a value does, so a publisher dropped right after
/// its last value leaves a subscriber every value and then the end.
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

/// AC-15.3.19: once taken out, a subscription woken by its fd ends every wait at once: a poll is
/// not prepared, and `recv` and `recv_async` return `Closed(Evicted)` without sleeping.
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

const SLOT: usize = 4096;

/// A topic of `max_subscribers` with the smallest ring and a pool of `max_subscribers + 3` slots
/// (a depth of 1, one loan), which waits up to `timeout`.
fn pooled(max_subscribers: usize, timeout: Duration) -> Publisher<Bytes> {
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
        backpressure: Some(Backpressure { timeout }),
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

/// AC-15.3.8 (loan): four subscribers, two woken by the futex and two by their fds, two copying
/// each value out and two reading it where it lies, receive every one of values that go round a
/// pool of seven slots many times.
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

/// AC-15.3.15 (pool): a lend waits for a pin as well as for a read. The one slot read past is
/// pinned; the wait ends when the sample lets go of it, though the subscriber reads no further.
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

/// AC-15.3.15 (pool): past the timeout a lend takes out the subscriber furthest behind, and no
/// more once a slot is read past: the one that read two values stays and loses none.
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
/// Once the lend takes out the subscriber furthest behind, the only slots read past are its
/// sample's and the other subscriber's, both pinned: a lend that passed over the pin of a
/// subscriber taken out would choose the first, the newer of the two read past.
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
    assert!(![a, b].contains(&slot), "slot {slot}, pinned");
    assert_eq!(publisher.stats().evicted, 2);
    assert_eq!(&*held, &value(0).0[..]);
    assert_eq!(&*also, &value(1).0[..]);
    drop((held, also));
    let evicted = Err(RecvError::Closed(Closed::Evicted));
    assert_eq!(first.try_recv(), evicted);
    assert_eq!(second.try_recv(), evicted);
}
