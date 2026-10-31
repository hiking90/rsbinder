// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Receives of a subscription woken through its socket (plan 15 §5.5a), within one process: the
//! publisher's bytes, `wait_any` and a poll loop of the caller's, this process's wakes, and what
//! a subscriber that never reads its socket can do to the publisher. A publisher that dies is in
//! `tests/two_process.rs`; the protocol under every interleaving is in `wait/loom_tests.rs`.
//!
//! "Bytes" here are the publisher's sends on the sockets, which it counts.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags, Timespec};

use crate::wait::Words;
use crate::{
    wait_any, Bytes, Closed, Payload, Publisher, PublisherPolicy, RecvError, Start, Subscriber,
    SubscriberPolicy, Wait, Wake,
};

fn num(n: u32) -> Bytes {
    Bytes(n.to_le_bytes().to_vec())
}

fn value(bytes: &Bytes) -> u32 {
    u32::from_le_bytes(bytes.0[..4].try_into().unwrap())
}

/// A payload of another type, so that one thread waits for topics of two.
#[derive(Debug, PartialEq)]
struct Word(u64);

impl Payload for Word {
    fn type_name() -> &'static str {
        "rsbinder.topic.test.Word"
    }

    fn encode(&self, out: &mut crate::Encoding<'_>) -> rsbinder::Result<()> {
        out.extend(&self.0.to_le_bytes());
        Ok(())
    }

    fn decode(record: &mut crate::Record<'_>) -> rsbinder::Result<Self> {
        let bytes = record
            .bytes()
            .try_into()
            .map_err(|_| rsbinder::StatusCode::BadValue)?;
        Ok(Self(u64::from_le_bytes(bytes)))
    }
}

fn topic(ring_bytes: usize) -> Publisher<Bytes> {
    topic_of(ring_bytes)
}

fn topic_of<T: Payload + 'static>(ring_bytes: usize) -> Publisher<T> {
    Publisher::new(&PublisherPolicy {
        ring_bytes,
        ..PublisherPolicy::default()
    })
    .unwrap()
}

fn woken_by<T: Payload + 'static>(publisher: &Publisher<T>, wake: Wake) -> Subscriber<T> {
    let policy = SubscriberPolicy {
        start: Start::Latest,
        wake,
        ..SubscriberPolicy::default()
    };
    Subscriber::request(&publisher.as_binder(), &policy).unwrap()
}

fn fd_subscriber<T: Payload + 'static>(publisher: &Publisher<T>) -> Subscriber<T> {
    woken_by(publisher, Wake::Fd)
}

/// The bound of every wait here: a lost wake fails a test instead of hanging it.
const WAIT: Duration = Duration::from_secs(10);

/// `recv` within [`WAIT`]; a wait that sleeps through its wake fails, even if its last look finds.
fn recv(subscriber: &mut Subscriber<Bytes>) -> Result<Bytes, RecvError> {
    let started = Instant::now();
    let got = subscriber.recv_timeout(WAIT);
    assert!(started.elapsed() < WAIT, "the wait slept through its wake");
    got
}

/// Poll `holds` until it does, failing after 10 s.
fn eventually(what: &str, mut holds: impl FnMut() -> bool) {
    let end = Instant::now() + WAIT;
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

/// `poll` the fd of `subscriber` for up to `timeout`; whether it was readable, as 1 or 0.
fn poll_for(subscriber: &Subscriber<Bytes>, timeout: Duration) -> usize {
    let fd = subscriber.wait_fd().unwrap();
    let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
    poll(&mut fds, Some(&Timespec::try_from(timeout).unwrap())).unwrap()
}

#[test]
fn a_receive_sleeps_until_a_commit_and_one_byte_ends_it() {
    let publisher = topic(64 << 10);
    let receiving = asleep(fd_subscriber(&publisher), recv);
    publisher.publish(&num(7)).unwrap();
    assert_eq!(receiving.join().unwrap(), Ok(7));
    assert_eq!((publisher.sends(), publisher.wakes()), (1, 0));
}

/// AC-15.2.45: one publish wakes both kinds: one futex wake for all, a byte for each fd waiter.
#[test]
fn futex_and_fd_waiters_of_one_topic_wake_with_one_futex_wake_and_a_byte_each() {
    let publisher = topic(64 << 10);
    let futex: Vec<_> = (0..2)
        .map(|_| asleep(woken_by(&publisher, Wake::Futex), recv))
        .collect();
    let fd: Vec<_> = (0..3)
        .map(|_| asleep(fd_subscriber(&publisher), recv))
        .collect();
    publisher.publish(&num(1)).unwrap();
    assert_eq!((publisher.wakes(), publisher.sends()), (1, 3));
    for receiving in futex.into_iter().chain(fd) {
        assert_eq!(receiving.join().unwrap(), Ok(1));
    }
}

/// AC-15.2.19 for the fd: one byte for a wait, however many commits land before it ends.
#[test]
fn a_hundred_publishes_before_the_woken_wait_ends_send_one_byte() {
    let publisher = topic(64 << 10);
    let subscriber = fd_subscriber(&publisher);
    let release = Arc::new(AtomicBool::new(false));
    let held = release.clone();
    *subscriber.local().after_sleep.lock().unwrap() = Some(Box::new(move || {
        while !held.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_micros(100));
        }
    }));
    let receiving = asleep(subscriber, recv);
    for n in 0..100 {
        publisher.publish(&num(n)).unwrap();
    }
    assert_eq!(publisher.sends(), 1);
    release.store(true, Ordering::SeqCst);
    assert_eq!(receiving.join().unwrap(), Ok(0));
}

/// `state` keeps 30 bits of `seq`: a commit that finds no wait forgets the one it woke.
#[test]
fn a_wait_in_the_state_of_one_woken_long_ago_is_woken() {
    for wake in [Wake::Futex, Wake::Fd] {
        let publisher = topic(64 << 10);
        let mut subscriber = woken_by(&publisher, wake);
        let next = subscriber.local().words().seq().wrapping_add(1);
        publisher.take_as_woken(crate::layout::seen_field(next) | crate::layout::WAITING);
        publisher.publish(&num(1)).unwrap();
        assert_eq!(recv(&mut subscriber).map(|bytes| value(&bytes)), Ok(1));
        let receiving = asleep(subscriber, recv);
        publisher.publish(&num(2)).unwrap();
        assert_eq!(receiving.join().unwrap(), Ok(2), "{wake:?}");
    }
}

/// A publish in the hook that runs once, between the waiter's last look and its `WAITING`.
fn commit_before_the_wait(publisher: &Arc<Publisher<Bytes>>, subscriber: &mut Subscriber<Bytes>) {
    let publisher = publisher.clone();
    let mut once = Some(());
    subscriber.about_to_park = Some(Box::new(move || {
        if once.take().is_some() {
            publisher.publish(&num(8)).unwrap();
        }
    }));
}

/// The publisher saw no wait, so sent nothing: the look at `seq` after `WAITING` finds the commit.
#[test]
fn a_commit_just_before_a_receive_waits_is_found_by_its_second_look() {
    let publisher = Arc::new(topic(64 << 10));
    let mut subscriber = fd_subscriber(&publisher);
    commit_before_the_wait(&publisher, &mut subscriber);
    let started = Instant::now();
    // The look after the timeout would find it too: only the time tells the two apart.
    let got = subscriber.recv_timeout(Duration::from_secs(10));
    assert_eq!(got.map(|bytes| value(&bytes)), Ok(8));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(publisher.sends(), 0);
}

#[test]
fn a_commit_just_before_prepare_wait_arms_makes_it_return_false() {
    let publisher = Arc::new(topic(64 << 10));
    let mut subscriber = fd_subscriber(&publisher);
    commit_before_the_wait(&publisher, &mut subscriber);
    assert!(!subscriber.prepare_wait());
    assert_eq!(subscriber.try_recv().map(|bytes| value(&bytes)), Ok(8));
}

/// A loop that takes one record per turn: each `prepare_wait` judges by the look made since.
#[test]
fn a_poll_loop_that_takes_one_record_a_turn_polls_only_once_none_is_unread() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    publisher.publish(&num(1)).unwrap();
    publisher.publish(&num(2)).unwrap();
    let short = Some(Duration::from_millis(100));
    for n in [1, 2] {
        assert!(wait_any(&mut [&mut subscriber], short).unwrap(), "{n}");
        assert_eq!(subscriber.try_recv().map(|bytes| value(&bytes)), Ok(n));
    }
    assert!(!wait_any(&mut [&mut subscriber], short).unwrap());
}

/// AC-15.2.45: a loop that polls the fds itself, with no `wait_any` and no runtime, gets them all.
#[test]
fn a_poll_loop_of_the_callers_receives_every_record() {
    const RECORDS: u32 = 20_000;
    let publisher = Arc::new(topic(1 << 20));
    let mut subscriber = fd_subscriber(&publisher);
    let publishing = {
        let publisher = publisher.clone();
        thread::spawn(move || {
            for n in 0..RECORDS {
                publisher.publish(&num(n)).unwrap();
                if n % 64 == 0 {
                    thread::sleep(Duration::from_micros(50));
                }
            }
            publisher.close();
        })
    };
    let mut next = 0;
    loop {
        match subscriber.try_recv() {
            Ok(bytes) => {
                assert_eq!(value(&bytes), next);
                next += 1;
                continue;
            }
            Err(RecvError::Empty) => {}
            Err(RecvError::Closed(Closed::End)) => break,
            Err(other) => panic!("{other}"),
        }
        if subscriber.prepare_wait() {
            assert_ne!(
                poll_for(&subscriber, Duration::from_secs(10)),
                0,
                "a wake was lost"
            );
            subscriber.finish_wait();
        }
    }
    assert_eq!(next, RECORDS);
    publishing.join().unwrap();
}

/// AC-15.2.17: one thread, three topics of two types, `wait_any`, every record.
#[test]
fn one_thread_waits_for_three_topics_with_wait_any_and_receives_everything() {
    const RECORDS: u32 = 5_000;
    let first = Arc::new(topic(1 << 20));
    let second = Arc::new(topic_of::<Word>(1 << 20));
    let third = Arc::new(topic(1 << 20));
    let (mut a, mut b, mut c) = (
        fd_subscriber(&first),
        fd_subscriber(&second),
        fd_subscriber(&third),
    );
    let pace = |n: u32| {
        if n % 32 == 0 {
            thread::sleep(Duration::from_micros(100));
        }
    };
    let publishing = [
        thread::spawn({
            let first = first.clone();
            move || (0..RECORDS).for_each(|n| (first.publish(&num(n)).unwrap(), pace(n)).1)
        }),
        thread::spawn({
            let second = second.clone();
            move || (0..RECORDS).for_each(|n| (second.publish(&Word(n.into())).unwrap(), pace(n)).1)
        }),
        thread::spawn({
            let third = third.clone();
            move || (0..RECORDS).for_each(|n| (third.publish(&num(n)).unwrap(), pace(n)).1)
        }),
    ];
    let mut next = [0u32; 3];
    while next != [RECORDS; 3] {
        let waits: &mut [&mut dyn Wait] = &mut [&mut a, &mut b, &mut c];
        let woke = wait_any(waits, Some(Duration::from_secs(10)));
        assert!(woke.unwrap(), "a wake was lost at {next:?}");
        while let Ok(bytes) = a.try_recv() {
            assert_eq!(value(&bytes), next[0]);
            next[0] += 1;
        }
        while let Ok(Word(word)) = b.try_recv() {
            assert_eq!(word, u64::from(next[1]));
            next[1] += 1;
        }
        while let Ok(bytes) = c.try_recv() {
            assert_eq!(value(&bytes), next[2]);
            next[2] += 1;
        }
    }
    for publishing in publishing {
        publishing.join().unwrap();
    }
}

#[test]
fn wait_any_returns_at_once_with_a_record_unread_and_false_after_its_timeout() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    let started = Instant::now();
    let woke = wait_any(&mut [&mut subscriber], Some(Duration::from_millis(50)));
    assert!(!woke.unwrap());
    assert!(started.elapsed() >= Duration::from_millis(50));
    publisher.publish(&num(3)).unwrap();
    // A bound: a poll that sleeps on the unread record times out and fails the test.
    let woke = wait_any(&mut [&mut subscriber], Some(WAIT));
    assert!(woke.unwrap());
    assert_eq!(subscriber.try_recv().map(|bytes| value(&bytes)), Ok(3));
    // Neither wait was armed when a commit landed, and none is left armed.
    publisher.publish(&num(4)).unwrap();
    assert_eq!(publisher.sends(), 0);
}

#[test]
#[should_panic(expected = "no file descriptor to poll")]
fn wait_any_refuses_a_subscription_woken_by_the_futex() {
    let publisher = topic(64 << 10);
    let mut subscriber = woken_by(&publisher, Wake::Futex);
    assert!(subscriber.wait_fd().is_none());
    let _ = wait_any(&mut [&mut subscriber], Some(Duration::ZERO));
}

#[test]
#[should_panic(expected = "finish_wait: a subscription made with `Wake::Futex`")]
fn a_subscription_woken_by_the_futex_has_no_wait_to_finish() {
    let publisher = topic(64 << 10);
    let mut subscriber = woken_by(&publisher, Wake::Futex);
    subscriber.finish_wait();
}

#[test]
#[should_panic(expected = "no subscription to wait for, and no bound")]
fn wait_any_refuses_to_wait_for_nothing_without_a_bound() {
    let _ = wait_any(&mut [], None);
}

#[test]
#[should_panic(expected = "no subscription to wait for, and no bound")]
fn wait_any_refuses_to_wait_for_nothing_for_longer_than_an_instant_holds() {
    let _ = wait_any(&mut [], Some(Duration::MAX));
}

/// Nothing to wait for, with a bound: a sleep to it.
#[test]
fn wait_any_of_nothing_with_a_bound_sleeps_to_it() {
    let started = Instant::now();
    assert!(!wait_any(&mut [], Some(Duration::from_millis(20))).unwrap());
    assert!(started.elapsed() >= Duration::from_millis(20));
}

/// A flag that a wait of `subscriber` raises once its arm found no bit up, before it sleeps.
fn armed_to_sleep(subscriber: &Subscriber<Bytes>) -> Arc<AtomicBool> {
    let armed = Arc::new(AtomicBool::new(false));
    let raised = armed.clone();
    *subscriber.local().before_sleep.lock().unwrap() =
        Some(Box::new(move || raised.store(true, Ordering::SeqCst)));
    armed
}

/// An interrupt is one byte sent, with no loop of wakes: it stays until drained.
#[test]
fn an_interrupt_ends_an_fd_wait_with_one_wake() {
    let publisher = topic(64 << 10);
    let subscriber = fd_subscriber(&publisher);
    let interrupter = subscriber.interrupter();
    let armed = armed_to_sleep(&subscriber);
    let receiving = asleep(subscriber, |subscriber| {
        subscriber.recv_timeout(Duration::from_secs(10))
    });
    // Past an arm that saw no bit: `parked` stays up until a wake, so the interrupt sends one.
    eventually("the wait armed to sleep", || armed.load(Ordering::SeqCst));
    let interrupted = Instant::now();
    assert_eq!(interrupter.interrupt_counting(), 1);
    assert_eq!(receiving.join().unwrap(), Err(RecvError::Cancelled));
    // A wait that slept through its timeout would see the interrupt too, 10 s late.
    assert!(interrupted.elapsed() < Duration::from_secs(5));
}

/// Its byte ends the poll, the next `prepare_wait` takes its kick, and the poll after sleeps.
#[test]
fn an_interrupt_wakes_a_poll_loop_and_stays_for_the_next_receive() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    assert!(subscriber.prepare_wait());
    subscriber.interrupter().interrupt();
    assert_eq!(poll_for(&subscriber, Duration::from_secs(10)), 1);
    subscriber.finish_wait();
    assert!(!subscriber.prepare_wait(), "the interrupt's kick");
    // Taken: the next poll sleeps through its timeout.
    assert!(subscriber.prepare_wait());
    assert_eq!(poll_for(&subscriber, Duration::from_millis(20)), 0);
    subscriber.finish_wait();
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
}

/// `wait_any` of one subscriber, bounded by `timeout`.
fn waited(subscriber: &mut Subscriber<Bytes>, timeout: Duration) -> bool {
    wait_any(&mut [subscriber], Some(timeout)).unwrap()
}

/// Raised after the poll returned, its byte drained by `finish_wait`: the next call ends at once.
#[test]
fn an_interrupt_whose_byte_a_finish_wait_drains_ends_the_next_poll() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    assert!(subscriber.prepare_wait());
    // As at the poll's timeout, or a return for another fd of the loop's.
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

/// A poll asleep at the interrupt returns, then at most one call at once, then one sleeps.
#[test]
fn an_interrupt_ends_a_sleeping_poll_and_at_most_one_call_after_it() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    let (local, interrupter) = (subscriber.local().clone(), subscriber.interrupter());
    let polling = thread::spawn(move || {
        let woken = waited(&mut subscriber, WAIT);
        let mut early = 0;
        while waited(&mut subscriber, Duration::from_millis(20)) {
            early += 1;
            assert!(early <= 1, "a poll loop that spins at one interrupt");
        }
        (woken, subscriber)
    });
    eventually("the poll sleeps", || local.is_parked());
    interrupter.interrupt();
    let (woken, mut subscriber) = polling.join().unwrap();
    assert!(woken);
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
}

/// With no wait `parked` an interrupt sends no byte; the next `prepare_wait` sees it as it arms.
#[test]
fn an_interrupt_with_no_wait_sends_no_byte() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    assert_eq!(subscriber.interrupter().interrupt_counting(), 0);
    assert_eq!(poll_for(&subscriber, Duration::ZERO), 0);
    assert!(!subscriber.prepare_wait(), "the interrupt's kick");
    assert!(subscriber.prepare_wait());
    subscriber.finish_wait();
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
}

/// A send that queues no byte while a poll sleeps is sent again until one is.
#[test]
fn an_interrupt_whose_first_byte_is_not_queued_ends_a_sleeping_poll() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    let (local, interrupter) = (subscriber.local().clone(), subscriber.interrupter());
    let fd = local.words().fd.as_ref().unwrap();
    fd.failing.store(1, Ordering::Relaxed);
    let armed = armed_to_sleep(&subscriber);
    let polling = thread::spawn(move || waited(&mut subscriber, WAIT));
    // Past an arm that saw no bit: the failed send is followed by one while the poll is parked.
    eventually("the poll armed to sleep", || armed.load(Ordering::SeqCst));
    let started = Instant::now();
    assert_eq!(interrupter.interrupt_counting(), 2);
    assert!(polling.join().unwrap());
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// A receive that returns an interrupt takes its kick too: no poll after it ends for it.
#[test]
fn a_receive_cancelled_by_an_interrupt_leaves_no_poll_to_end_for_it() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    subscriber.interrupter().interrupt();
    let short = Duration::from_millis(50);
    assert_eq!(subscriber.recv_timeout(short), Err(RecvError::Cancelled));
    assert!(!waited(&mut subscriber, Duration::from_millis(20)));
}

/// A `prepare_wait` that returns for a record leaves the interrupt's kick to the next one.
#[test]
fn a_poll_that_returns_for_a_record_leaves_the_kick_to_the_next() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    publisher.publish(&num(1)).unwrap();
    subscriber.interrupter().interrupt();
    assert!(!subscriber.prepare_wait(), "a record is unread");
    assert_eq!(subscriber.try_recv().map(|bytes| value(&bytes)), Ok(1));
    assert!(!subscriber.prepare_wait(), "the interrupt's kick");
    assert!(subscriber.prepare_wait());
    subscriber.finish_wait();
    assert_eq!(recv(&mut subscriber), Err(RecvError::Cancelled));
}

#[test]
fn a_bounded_fd_wait_times_out_and_the_next_receive_still_works() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    let started = Instant::now();
    let timeout = Duration::from_millis(50);
    assert_eq!(subscriber.recv_timeout(timeout), Err(RecvError::Timeout));
    assert!(started.elapsed() >= timeout);
    publisher.publish(&num(5)).unwrap();
    assert_eq!(recv(&mut subscriber).map(|bytes| value(&bytes)), Ok(5));
}

/// The publisher's end closes after its end record, which every later receive returns.
#[test]
fn a_dropped_publisher_ends_an_fd_wait_and_every_later_receive_with_its_end() {
    let publisher = topic(64 << 10);
    let subscriber = fd_subscriber(&publisher);
    let receiving = asleep(subscriber, |subscriber| {
        let first = recv(subscriber);
        for _ in 0..3 {
            assert_eq!(recv(subscriber), first);
        }
        first
    });
    drop(publisher);
    assert_eq!(
        receiving.join().unwrap(),
        Err(RecvError::Closed(Closed::End))
    );
}

/// A subscriber dropped armed on a socket whose sends fail with `EPIPE` still drops.
#[test]
fn a_subscriber_dropped_armed_on_a_shut_down_socket_drops() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    assert!(subscriber.prepare_wait());
    // The socket state a publisher's `SHUT_WR` makes: `RCV_SHUTDOWN` here, `SEND_SHUTDOWN` there.
    rustix::net::shutdown(subscriber.wait_fd().unwrap(), rustix::net::Shutdown::Read).unwrap();
    let (dropped, done) = std::sync::mpsc::channel();
    thread::spawn(move || {
        drop(subscriber);
        dropped.send(()).unwrap();
    });
    assert!(done.recv_timeout(Duration::from_secs(10)).is_ok());
}

/// A copy of the publisher's end of the wake socket, its send buffer raised past the protocol's.
fn hostile_end(subscriber: &Subscriber<Bytes>) -> std::os::fd::OwnedFd {
    let words = subscriber.local().words();
    let end = words
        .fd
        .as_ref()
        .unwrap()
        .sender()
        .try_clone_to_owned()
        .unwrap();
    rustix::net::sockopt::set_socket_send_buffer_size(&end, 1 << 20).unwrap();
    end
}

/// Send 64 KiB at a time on `end` until the socket takes no more.
fn fill(end: &std::os::fd::OwnedFd) {
    let chunk = vec![0u8; 64 << 10];
    let flags = rustix::net::SendFlags::DONTWAIT | rustix::net::SendFlags::NOSIGNAL;
    loop {
        match rustix::net::send(end, &chunk, flags) {
            Ok(_) => {}
            Err(rustix::io::Errno::AGAIN) => return,
            Err(error) => panic!("send: {error}"),
        }
    }
}

/// A publisher that keeps the wake socket full holds no receive past its deadline.
#[test]
fn a_receive_on_a_socket_a_publisher_keeps_full_ends_at_its_deadline() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    let end = hostile_end(&subscriber);
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let flooding = thread::spawn(move || {
        let started = Instant::now();
        while !stopped.load(Ordering::Relaxed) && started.elapsed() < Duration::from_secs(5) {
            fill(&end);
        }
    });
    let started = Instant::now();
    let got = subscriber.recv_timeout(Duration::from_millis(100));
    let took = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    flooding.join().unwrap();
    assert_eq!(got, Err(RecvError::Timeout));
    assert!(
        took < Duration::from_secs(2),
        "the 100 ms wait took {took:?}"
    );
}

/// Bytes one `finish_wait` leaves end the next `prepare_wait`, which an edge-triggered poll needs.
#[test]
fn a_poll_loop_drains_a_full_socket_in_parts_and_then_sleeps_until_a_wake() {
    let publisher = topic(64 << 10);
    let mut subscriber = fd_subscriber(&publisher);
    fill(&hostile_end(&subscriber));
    assert!(subscriber.prepare_wait());
    subscriber.finish_wait();
    assert_eq!(poll_for(&subscriber, Duration::ZERO), 1, "the bytes left");
    assert!(
        !subscriber.prepare_wait(),
        "a poll would sleep on bytes left"
    );
    let mut calls = 1;
    while !subscriber.prepare_wait() {
        calls += 1;
        assert!(calls < 10_000, "never drained");
    }
    assert_eq!(poll_for(&subscriber, Duration::ZERO), 0, "drained");
    subscriber.interrupter().interrupt();
    assert_eq!(poll_for(&subscriber, WAIT), 1, "the interrupt's byte");
    subscriber.finish_wait();
}

/// AC-15.2.18: a subscriber that raises `WAITING` and never reads; a million publishes, one byte.
#[test]
fn a_waiting_subscriber_that_never_reads_its_socket_holds_no_publish() {
    let publishes = std::env::var("RSBINDER_TOPIC_UNREAD_PUBLISHES")
        .map_or(1_000_000, |count| count.parse().unwrap());
    let publisher = topic(64 << 10);
    let subscriber = fd_subscriber(&publisher);
    subscriber.local().arm(subscriber.local().words().seq());
    for n in 0..publishes {
        publisher.publish(&num(n)).unwrap();
    }
    assert_eq!(publisher.sends(), 1);
    drop(subscriber);
}

/// AC-15.2.18: a wait counted at every commit, never read: a byte each until `EAGAIN`, no wait.
#[test]
fn a_subscriber_that_waits_anew_at_every_commit_and_never_reads_holds_no_publish() {
    const COMMITS: u64 = 100_000;
    let publisher = topic(64 << 10);
    let subscriber = fd_subscriber(&publisher);
    let words = subscriber.local().words();
    for n in 0..COMMITS {
        words.begin_wait(words.seq());
        publisher.publish(&num(n as u32)).unwrap();
    }
    assert_eq!(publisher.sends(), COMMITS);
    let socket = subscriber.wait_fd().unwrap();
    let queued = rustix::io::ioctl_fionread(socket).unwrap();
    assert!(
        queued < COMMITS,
        "the socket took {queued} bytes: no send met a full buffer"
    );
}

/// AC-15.2.18: the subscriber's end closed, and `SIGPIPE` at its default; the link goes.
#[test]
fn a_subscriber_that_closes_its_socket_is_taken_out_and_raises_no_sigpipe() {
    if !crate::connect_tests::alone(
        "fd_wait_tests::a_subscriber_that_closes_its_socket_is_taken_out_and_raises_no_sigpipe",
    ) {
        return;
    }
    // SAFETY: in a process of its own (`alone`), with no other thread that handles signals.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let publisher = topic(64 << 10);
    let (fd, page) = crate::layout::ControlPage::create(0).unwrap();
    let token = crate::connect::Token::detached();
    let (theirs, polled) = crate::wake::pair().unwrap();
    let request = crate::aidl::SubscribeRequest::SubscribeRequest {
        version: crate::layout::VERSION as i32,
        r#type: Bytes::type_name().to_owned(),
        token: Some(token.clone()),
        control: Some(rsbinder::ParcelFileDescriptor::from(fd)),
        pins: 0,
        wake: crate::aidl::ITopic::WAKE_FD,
        wakeFd: Some(rsbinder::ParcelFileDescriptor::new(theirs)),
    };
    {
        let _asking = crate::connect::Asking::new(&request);
        publisher.acceptor().accept(&request).unwrap();
    }
    // The publisher keeps its end; this side closes both of its own, as a dropped one would.
    drop(request);
    drop(polled);
    // A wait on `seq` 0, which the publish below moves on from.
    page.state()
        .fetch_or(crate::layout::WAITING, Ordering::SeqCst);
    assert_eq!(publisher.subscriber_count(), 1);
    publisher.publish(&num(1)).unwrap();
    assert_eq!(publisher.sends(), 1);
    assert_eq!(publisher.subscriber_count(), 0);
}

/// AC-15.2.18: a `recv` held in a page fault holds its socket's lock, and no publish waits for it.
#[test]
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "x86",
    target_arch = "arm"
))]
fn a_receive_held_in_a_page_fault_holds_no_publish() {
    use std::os::fd::{FromRawFd, OwnedFd};

    // `_IOWR(0xAA, nr, size)` of `linux/userfaultfd.h`, in the encoding of these architectures.
    const UFFDIO_API: u32 = 0xC018_AA3F;
    const UFFDIO_REGISTER: u32 = 0xC020_AA00;
    #[repr(C)]
    struct Api {
        api: u64,
        features: u64,
        ioctls: u64,
    }
    #[repr(C)]
    struct Register {
        start: u64,
        len: u64,
        mode: u64,
        ioctls: u64,
    }

    // SAFETY: a system call that takes flags alone.
    let uffd = unsafe { libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC) };
    if uffd < 0 {
        let why = std::io::Error::last_os_error();
        println!(
            "skipped: holding a fault in kernel mode needs root or \
             vm.unprivileged_userfaultfd=1 ({why})"
        );
        return;
    }
    // SAFETY: the system call returned this fd and nothing else owns it.
    let uffd = unsafe { OwnedFd::from_raw_fd(uffd as i32) };
    let mut api = Api {
        api: 0xAA,
        features: 0,
        ioctls: 0,
    };
    // SAFETY: `api` is the struct this request reads and writes.
    assert_eq!(
        unsafe { libc::ioctl(uffd.as_raw_fd(), UFFDIO_API as _, &mut api) },
        0
    );
    let page = rustix::param::page_size();
    // SAFETY: a new private anonymous mapping, unmapped below.
    let region = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(region, libc::MAP_FAILED);
    let mut register = Register {
        start: region as u64,
        len: page as u64,
        mode: 1,
        ioctls: 0,
    };
    // SAFETY: `register` is the struct this request reads and writes; the range is mapped.
    assert_eq!(
        unsafe { libc::ioctl(uffd.as_raw_fd(), UFFDIO_REGISTER as _, &mut register) },
        0
    );

    let publisher = topic(64 << 10);
    let subscriber = fd_subscriber(&publisher);
    // A byte in the socket for the `recv` to copy into the page that faults.
    subscriber.local().arm(subscriber.local().words().seq());
    publisher.publish(&num(0)).unwrap();
    let socket = subscriber.wait_fd().unwrap().as_raw_fd();
    let address = region as usize;
    // SAFETY: one byte into the mapping, which outlives the thread (joined below).
    let held = thread::spawn(move || unsafe { libc::recv(socket, address as *mut _, 1, 0) });
    // The fault is reported once the `recv` is in it, holding the socket's receive lock.
    let mut fds = [PollFd::new(&uffd, PollFlags::IN)];
    let ten = Timespec::try_from(Duration::from_secs(10)).unwrap();
    assert_eq!(poll(&mut fds, Some(&ten)).unwrap(), 1);
    let started = Instant::now();
    for n in 1..=1000 {
        let words = subscriber.local().words();
        words.begin_wait(words.seq());
        publisher.publish(&num(n)).unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(publisher.sends(), 1001);
    // Without its userfaultfd the fault is served as any other, and the `recv` returns.
    drop(uffd);
    assert_eq!(held.join().unwrap(), 1);
    // SAFETY: the mapping made above, which nothing uses now.
    unsafe { libc::munmap(region, page) };
}

#[cfg(feature = "tokio")]
mod reactor {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_io()
            .enable_time()
            .build()
            .unwrap()
    }

    fn threads() -> usize {
        std::fs::read_dir("/proc/self/task").unwrap().count()
    }

    #[test]
    fn an_async_receive_gets_every_record() {
        const RECORDS: u32 = 10_000;
        let runtime = runtime();
        let publisher = Arc::new(topic(1 << 20));
        let mut subscriber = fd_subscriber(&publisher);
        let receiving = runtime.spawn(async move {
            for n in 0..RECORDS {
                let waited = tokio::time::timeout(WAIT, subscriber.recv_async()).await;
                let got = waited.expect("a wake was lost").map(|bytes| value(&bytes));
                assert_eq!(got, Ok(n));
            }
        });
        for n in 0..RECORDS {
            publisher.publish(&num(n)).unwrap();
            if n % 64 == 0 {
                thread::sleep(Duration::from_micros(50));
            }
        }
        runtime.block_on(receiving).unwrap();
    }

    /// AC-15.2.17: eight async receives wait on the reactor, and the process has no more threads.
    #[test]
    fn async_receives_on_fd_wakes_hold_no_thread() {
        if !crate::connect_tests::alone(
            "fd_wait_tests::reactor::async_receives_on_fd_wakes_hold_no_thread",
        ) {
            return;
        }
        let runtime = runtime();
        let publisher = topic(64 << 10);
        let waiting = |wake: Wake| {
            let before = threads();
            let receiving: Vec<_> = (0..8)
                .map(|_| {
                    let mut subscriber = woken_by(&publisher, wake);
                    let local = subscriber.local().clone();
                    let task = runtime.spawn(async move {
                        let waited = tokio::time::timeout(WAIT, subscriber.recv_async()).await;
                        waited.is_ok_and(|got| got.is_ok())
                    });
                    eventually("the wait sleeps", || local.is_parked());
                    task
                })
                .collect();
            let during = threads();
            publisher.publish(&num(1)).unwrap();
            for task in receiving {
                assert!(runtime.block_on(task).unwrap());
            }
            during.saturating_sub(before)
        };
        assert_eq!(waiting(Wake::Fd), 0);
        // The futex's waits hold a pool thread each: what the count above would have seen.
        assert!(waiting(Wake::Futex) >= 8);
    }

    /// A dropped `recv_async` leaves nothing behind: no wait raised, nothing on the pool.
    #[test]
    fn a_dropped_async_receive_leaves_no_wait() {
        let runtime = runtime();
        let publisher = topic(64 << 10);
        let mut subscriber = fd_subscriber(&publisher);
        let local = subscriber.local().clone();
        {
            let _enter = runtime.enter();
            let mut future = std::pin::pin!(subscriber.recv_async());
            let polled = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()));
            assert!(matches!(polled, Poll::Pending), "nothing to receive yet");
            assert!(local.is_parked());
        }
        assert!(!local.is_parked());
        let state = subscriber
            .local()
            .words()
            .control
            .state()
            .load(Ordering::SeqCst);
        assert_eq!(state & crate::layout::WAITING, 0);
        publisher.publish(&num(2)).unwrap();
        assert_eq!(publisher.sends(), 0);
        let got = runtime.block_on(subscriber.recv_async());
        assert_eq!(got.map(|bytes| value(&bytes)), Ok(2));
    }

    #[test]
    fn a_commit_just_before_an_async_receive_waits_is_found_by_its_second_look() {
        let runtime = runtime();
        let publisher = Arc::new(topic(64 << 10));
        let mut subscriber = fd_subscriber(&publisher);
        commit_before_the_wait(&publisher, &mut subscriber);
        let got = runtime.block_on(async {
            let waited = tokio::time::timeout(Duration::from_secs(10), subscriber.recv_async());
            waited.await.map_err(drop)
        });
        assert_eq!(got.map(|got| got.map(|bytes| value(&bytes))), Ok(Ok(8)));
        assert_eq!(publisher.sends(), 0);
    }

    /// An async wait sleeps between wakes: it arms once per wake, not once per spin.
    #[test]
    fn an_async_wait_arms_once_per_wake() {
        let runtime = runtime();
        let publisher = topic(64 << 10);
        let mut subscriber = fd_subscriber(&publisher);
        let local = subscriber.local().clone();
        let waits = || local.arms.load(Ordering::SeqCst);
        let receiving = runtime.spawn(async move {
            let mut got = Vec::new();
            for _ in 0..2 {
                let waited = tokio::time::timeout(WAIT, subscriber.recv_async()).await;
                got.push(waited.expect("a wake was lost").map(|bytes| value(&bytes)));
            }
            got
        });
        eventually("the first wait", || local.is_parked());
        publisher.publish(&num(1)).unwrap();
        eventually("the second wait", || waits() >= 2 && local.is_parked());
        thread::sleep(Duration::from_millis(100));
        assert!(waits() <= 3, "{} waits for one wake", waits());
        publisher.publish(&num(2)).unwrap();
        assert_eq!(runtime.block_on(receiving).unwrap(), [Ok(1), Ok(2)]);
    }

    #[test]
    fn an_interrupt_ends_an_async_fd_wait() {
        let runtime = runtime();
        let publisher = topic(64 << 10);
        let mut subscriber = fd_subscriber(&publisher);
        let interrupter = subscriber.interrupter();
        let local = subscriber.local().clone();
        let receiving = runtime.spawn(async move {
            let waited = tokio::time::timeout(Duration::from_secs(10), subscriber.recv_async());
            waited.await.map(|got| got.map(drop))
        });
        eventually("the wait sleeps", || local.is_parked());
        interrupter.interrupt();
        assert_eq!(
            runtime.block_on(receiving).unwrap().ok(),
            Some(Err(RecvError::Cancelled))
        );
    }

    /// A runtime made per receive: the one after a runtime's drop waits on the next one's reactor.
    #[test]
    fn an_async_receive_after_its_reactors_runtime_is_dropped_waits_on_its_own() {
        let publisher = Arc::new(topic(64 << 10));
        let mut subscriber = fd_subscriber(&publisher);
        for n in 1..=2 {
            let local = subscriber.local().clone();
            let later = publisher.clone();
            let publishing = thread::spawn(move || {
                eventually("the wait sleeps", || local.is_parked());
                later.publish(&num(n)).unwrap();
            });
            let arms = subscriber.local().arms.load(Ordering::SeqCst);
            let got = runtime().block_on(async {
                let waited = tokio::time::timeout(WAIT, subscriber.recv_async()).await;
                waited.map(|got| got.map(|bytes| value(&bytes)))
            });
            assert_eq!(got.ok(), Some(Ok(n)), "receive {n}");
            let spun = subscriber.local().arms.load(Ordering::SeqCst) - arms;
            assert!(spun <= 3, "receive {n} armed {spun} times");
            publishing.join().unwrap();
        }
    }

    /// An async wait on a socket left full drains it, then wakes at the interrupt it could hide.
    #[test]
    fn an_async_wait_on_a_full_socket_drains_it_and_wakes_at_an_interrupt() {
        let runtime = runtime();
        let publisher = topic(64 << 10);
        let mut subscriber = fd_subscriber(&publisher);
        fill(&hostile_end(&subscriber));
        let interrupter = subscriber.interrupter();
        let local = subscriber.local().clone();
        let receiving = runtime.spawn(async move {
            let waited = tokio::time::timeout(WAIT, subscriber.recv_async());
            waited.await.map(|got| got.map(drop))
        });
        let arms = || local.arms.load(Ordering::SeqCst);
        eventually("a drain, then a wait", || arms() >= 2 && local.is_parked());
        interrupter.interrupt();
        assert_eq!(
            runtime.block_on(receiving).unwrap().ok(),
            Some(Err(RecvError::Cancelled))
        );
    }

    /// The caller's fd, as the reactor takes it.
    struct Theirs(std::os::fd::RawFd);

    impl AsRawFd for Theirs {
        fn as_raw_fd(&self) -> std::os::fd::RawFd {
            self.0
        }
    }

    /// A reactor that refuses the fd (registered by the caller already) is a runtime error, each
    /// time, apart from an interrupt; once it takes the fd, the wait goes on.
    #[test]
    fn a_reactor_that_refuses_the_fd_is_a_runtime_error_and_not_a_cancel() {
        use tokio::io::{unix::AsyncFd, Interest};
        let runtime = runtime();
        let publisher = topic(64 << 10);
        let mut subscriber = fd_subscriber(&publisher);
        let fd = Theirs(subscriber.wait_fd().unwrap().as_raw_fd());
        let refused = Err(RecvError::Runtime(std::io::ErrorKind::AlreadyExists));
        let theirs = runtime.block_on(async {
            // SAFETY: the subscriber, which owns the fd, outlives this registration.
            let theirs = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE) };
            let theirs = theirs.unwrap();
            assert_eq!(subscriber.recv_async().await.map(drop), refused);
            assert_eq!(subscriber.recv_async().await.map(drop), refused);
            theirs
        });
        drop(theirs);
        publisher.publish(&num(4)).unwrap();
        let got = runtime.block_on(async {
            let waited = tokio::time::timeout(WAIT, subscriber.recv_async()).await;
            waited.map(|got| got.map(|bytes| value(&bytes)))
        });
        assert_eq!(got.ok(), Some(Ok(4)));
        // With no record, the wait registers the fd now and an interrupt ends it as `Cancelled`.
        let interrupter = subscriber.interrupter();
        let local = subscriber.local().clone();
        let receiving = runtime.spawn(async move {
            let waited = tokio::time::timeout(WAIT, subscriber.recv_async());
            waited.await.map(|got| got.map(drop))
        });
        eventually("the wait sleeps", || local.is_parked());
        interrupter.interrupt();
        assert_eq!(
            runtime.block_on(receiving).unwrap().ok(),
            Some(Err(RecvError::Cancelled))
        );
    }
}
