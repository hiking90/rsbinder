// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Receives that wait, within one process (plan 15 §5.5): the publisher's one wake, the wakes
//! from this process, and the join of a wait a dropped `recv_async` left on the pool. A publisher
//! that dies is in `tests/two_process.rs`; the protocol under every interleaving is in
//! `wait/loom_tests.rs`.
//!
//! "Futex system calls" here are the publisher's wakes, which it counts: the lock it takes is
//! uncontended in these tests, and a waiting subscriber's own sleep is not the publisher's.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::Status;

use crate::{
    Bytes, Closed, Publisher, PublisherPolicy, RecvError, Start, Subscriber, SubscriberPolicy, Wake,
};

fn num(n: u32) -> Bytes {
    Bytes(n.to_le_bytes().to_vec())
}

fn value(bytes: &Bytes) -> u32 {
    u32::from_le_bytes(bytes.0[..4].try_into().unwrap())
}

fn topic(ring_bytes: usize) -> Publisher<Bytes> {
    Publisher::new(&PublisherPolicy {
        ring_bytes,
        ..PublisherPolicy::default()
    })
    .unwrap()
}

fn subscriber(publisher: &Publisher<Bytes>) -> Subscriber<Bytes> {
    let policy = SubscriberPolicy {
        start: Start::Latest,
        ..SubscriberPolicy::default()
    };
    Subscriber::request(&publisher.as_binder(), &policy).unwrap()
}

/// Poll `holds` until it does, failing after 10 s.
fn eventually(what: &str, mut holds: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !holds() {
        assert!(Instant::now() < end, "never: {what}");
        thread::sleep(Duration::from_micros(50));
    }
}

/// Join `receiving` once it returns, failing after 10 s: a lost wake fails a test, not hangs it.
fn joined<T>(receiving: thread::JoinHandle<T>) -> T {
    eventually("the receive returns", || receiving.is_finished());
    receiving.join().unwrap()
}

/// `recv` on a thread of its own, once the subscriber sleeps; its results by value.
fn asleep(
    mut subscriber: Subscriber<Bytes>,
    receive: fn(&mut Subscriber<Bytes>) -> Result<Bytes, RecvError>,
) -> thread::JoinHandle<Result<u32, RecvError>> {
    let local = subscriber.local().clone();
    let receiving = thread::spawn(move || receive(&mut subscriber).map(|bytes| value(&bytes)));
    eventually("the subscriber sleeps", || local.is_parked());
    receiving
}

#[test]
fn a_receive_sleeps_until_the_publisher_commits_and_one_wake_ends_it() {
    let publisher = topic(64 << 10);
    let receiving = asleep(subscriber(&publisher), Subscriber::recv);
    publisher.publish(&num(7)).unwrap();
    assert_eq!(joined(receiving), Ok(7));
    assert_eq!(publisher.wakes(), 1);
}

/// A wait that unwinds between its arm and its sleep is disarmed: `parked` and `WAITING` down.
#[test]
fn a_wait_that_unwinds_before_it_sleeps_is_disarmed() {
    let publisher = topic(64 << 10);
    let mut subscriber = subscriber(&publisher);
    let local = subscriber.local().clone();
    *local.before_sleep.lock().unwrap() = Some(Box::new(|| panic!("a sleep that unwinds")));
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| subscriber.recv()));
    // The pause's own panic: an unwind past the arm, not one before it.
    let payload = unwound.expect_err("the sleep unwinds");
    assert_eq!(
        payload.downcast_ref::<&str>(),
        Some(&"a sleep that unwinds")
    );
    let state = local.words().control.state().load(Ordering::SeqCst);
    let (parked, waiting) = (local.is_parked(), state & crate::layout::WAITING != 0);
    if parked {
        // Its drop would wake a wait left `parked` until it is out, which it never is.
        std::mem::forget(subscriber);
    }
    assert!(!parked && !waiting, "parked {parked}, WAITING {waiting}");
}

/// AC-15.2.6: no subscriber sleeps, so no publish makes a system call.
#[test]
fn publishes_with_no_subscriber_asleep_wake_nobody() {
    let publisher = topic(64 << 10);
    let mut awake = subscriber(&publisher);
    for n in 0..10_000 {
        publisher.publish(&num(n)).unwrap();
        // A receive that finds a record does not sleep, so the next publish has none to wake.
        if n % 7 == 0 {
            while awake.try_recv().is_ok() {}
        }
    }
    assert_eq!(publisher.wakes(), 0);
}

/// AC-15.2.7: eight subscribers asleep, one publish, one wake.
#[test]
fn one_publish_wakes_eight_sleeping_subscribers_with_one_system_call() {
    let publisher = topic(64 << 10);
    let receiving: Vec<_> = (0..8)
        .map(|_| asleep(subscriber(&publisher), Subscriber::recv))
        .collect();
    publisher.publish(&num(1)).unwrap();
    assert_eq!(publisher.wakes(), 1);
    for receiving in receiving {
        assert_eq!(joined(receiving), Ok(1));
    }
}

/// AC-15.2.19: one wait, one wake, however many commits land before it lowers `WAITING`.
#[test]
fn a_hundred_publishes_before_the_woken_wait_ends_send_one_wake() {
    let publisher = topic(64 << 10);
    let subscriber = subscriber(&publisher);
    let release = Arc::new(AtomicBool::new(false));
    let held = release.clone();
    *subscriber.local().after_sleep.lock().unwrap() = Some(Box::new(move || {
        while !held.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_micros(100));
        }
    }));
    let receiving = asleep(subscriber, Subscriber::recv);
    for n in 0..100 {
        publisher.publish(&num(n)).unwrap();
    }
    assert_eq!(publisher.wakes(), 1);
    release.store(true, Ordering::SeqCst);
    assert_eq!(joined(receiving), Ok(0));
}

/// AC-15.2.37: `publish_all` wakes once.
#[test]
fn a_batch_is_announced_with_one_wake() {
    let publisher = topic(64 << 10);
    let receiving = asleep(subscriber(&publisher), Subscriber::recv);
    assert_eq!(publisher.publish_all(&[num(1), num(2), num(3)]), Ok(3));
    assert_eq!(publisher.wakes(), 1);
    assert_eq!(joined(receiving), Ok(1));
}

/// AC-15.2.37: the end reaches a sleeping subscriber with the status it was given.
#[test]
fn a_close_wakes_a_sleeping_subscriber_with_its_status() {
    let publisher = topic(64 << 10);
    let receiving = asleep(subscriber(&publisher), Subscriber::recv);
    let status = Status::new_service_specific_error(5, Some("over".into()));
    publisher.close_with(&status);
    assert_eq!(
        joined(receiving),
        Err(RecvError::Closed(Closed::Status(status)))
    );

    // A dropped publisher ends a wait the same way, with no error.
    let publisher = topic(64 << 10);
    let receiving = asleep(subscriber(&publisher), Subscriber::recv);
    drop(publisher);
    assert_eq!(joined(receiving), Err(RecvError::Closed(Closed::End)));
}

/// AC-15.2.37: a bounded wait ends at its bound, and the subscription goes on.
#[test]
fn a_bounded_wait_times_out_and_the_next_receive_still_works() {
    let publisher = topic(64 << 10);
    let mut subscriber = subscriber(&publisher);
    let started = Instant::now();
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(30)),
        Err(RecvError::Timeout)
    );
    assert!(started.elapsed() >= Duration::from_millis(30));
    assert_eq!(
        subscriber.recv_timeout(Duration::ZERO),
        Err(RecvError::Timeout)
    );
    publisher.publish(&num(4)).unwrap();
    let got = subscriber.recv_timeout(Duration::from_secs(5));
    assert_eq!(got.map(|bytes| value(&bytes)), Ok(4));
    // `Duration::MAX` cannot be a deadline, so it is no deadline.
    publisher.publish(&num(5)).unwrap();
    let got = subscriber.recv_timeout(Duration::MAX);
    assert_eq!(got.map(|bytes| value(&bytes)), Ok(5));
}

/// AC-15.2.37: `recv_latest` waits for some record, then returns the newest.
#[test]
fn the_latest_receive_waits_then_skips_to_the_newest() {
    let publisher = topic(64 << 10);
    let receiving = asleep(subscriber(&publisher), Subscriber::recv_latest);
    assert_eq!(publisher.publish_all(&[num(1), num(2), num(3)]), Ok(3));
    assert_eq!(joined(receiving), Ok(3));
}

/// AC-15.2.1: eight subscribers that keep up receive every record, in the same order.
#[test]
fn eight_subscribers_receive_the_same_records_in_order() {
    let publisher = topic(64 << 10);
    let receiving: Vec<_> = (0..8)
        .map(|_| {
            let mut subscriber = subscriber(&publisher);
            thread::spawn(move || {
                let mut got = Vec::new();
                for _ in 0..1000 {
                    let started = Instant::now();
                    let received = subscriber.recv_timeout(Duration::from_secs(5));
                    // A lost wake waits for the deadline: the test fails rather than hangs.
                    let received = match started.elapsed() < Duration::from_secs(1) {
                        true => received.map(|bytes| value(&bytes)),
                        false => Err(RecvError::Timeout),
                    };
                    let failed = received.is_err();
                    got.push(received);
                    if failed {
                        break;
                    }
                }
                got
            })
        })
        .collect();
    for n in 0..1000 {
        publisher.publish(&num(n)).unwrap();
    }
    let all: Vec<Result<u32, RecvError>> = (0..1000).map(Ok).collect();
    for receiving in receiving {
        assert_eq!(receiving.join().unwrap(), all);
    }
}

/// AC-15.2.2: a reader that stops is told how many records it lost; the others lose none.
#[test]
fn a_slow_subscriber_is_told_what_it_lost_and_the_others_lose_nothing() {
    let publisher = topic(64 << 10);
    let mut slow = subscriber(&publisher);
    let mut fast = subscriber(&publisher);
    let read = Arc::new(AtomicU32::new(0));
    // Up to the first error, so that a lag or a lost wake fails the test instead of hanging it.
    let receiving = thread::spawn({
        let read = read.clone();
        move || {
            let mut got = Vec::new();
            while got.len() < 20_000 {
                let received = fast.recv_timeout(Duration::from_secs(10));
                let stop = received.is_err();
                got.push(received.map(|bytes| value(&bytes)));
                read.fetch_add(1, Ordering::Relaxed);
                if stop {
                    break;
                }
            }
            got
        }
    });
    for n in 0..20_000u32 {
        // Half the ring ahead at most: a reader the scheduler holds back must not lag.
        while n.saturating_sub(read.load(Ordering::Relaxed)) > 2000 && !receiving.is_finished() {
            thread::sleep(Duration::from_micros(100));
        }
        publisher.publish(&num(n)).unwrap();
    }
    let all: Vec<Result<u32, RecvError>> = (0..20_000).map(Ok).collect();
    assert_eq!(receiving.join().unwrap(), all);
    let Err(RecvError::Lagged(lost)) = slow.recv() else {
        panic!("a reader 20000 records behind in a ring of 4096 lost some");
    };
    let first = value(&slow.recv().unwrap());
    assert_eq!(
        lost,
        u64::from(first),
        "the records before the first one left"
    );
}

/// AC-15.2.27: each `interrupt` of a waiter that waits again at once returns, after few wakes.
#[test]
fn ten_thousand_interrupts_of_a_waiter_that_waits_again_all_return() {
    let publisher = topic(64 << 10);
    let mut subscriber = subscriber(&publisher);
    let interrupter = subscriber.interrupter();
    let local = subscriber.local().clone();
    let receiving = thread::spawn(move || {
        let mut cancelled = 0u32;
        loop {
            match subscriber.recv() {
                Err(RecvError::Cancelled) => cancelled += 1,
                Ok(_) => return cancelled,
                other => panic!("{other:?}"),
            }
        }
    });
    let mut most = 0;
    for _ in 0..10_000 {
        // Each one at a wait that sleeps, so that each ends one.
        eventually("the subscriber sleeps again", || local.is_parked());
        most = most.max(interrupter.interrupt_counting());
    }
    publisher.publish(&num(0)).unwrap();
    assert_eq!(joined(receiving), 10_000);
    assert!(most >= 1, "every interrupt found the wait asleep");
    assert!(
        most <= WAKES_PER_INTERRUPT,
        "{most} wakes for one interrupt"
    );
}

/// Wakes one `interrupt` may send: 2 to 4 alone, 66 beside a build; without yielding, thousands.
const WAKES_PER_INTERRUPT: usize = 256;

/// AC-15.2.11 within a process: an interrupt ends eight waits within a second, a thousand times.
#[test]
fn interrupts_end_eight_waits_within_a_second() {
    let publisher = topic(64 << 10);
    for _ in 0..1000 {
        let subscribers: Vec<_> = (0..8).map(|_| subscriber(&publisher)).collect();
        let interrupters: Vec<_> = subscribers.iter().map(Subscriber::interrupter).collect();
        let receiving: Vec<_> = subscribers
            .into_iter()
            .map(|subscriber| asleep(subscriber, Subscriber::recv))
            .collect();
        let started = Instant::now();
        interrupters.iter().for_each(|i| i.interrupt());
        eventually("every wait ends", || {
            receiving.iter().all(|r| r.is_finished())
        });
        for receiving in receiving {
            assert_eq!(receiving.join().unwrap(), Err(RecvError::Cancelled));
        }
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

/// An interrupt with no wait in progress ends the next wait, once.
#[test]
fn an_interrupt_before_the_wait_ends_the_next_one() {
    let publisher = topic(64 << 10);
    let mut subscriber = subscriber(&publisher);
    subscriber.interrupter().interrupt();
    // A receive that does not wait leaves it for the next one that does.
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    // Bounded: a wait that slept past the interrupt would time out rather than hang the test.
    assert_eq!(
        subscriber.recv_timeout(Duration::from_secs(5)),
        Err(RecvError::Cancelled)
    );
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(10)),
        Err(RecvError::Timeout)
    );
    // After the subscriber, an interrupter does nothing.
    let interrupter = subscriber.interrupter();
    drop(subscriber);
    interrupter.interrupt();
}

/// A wake that lands before a wait on that commit's `seq` sleeps: the next commit still wakes it.
#[test]
fn a_wake_sent_before_the_wait_sleeps_does_not_stand_for_the_next_commit() {
    a_commit_after_a_wait_that_saw_the_last(Wake::Futex);
}

/// The same with the fd wake, whose byte would stay; the publisher sends none for such a wait.
#[test]
fn a_wait_that_saw_a_commit_on_the_fd_wake_is_woken_by_the_next() {
    a_commit_after_a_wait_that_saw_the_last(Wake::Fd);
}

fn a_commit_after_a_wait_that_saw_the_last(wake: Wake) {
    use std::sync::mpsc;
    let publisher = Arc::new(topic(64 << 10));
    let policy = SubscriberPolicy {
        wake,
        ..SubscriberPolicy::default()
    };
    let mut subscriber = Subscriber::request(&publisher.as_binder(), &policy).unwrap();
    // Once each: the first commit stops after its bump, the wait after `WAITING`.
    let (bumped, at_bump) = mpsc::sync_channel::<()>(0);
    let (resume, resumed) = mpsc::sync_channel::<()>(0);
    let pause = Arc::new(std::sync::Mutex::new(Some((bumped, resumed))));
    publisher.pause_after_bump(Box::new(move || {
        if let Some((bumped, resumed)) = pause.lock().unwrap().take() {
            bumped.send(()).unwrap();
            resumed.recv().unwrap();
        }
    }));
    let first = {
        let publisher = publisher.clone();
        thread::spawn(move || publisher.publish(&num(1)).unwrap())
    };
    at_bump.recv().unwrap();
    // Committed and `seq` bumped: read without waiting, then wait for the next record.
    assert_eq!(subscriber.try_recv().map(|bytes| value(&bytes)), Ok(1));
    let (armed, at_arm) = mpsc::sync_channel::<()>(0);
    let (go, gone) = mpsc::sync_channel::<()>(0);
    let hold = Arc::new(std::sync::Mutex::new(Some((armed, gone))));
    *subscriber.local().before_sleep.lock().unwrap() = Some(Box::new(move || {
        if let Some((armed, gone)) = hold.lock().unwrap().take() {
            armed.send(()).unwrap();
            gone.recv().unwrap();
        }
    }));
    let receiving = thread::spawn(move || {
        let started = Instant::now();
        let got = subscriber.recv_timeout(Duration::from_secs(5));
        (got.map(|bytes| value(&bytes)), started.elapsed())
    });
    at_arm.recv().unwrap();
    // The first commit looks at `state` now, while the wait has not slept yet.
    resume.send(()).unwrap();
    first.join().unwrap();
    go.send(()).unwrap();
    thread::sleep(Duration::from_millis(50));
    publisher.publish(&num(2)).unwrap();
    let (got, took) = receiving.join().unwrap();
    assert_eq!(got, Ok(2));
    assert!(
        took < Duration::from_secs(1),
        "the second commit did not wake the wait: {took:?}"
    );
    // The first commit sent nothing to the wait that had seen it; the second, one wake.
    let sent = match wake {
        Wake::Futex => publisher.wakes(),
        Wake::Fd => publisher.sends(),
    };
    assert_eq!(sent, 1);
}

/// AC-15.2.33: each subscription named with its caller, what was published, each control page's.
#[test]
fn a_publishers_stats_name_its_subscriptions_and_count_its_records() {
    let publisher = topic(64 << 10);
    let mut reading = subscriber(&publisher);
    let idle = subscriber(&publisher);
    for n in 0..3 {
        publisher.publish(&num(n)).unwrap();
    }
    assert_eq!(reading.try_recv().map(|bytes| value(&bytes)), Ok(0));
    let stats = publisher.stats();
    assert_eq!((stats.published, stats.bytes), (3, 12));
    // Outside a transaction, this process's own: the subscribers are in it.
    let caller = rsbinder::CallingContext::current();
    assert_eq!(stats.subscribers.len(), 2);
    for info in &stats.subscribers {
        assert_eq!((info.uid, info.pid, info.lost), (caller.uid, caller.pid, 0));
        assert!(!info.waiting);
    }
    // A record is 8 bytes of header and its payload padded to 8: 16 here.
    let lags: Vec<u64> = stats
        .subscribers
        .iter()
        .map(|info| info.lag_bytes)
        .collect();
    assert_eq!(lags, [32, 48]);
    drop(idle);
    assert_eq!(publisher.stats().subscribers.len(), 1);
    assert_eq!(
        reading.stats(),
        crate::SubscriberStats {
            received: 1,
            lost: 0,
            decode_errors: 0,
            connections: 1,
        }
    );
}

#[test]
fn a_waiting_subscriber_is_reported_waiting() {
    let publisher = topic(64 << 10);
    let subscriber = subscriber(&publisher);
    let local = subscriber.local().clone();
    let receiving = asleep(subscriber, Subscriber::recv);
    eventually("the subscriber sleeps", || local.is_parked());
    assert!(publisher.stats().subscribers[0].waiting);
    publisher.publish(&num(4)).unwrap();
    assert_eq!(joined(receiving), Ok(4));
    // The thread dropped its subscriber, which the next look takes out.
    assert!(publisher.stats().subscribers.is_empty());
}

/// The payload type of `Bytes` that cannot decode a record of four bytes.
#[derive(Debug, PartialEq)]
struct NotFour(Vec<u8>);

impl crate::Payload for NotFour {
    fn type_name() -> &'static str {
        <Bytes as crate::Payload>::type_name()
    }

    fn encode<'a>(&'a self, out: &mut crate::Encoding<'a>) -> rsbinder::Result<()> {
        out.borrow(&self.0);
        Ok(())
    }

    fn decode(record: &mut crate::Record<'_>) -> rsbinder::Result<Self> {
        match record.bytes().len() {
            4 => Err(rsbinder::StatusCode::BadValue),
            _ => Ok(Self(record.take())),
        }
    }
}

/// AC-15.2.33: a slow subscriber's `lost` is the sum of the `Lagged` it got, on both ends.
#[test]
fn a_slow_subscribers_lost_is_the_sum_of_its_lagged_on_both_ends() {
    let publisher = topic(4096);
    let policy = SubscriberPolicy::default();
    let mut slow = Subscriber::<NotFour>::request(&publisher.as_binder(), &policy).unwrap();
    for n in 0..1000 {
        publisher.publish(&num(n)).unwrap();
    }
    publisher.publish(&Bytes(vec![1; 5])).unwrap();
    let (mut lagged, mut decode_errors) = (0, 0);
    loop {
        match slow.try_recv() {
            Err(RecvError::Lagged(lost)) => lagged += lost,
            Err(RecvError::Decode(_)) => decode_errors += 1,
            Ok(value) => assert_eq!(value, NotFour(vec![1; 5])),
            Err(RecvError::Empty) => break,
            Err(other) => panic!("{other:?}"),
        }
    }
    assert!(lagged > 0, "a ring of 4 KiB kept all of 1000 records");
    let stats = slow.stats();
    assert_eq!((stats.lost, stats.received), (lagged, 1));
    assert_eq!(stats.lost + stats.decode_errors + stats.received, 1001);
    assert_eq!(stats.decode_errors, decode_errors);
    let info = &publisher.stats().subscribers[0];
    assert_eq!((info.lost, info.lag_bytes), (lagged, 0));
}

#[cfg(feature = "tokio")]
mod pool {
    use std::future::Future;
    use std::sync::mpsc;
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::wait::{Local, Parts, Slept, ABANDON, DIED};
    use crate::Interrupter;

    /// A runtime whose drop waits at most 1 s for the pool: a wait stuck there is left behind.
    struct Pool(Option<tokio::runtime::Runtime>);

    impl std::ops::Deref for Pool {
        type Target = tokio::runtime::Runtime;

        fn deref(&self) -> &Self::Target {
            self.0.as_ref().unwrap()
        }
    }

    impl Drop for Pool {
        fn drop(&mut self) {
            self.0
                .take()
                .unwrap()
                .shutdown_timeout(Duration::from_secs(1));
        }
    }

    fn runtime() -> Pool {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        Pool(Some(runtime))
    }

    /// Start a `recv_async` and drop it mid-wait, leaving its wait on the pool.
    fn abandon(runtime: &tokio::runtime::Runtime, subscriber: &mut Subscriber<Bytes>) {
        let _enter = runtime.enter();
        let mut future = std::pin::pin!(subscriber.recv_async());
        let polled = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(polled, Poll::Pending), "nothing to receive yet");
    }

    /// `abandon`, dropping the future only once a pool thread sleeps in its wait.
    fn abandon_asleep(runtime: &tokio::runtime::Runtime, subscriber: &mut Subscriber<Bytes>) {
        let local = subscriber.local().clone();
        let _enter = runtime.enter();
        let mut future = std::pin::pin!(subscriber.recv_async());
        // Polled until then: it may first join the wait of a receive that found a record.
        eventually("the wait sleeps on the pool", || {
            let polled = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()));
            assert!(matches!(polled, Poll::Pending), "nothing to receive yet");
            local.is_parked()
        });
    }

    /// Start a `recv_async` and leak it mid-wait: with no drop, its wait stays handed over.
    fn leak(runtime: &tokio::runtime::Runtime, subscriber: &mut Subscriber<Bytes>) {
        let _enter = runtime.enter();
        let mut future = Box::pin(subscriber.recv_async());
        let polled = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(polled, Poll::Pending), "nothing to receive yet");
        std::mem::forget(future);
    }

    #[test]
    fn an_async_receive_waits_on_the_pool() {
        let runtime = runtime();
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        let local = subscriber.local().clone();
        let receiving = runtime.spawn(async move { subscriber.recv_async().await });
        eventually("the wait sleeps", || local.is_parked());
        publisher.publish(&num(9)).unwrap();
        eventually("the receive returns", || receiving.is_finished());
        let got = runtime.block_on(receiving).unwrap();
        assert_eq!(got.map(|bytes| value(&bytes)), Ok(9));
    }

    /// AC-15.2.41: a receive right after a dropped `recv_async` joins the wait it left first.
    #[test]
    fn a_receive_after_a_dropped_async_receive_gets_every_record() {
        let runtime = runtime();
        let publisher = Arc::new(topic(64 << 10));
        let mut subscriber = subscriber(&publisher);
        // Each value is published after its delay; `published` says when, from `base`.
        let (send, values) = mpsc::channel::<(u32, Duration)>();
        let base = Instant::now();
        let published = Arc::new(std::sync::Mutex::new((0u32, Duration::ZERO)));
        let publishing = {
            let publisher = publisher.clone();
            let published = published.clone();
            thread::spawn(move || {
                for (n, delay) in values {
                    thread::sleep(delay);
                    publisher.publish(&num(n)).unwrap();
                    *published.lock().unwrap() = (n, base.elapsed());
                }
            })
        };
        let rounds = |subscriber: &mut Subscriber<Bytes>, from: u32, to: u32, delay| {
            for n in from..to {
                // Half the drops race the pool thread for the wait, half wake it from its sleep.
                match n % 2 {
                    0 => abandon(&runtime, subscriber),
                    _ => abandon_asleep(&runtime, subscriber),
                }
                let wakes = publisher.wakes();
                send.send((n, delay)).unwrap();
                let started = Instant::now();
                let got = subscriber.recv_timeout(Duration::from_secs(5));
                // A wait that slept through the commit still finds it once its deadline passes.
                let took = started.elapsed();
                if took >= Duration::from_secs(1) {
                    let words = subscriber.local().words();
                    panic!(
                        "round {n} missed the wake ({took:?}): {got:?}, started at {:?}, \
                         published {:?}, {} futex wakes, joined in {:?}, sleeps {:?}, \
                         state {:#x}, seq {}, read {} of {}",
                        started - base,
                        *published.lock().unwrap(),
                        publisher.wakes() - wakes,
                        subscriber.trace.join,
                        subscriber.trace.parks,
                        words.control.state().load(Ordering::SeqCst),
                        crate::wait::Words::seq(words),
                        subscriber.position(),
                        subscriber.committed(),
                    );
                }
                assert_eq!(got.map(|bytes| value(&bytes)), Ok(n), "round {n}");
            }
        };
        rounds(&mut subscriber, 0, 10_000, Duration::ZERO);

        // The left wait lowers `WAITING` after a new one would sleep, the commit later still.
        let main = thread::current().id();
        *subscriber.local().after_sleep.lock().unwrap() = Some(Box::new(move || {
            if thread::current().id() != main {
                thread::sleep(Duration::from_micros(200));
            }
        }));
        rounds(&mut subscriber, 10_000, 11_000, Duration::from_millis(1));
        drop(send);
        publishing.join().unwrap();
        eventually("no wait is left on the pool", || {
            !subscriber.leftover().busy()
        });
    }

    /// A wait queued behind the pool's one busy thread is taken back at its drop and never runs.
    #[test]
    fn a_wait_the_pool_has_not_started_is_taken_back_when_its_future_drops() {
        let (runtime, release) = held_pool();
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        abandon(&runtime, &mut subscriber);
        assert!(!subscriber.leftover().busy(), "taken back at the drop");
        let stop = subscriber.local().stop_bits();
        assert_eq!(stop & ABANDON, 0, "no wait to end");
        publisher.publish(&num(7)).unwrap();
        let got = subscriber.recv_timeout(Duration::from_millis(100));
        assert_eq!(got.map(|bytes| value(&bytes)), Ok(7));
        release.send(()).unwrap();
        // Queued after the closure of the wait taken back, on the one thread: it runs after it.
        let after = runtime.spawn_blocking(|| ());
        runtime.block_on(after).unwrap();
        let arms = subscriber.local().arms.load(Ordering::Relaxed);
        assert_eq!(arms, 0, "the closure of the wait taken back made a wait");
    }

    /// A busy pool, a dropped `recv_async`: a bounded receive sleeps on its own to its deadline.
    #[test]
    fn a_bounded_receive_after_a_wait_taken_back_sleeps_on_its_own_to_its_deadline() {
        let (runtime, release) = held_pool();
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        abandon(&runtime, &mut subscriber);
        let started = Instant::now();
        let got = subscriber.recv_timeout(Duration::from_millis(100));
        let took = started.elapsed();
        release.send(()).unwrap();
        assert_eq!(got.map(|bytes| value(&bytes)), Err(RecvError::Timeout));
        assert!(took < Duration::from_secs(1), "{took:?}");
        let parks = &subscriber.trace.parks;
        let slept = parks.iter().any(|(slept, ..)| *slept == Slept::TimedOut);
        assert!(slept, "joined in {:?}", subscriber.trace.join);
    }

    /// A busy pool, a dropped `recv_async`, then a record: no receive waits for the pool.
    #[test]
    fn a_record_committed_after_a_receive_behind_a_wait_taken_back_began_ends_it() {
        let (runtime, release) = held_pool();
        let publisher = Arc::new(topic(64 << 10));
        let mut subscriber = subscriber(&publisher);
        abandon(&runtime, &mut subscriber);
        let publishing = {
            let publisher = publisher.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                publisher.publish(&num(7)).unwrap();
            })
        };
        let started = Instant::now();
        let got = subscriber.recv_timeout(Duration::from_secs(5));
        let took = started.elapsed();
        release.send(()).unwrap();
        publishing.join().unwrap();
        assert_eq!(got.map(|bytes| value(&bytes)), Ok(7));
        assert!(took < Duration::from_secs(1), "the receive waited {took:?}");
    }

    /// An interrupt before a receive behind a dropped `recv_async` on a busy pool ends it at once.
    #[test]
    fn an_interrupt_before_a_receive_after_a_wait_taken_back_ends_it_at_once() {
        let (runtime, release) = held_pool();
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        abandon(&runtime, &mut subscriber);
        subscriber.interrupter().interrupt();
        let started = Instant::now();
        let got = subscriber.recv_timeout(Duration::from_secs(1));
        let took = started.elapsed();
        release.send(()).unwrap();
        assert_eq!(got.map(|bytes| value(&bytes)), Err(RecvError::Cancelled));
        assert!(took < Duration::from_millis(500), "{took:?}");
    }

    /// A wait dropped once a pool thread sleeps in it is woken, and that thread leaves it.
    #[test]
    fn a_wait_the_pool_runs_is_woken_when_its_future_drops() {
        let runtime = runtime();
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        abandon_asleep(&runtime, &mut subscriber);
        let local = subscriber.local();
        assert!(!local.is_parked(), "the drop returns once the wait is out");
        let started = Instant::now();
        eventually("the left wait ends", || !subscriber.leftover().busy());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(local.stop_bits() & ABANDON, 0, "the wait takes its own");
    }

    /// The closure of a wait taken back runs on an empty hand: the next hand-off is its own.
    #[test]
    fn a_hand_off_after_a_wait_taken_back_is_not_settled_by_its_closure() {
        let (runtime, release) = held_pool();
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        abandon(&runtime, &mut subscriber);
        let local = subscriber.local().clone();
        let left = subscriber.leftover().clone();
        // Its closure queues behind the empty one.
        let receiving = runtime.spawn(async move { subscriber.recv_async().await });
        eventually("the second wait is handed over", || left.busy());
        release.send(()).unwrap();
        eventually("the second wait sleeps on the pool", || local.is_parked());
        assert!(left.busy(), "the empty closure settled the second wait");
        publisher.publish(&num(7)).unwrap();
        eventually("the receive returns", || receiving.is_finished());
        let got = runtime.block_on(receiving).unwrap();
        assert_eq!(got.map(|bytes| value(&bytes)), Ok(7));
        assert!(!left.busy());
    }

    /// A hand-off that unwinds, as `spawn_blocking` outside a runtime does, leaves no wait out.
    #[test]
    fn a_hand_off_that_unwinds_leaves_no_wait_out() {
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut future = std::pin::pin!(subscriber.recv_async());
            let _ = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()));
        }));
        assert!(unwound.is_err(), "no runtime to hand the wait to");
        assert!(!subscriber.leftover().busy());
    }

    /// A runtime whose one blocking thread is busy until the sender sends, for 10 s at most.
    fn held_pool() -> (Pool, mpsc::Sender<()>) {
        let runtime = Pool(Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .max_blocking_threads(1)
                .build()
                .unwrap(),
        ));
        let (release, held) = mpsc::channel::<()>();
        let _holding = runtime.spawn_blocking(move || held.recv_timeout(Duration::from_secs(10)));
        (runtime, release)
    }

    /// Until `holds` does, for 1 s at most; whether it did.
    fn within_a_second(mut holds: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(1);
        while !holds() {
            if Instant::now() >= end {
                return false;
            }
            thread::sleep(Duration::from_micros(50));
        }
        true
    }

    /// A busy pool, a dropped `recv_async`, then a `recv` that sleeps: `raise` ends it in 1 s.
    fn a_receive_after_a_wait_taken_back_ends_at(
        raise: impl FnOnce(&Interrupter, &Local<Parts>),
        ends: RecvError,
    ) -> Subscriber<Bytes> {
        let (runtime, release) = held_pool();
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        abandon(&runtime, &mut subscriber);
        let (local, interrupter) = (subscriber.local().clone(), subscriber.interrupter());
        let (send, got) = mpsc::channel();
        let receiving = thread::spawn(move || {
            send.send(subscriber.recv().map(|bytes| value(&bytes)))
                .unwrap();
            subscriber
        });
        let parked = within_a_second(|| local.is_parked());
        raise(&interrupter, &local);
        let ended = got.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        assert_eq!(ended, Ok(Err(ends)));
        assert!(parked, "the receive slept on its own");
        receiving.join().unwrap()
    }

    #[test]
    fn an_interrupt_ends_a_receive_after_a_wait_taken_back_from_a_busy_pool() {
        let interrupt = |interrupter: &Interrupter, _: &_| interrupter.interrupt();
        drop(a_receive_after_a_wait_taken_back_ends_at(
            interrupt,
            RecvError::Cancelled,
        ));
    }

    #[test]
    fn a_notice_ends_a_receive_after_a_wait_taken_back_from_a_busy_pool() {
        let notice = |interrupter: &Interrupter, _: &_| interrupter.notice();
        let subscriber = a_receive_after_a_wait_taken_back_ends_at(notice, RecvError::Cancelled);
        assert!(subscriber.take_notice(), "the receive leaves the bit up");
    }

    #[test]
    fn a_death_ends_a_receive_after_a_wait_taken_back_from_a_busy_pool() {
        let died = |_: &_, local: &Local<_>| {
            local.raise(DIED);
        };
        let ends = RecvError::Closed(Closed::PublisherDied);
        drop(a_receive_after_a_wait_taken_back_ends_at(died, ends));
    }

    /// Records behind a leaked wait the pool has not run: receives with no bound get them at once.
    #[test]
    fn an_unbounded_receive_behind_a_leaked_wait_the_pool_has_not_run_returns_the_record() {
        let (runtime, release) = held_pool();
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        leak(&runtime, &mut subscriber);
        publisher.publish(&num(7)).unwrap();
        publisher.publish(&num(8)).unwrap();
        {
            let _enter = runtime.enter();
            let mut future = std::pin::pin!(subscriber.recv_async());
            let polled = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()));
            let polled = polled.map(|got| got.map(|bytes| value(&bytes)));
            assert_eq!(polled, Poll::Ready(Ok(7)), "async");
        }
        assert!(subscriber.leftover().busy(), "the left wait is queued");
        let (send, got) = mpsc::channel();
        let receiving = thread::spawn(move || {
            send.send(subscriber.recv().map(|bytes| value(&bytes)))
                .unwrap();
            subscriber
        });
        let first = got.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        assert_eq!(first, Ok(Ok(8)), "sync");
        let subscriber = receiving.join().unwrap();
        eventually("the left wait ends", || !subscriber.leftover().busy());
    }

    /// A record committed while a bounded receive joins a leaked wait the pool has not run.
    #[test]
    fn a_bounded_receive_gets_a_record_committed_while_it_joins_a_leaked_wait() {
        let (runtime, release) = held_pool();
        let publisher = Arc::new(topic(64 << 10));
        let mut subscriber = subscriber(&publisher);
        leak(&runtime, &mut subscriber);
        {
            let publisher = publisher.clone();
            // Committed past the look before the join, as the join begins.
            *subscriber.leftover().before_join.lock().unwrap() = Some(Box::new(move || {
                publisher.publish(&num(7)).unwrap();
            }));
        }
        let got = subscriber.recv_timeout(Duration::from_secs(1));
        // Set only past the join: a look before it that found the record leaves it zero.
        let joined_for = subscriber.trace.join;
        release.send(()).unwrap();
        assert_eq!(got.map(|bytes| value(&bytes)), Ok(7));
        assert!(joined_for >= Duration::from_millis(500), "{joined_for:?}");
        eventually("the left wait ends", || !subscriber.leftover().busy());
    }

    /// AC-15.2.11 within a process: dropped async receives leave the pool within a second.
    #[test]
    fn eight_dropped_async_receives_leave_the_pool_within_a_second() {
        let runtime = runtime();
        let publisher = topic(64 << 10);
        for _ in 0..1000 {
            let mut subscribers: Vec<_> = (0..8).map(|_| subscriber(&publisher)).collect();
            let left: Vec<_> = subscribers
                .iter_mut()
                .map(|subscriber| {
                    abandon(&runtime, subscriber);
                    subscriber.leftover().clone()
                })
                .collect();
            let started = Instant::now();
            for left in &left {
                eventually("the left wait ends", || !left.busy());
            }
            assert!(started.elapsed() < Duration::from_secs(1));
        }
    }

    /// A subscriber dropped while its left wait sleeps wakes that wait, with no commit to help.
    #[test]
    fn a_dropped_subscriber_ends_the_wait_it_left_on_the_pool() {
        let runtime = runtime();
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        let local = subscriber.local().clone();
        let left = subscriber.leftover().clone();
        let _enter = runtime.enter();
        // Leaked, so that its drop does not wake the wait: only the subscriber's drop can.
        let mut future = Box::pin(subscriber.recv_async());
        let polled = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(polled, Poll::Pending));
        std::mem::forget(future);
        eventually("the left wait sleeps", || local.is_parked());
        let started = Instant::now();
        drop(subscriber);
        eventually("the left wait ends", || !left.busy());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(publisher.subscriber_count(), 0);
    }

    /// AC-15.2.38: a wait that lowers `WAITING` after its subscriber went leaves `CLOSED` up.
    #[test]
    fn a_wait_that_outlives_its_subscriber_does_not_reopen_it() {
        let runtime = runtime();
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        let local = subscriber.local().clone();
        let left = subscriber.leftover().clone();
        let (paused, pause) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
        );
        {
            let (paused, pause) = (paused.clone(), pause.clone());
            *local.after_sleep.lock().unwrap() = Some(Box::new(move || {
                paused.store(true, Ordering::SeqCst);
                while pause.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_micros(100));
                }
            }));
        }
        // Leaked, so that its drop does not wake the wait: the wait stays out on the pool.
        let _enter = runtime.enter();
        let mut future = Box::pin(subscriber.recv_async());
        let polled = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(polled, Poll::Pending));
        std::mem::forget(future);
        eventually("the left wait sleeps", || local.is_parked());

        // Woken by a commit, it pauses with `WAITING` still up; the subscriber goes meanwhile.
        publisher.publish(&num(1)).unwrap();
        eventually("the left wait woke", || paused.load(Ordering::SeqCst));
        drop(subscriber);
        pause.store(false, Ordering::SeqCst);
        eventually("the left wait ends", || !left.busy());
        assert_eq!(publisher.subscriber_count(), 0);
    }
}
