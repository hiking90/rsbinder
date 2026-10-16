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
    assert_eq!(receiving.join().unwrap(), Ok(7));
    assert_eq!(publisher.wakes(), 1);
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
        assert_eq!(receiving.join().unwrap(), Ok(1));
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
    assert_eq!(receiving.join().unwrap(), Ok(0));
}

/// AC-15.2.37: `publish_all` wakes once.
#[test]
fn a_batch_is_announced_with_one_wake() {
    let publisher = topic(64 << 10);
    let receiving = asleep(subscriber(&publisher), Subscriber::recv);
    assert_eq!(publisher.publish_all(&[num(1), num(2), num(3)]), Ok(3));
    assert_eq!(publisher.wakes(), 1);
    assert_eq!(receiving.join().unwrap(), Ok(1));
}

/// AC-15.2.37: the end reaches a sleeping subscriber with the status it was given.
#[test]
fn a_close_wakes_a_sleeping_subscriber_with_its_status() {
    let publisher = topic(64 << 10);
    let receiving = asleep(subscriber(&publisher), Subscriber::recv);
    let status = Status::new_service_specific_error(5, Some("over".into()));
    publisher.close(&status);
    assert_eq!(
        receiving.join().unwrap(),
        Err(RecvError::Closed(Closed::Status(status)))
    );

    // A dropped publisher ends a wait the same way, with no error.
    let publisher = topic(64 << 10);
    let receiving = asleep(subscriber(&publisher), Subscriber::recv);
    drop(publisher);
    assert_eq!(
        receiving.join().unwrap(),
        Err(RecvError::Closed(Closed::End))
    );
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
    assert_eq!(receiving.join().unwrap(), Ok(3));
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
    assert_eq!(receiving.join().unwrap(), 10_000);
    assert!(most >= 1, "every interrupt found the wait asleep");
    assert!(
        most <= WAKES_PER_INTERRUPT,
        "{most} wakes for one interrupt"
    );
}

/// The wakes one `interrupt` may send: 2 to 4 alone, 66 seen beside a build; a raise that wakes
/// at every look without yielding sends thousands.
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

/// A commit whose look at `state` comes after a newer wait raised `WAITING` wakes that wait
/// before it sleeps, which is no wake for it: it sleeps on the `seq` the commit made. The next
/// commit must wake it all the same.
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

#[cfg(feature = "tokio")]
mod pool {
    use std::future::Future;
    use std::sync::mpsc;
    use std::task::{Context, Poll, Waker};

    use super::*;

    /// A runtime whose drop waits 1 s at most for the pool: a stuck wait fails, not hangs, a test.
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

    #[test]
    fn an_async_receive_waits_on_the_pool() {
        let runtime = runtime();
        let publisher = topic(64 << 10);
        let mut subscriber = subscriber(&publisher);
        let local = subscriber.local().clone();
        let receiving = runtime.spawn(async move { subscriber.recv_async().await });
        eventually("the wait sleeps", || local.is_parked());
        publisher.publish(&num(9)).unwrap();
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
                abandon(&runtime, subscriber);
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
