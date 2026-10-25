// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A publisher and a subscriber in two processes over the kernel binder (plan 15 AC-15.2.4,
//! 2.13, 2.20, 2.23, 2.44, and the end of a subscription whose publisher died). `#[ignore]`d: they
//! need a binder device and a service manager that lets any name register:
//!
//! ```text
//! target/debug/rsb_hub --config tests/policy/permissive.toml &
//! cargo test -p rsbinder-topic --test two_process -- --ignored --test-threads=1
//! ```
//!
//! The other process is this test binary again, running `child` with `RSBINDER_TOPIC_ROLE` set;
//! it answers on stdout with lines that start with `topic:` and exits when its stdin closes.
//! The publisher's side counts its memfds (fds open on one, mappings of one), so the tests run
//! one at a time.
//!
//! A reply whose fds the subscriber has no numbers for ends by driver. The C driver (`binder.c`,
//! `binder_thread_read`, "fd fixups failed") answers it with `BR_FAILED_REPLY` and records no
//! errno, which rsbinder reports as `FailedTransaction`. The Rust driver
//! (`drivers/android/binder/transaction.rs`) drops the reply and sends `BR_FAILED_REPLY` only to
//! the sender of a transaction, so the caller waits for ever, and the test returns before it.
//! Either way the publisher added the subscription and holds the control page mapped; the
//! subscriber, still alive, closed it, and the next commit takes the subscription out.

#![cfg(any(target_os = "linux", target_os = "android"))]

mod common;

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use common::{eventually, init, name, signal, Peer, NAME, ROLE, WAIT};
use rsbinder::fmq::shm::{self, CreateOptions, Seals};
use rsbinder::{hub, ExceptionCode, Interface, ParcelFileDescriptor, SIBinder, StatusCode};
use rsbinder_topic::aidl::ITopic::{ITopic, ERROR_VERSION};
use rsbinder_topic::aidl::SubscribeRequest::SubscribeRequest;
use rsbinder_topic::{
    wait_any, Backpressure, Bytes, Closed, Disconnected, NamedSubscriber, Payload, PoolPolicy,
    Publisher, PublisherPolicy, RecvError, Subscriber, SubscriberPolicy, Wait, Wake,
};

/// This process's memfds: the fds open on one, and the mappings of one.
fn memfds() -> (usize, usize) {
    let fds = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| target.to_string_lossy().starts_with("/memfd:"))
        .count();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    (
        fds,
        maps.lines().filter(|line| line.contains("/memfd:")).count(),
    )
}

fn publish(tag: &str) -> (String, Publisher<Bytes>) {
    init();
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).unwrap();
    let name = name(tag);
    hub::add_service(&name, publisher.as_binder()).unwrap();
    (name, publisher)
}

#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_killed_or_dropped_subscriber_is_taken_out_and_its_page_unmapped() {
    let (name, publisher) = publish("gone");
    let before = memfds();
    let mut peer = Peer::spawn("subscribe", &name);
    assert_eq!(peer.says(), "subscribed");
    assert_eq!(publisher.subscriber_count(), 1);
    // The control page is mapped and its fd closed.
    assert_eq!(memfds(), (before.0, before.1 + 1));
    peer.kill();
    // The death notification alone, with no publish or count to look at the page.
    eventually("the death to unmap the page", || memfds() == before);
    assert_eq!(publisher.subscriber_count(), 0);

    let mut peer = Peer::spawn("subscribe", &name);
    assert_eq!(peer.says(), "subscribed");
    assert_eq!(publisher.subscriber_count(), 1);
    peer.tell("drop");
    assert_eq!(peer.says(), "dropped");
    // No death: the page says `CLOSED`, and the count takes it out.
    assert_eq!(publisher.subscriber_count(), 0);
    assert_eq!(memfds(), before);
}

/// Lend a slot for `value` and publish it; the slot's address, which names it.
fn lend(publisher: &Publisher<Bytes>, value: &[u8]) -> usize {
    let mut loan = publisher.loan_slice(value.len()).unwrap();
    loan.copy_from_slice(value);
    let at = loan.as_ptr() as usize;
    loan.publish().unwrap();
    at
}

/// AC-15.3.4: a slot a killed subscriber pinned is lent again once its death takes it out.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_slot_a_killed_subscriber_pinned_is_lent_again() {
    init();
    // 2 pins, one to write, a depth of 1 and 1 loan: 5 slots.
    let policy = PublisherPolicy {
        max_subscribers: 2,
        max_per_uid: 2,
        pool: Some(PoolPolicy {
            slot_bytes: 4096,
            depth: 1,
            max_loans: 1,
            huge_pages: false,
        }),
        ..PublisherPolicy::default()
    };
    let publisher = Publisher::<Bytes>::new(&policy).unwrap();
    let name = name("pinned");
    hub::add_service(&name, publisher.as_binder()).unwrap();
    // A subscriber that reads nothing: no slot is read by everyone, so the oldest go round.
    let idle = SubscriberPolicy::default();
    let _idle = Subscriber::<Bytes>::request(&publisher.as_binder(), &idle).unwrap();
    let mut peer = Peer::spawn("pin", &name);
    assert_eq!(peer.says(), "subscribed");
    let pinned = lend(&publisher, b"held");
    assert_eq!(peer.says(), "pinned held");
    let lent = |count| {
        (0..count)
            .map(|_| lend(&publisher, b"next"))
            .collect::<Vec<_>>()
    };
    assert!(!lent(5 * 4).contains(&pinned), "a pinned slot lent");
    assert_eq!(publisher.subscriber_count(), 2);
    peer.kill();
    eventually("the death to take the subscription out", || {
        publisher.subscriber_count() == 1
    });
    assert!(
        lent(5).contains(&pinned),
        "the dead subscriber's slot not lent again"
    );
}

/// AC-15.2.33: a subscription from another process is named with that process's uid and pid.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_publishers_stats_name_a_subscriber_of_another_process() {
    let (name, publisher) = publish("stats");
    let mut peer = Peer::spawn("subscribe", &name);
    assert_eq!(peer.says(), "subscribed");
    publisher.publish(&Bytes(vec![7; 9])).unwrap();
    let stats = publisher.stats();
    assert_eq!((stats.published, stats.bytes), (1, 9));
    let [info] = stats.subscribers.as_slice() else {
        panic!("{stats:?}");
    };
    assert_eq!(info.pid, peer.child.id() as i32);
    assert_eq!(info.uid, rustix::process::getuid().as_raw());
    // Not read: the record, 8 bytes of header and 16 of padded payload.
    assert_eq!(info.lag_bytes, 24);
    peer.tell("drop");
    assert_eq!(peer.says(), "dropped");
    assert!(publisher.stats().subscribers.is_empty());
}

#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn subscribers_killed_while_they_subscribe_leave_nothing() {
    let (name, publisher) = publish("killed");
    let before = memfds();
    let kills: u32 = std::env::var("RSBINDER_TOPIC_KILLS")
        .ok()
        .and_then(|kills| kills.parse().ok())
        .unwrap_or(1000);
    let mut seed = u64::from(std::process::id()) | 1;
    for _ in 0..kills {
        let mut peer = Peer::spawn("subscribe-loop", &name);
        assert_eq!(peer.says(), "looping");
        // xorshift: a delay up to 2 ms, so that the kill lands anywhere in a subscription.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        std::thread::sleep(Duration::from_micros(seed % 2000));
        peer.kill();
        // Some of the dropped ones wait for a commit; one keeps the list short.
        publisher.publish(&Bytes(Vec::new())).unwrap();
    }
    // The last kill came after the last publish: its death notification takes it out.
    eventually("every page unmapped", || memfds() == before);
    assert_eq!(publisher.subscriber_count(), 0);
}

#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_request_from_another_process_is_refused_for_what_is_wrong_with_it() {
    let (name, publisher) = publish("refused");
    let before = memfds();
    let peer = Peer::spawn("refuse", &name);
    assert_eq!(peer.says(), format!("version: specific {ERROR_VERSION}"));
    assert_eq!(peer.says(), "type: BadType");
    // The topic's own binder: one of the publisher's process, not the caller's.
    assert_eq!(peer.says(), "foreign token: BadValue");
    assert_eq!(peer.says(), "done");
    assert_eq!(publisher.subscriber_count(), 0);
    assert_eq!(memfds(), before);
}

#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_reply_the_subscriber_refuses_leaves_its_page_closed() {
    let (name, publisher) = publish("small");
    let before = memfds();
    let peer = Peer::spawn("small-ring", &name);
    assert_eq!(peer.says(), "refused: BadValue");
    // As below, with the reply delivered and refused (a ring above the subscriber's bound).
    assert_eq!(memfds(), (before.0, before.1 + 1));
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(memfds(), before);
    assert_eq!(publisher.subscriber_count(), 0);
}

/// Whether the binder driver is the Rust one: its module is loaded.
fn rust_binder() -> bool {
    std::path::Path::new("/sys/module/rust_binder").exists()
}

/// A reply with fds the subscriber has no numbers for: the C driver fails it; the Rust one hangs.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_reply_the_subscriber_cannot_take_leaves_its_page_closed() {
    if rust_binder() {
        // The Rust driver drops a reply whose fds cannot be installed; the caller never returns.
        eprintln!("skipped: the Rust binder driver leaves the caller of a failed reply waiting");
        return;
    }
    let (name, publisher) = publish("nofile");
    let before = memfds();
    let peer = Peer::spawn("nofile", &name);
    // The C driver's `BR_FAILED_REPLY`: no fd number for the topic memory, and no errno.
    assert_eq!(peer.says(), "failed: FailedTransaction");
    // The publisher still maps the page the subscriber closed; the next commit drops it.
    assert_eq!(memfds(), (before.0, before.1 + 1));
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(memfds(), before);
    assert_eq!(publisher.subscriber_count(), 0);
}

#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_subscriber_reads_what_a_dead_publisher_committed_then_learns_it_died() {
    init();
    let name = name("died");
    let mut peer = Peer::spawn("publish", &name);
    assert_eq!(peer.says(), "ready");
    let topic = hub::check_service(&name).expect("the publisher's binder");
    let mut subscriber =
        Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default()).unwrap();
    peer.tell("publish 3");
    assert_eq!(peer.says(), "published");
    peer.kill();
    let mut received = Vec::new();
    eventually("the end of the subscription", || {
        match subscriber.try_recv() {
            Ok(value) => {
                received.push(value);
                false
            }
            Err(RecvError::Empty) => false,
            Err(RecvError::Closed(Closed::PublisherDied)) => true,
            Err(other) => panic!("{other}"),
        }
    });
    assert_eq!(
        received,
        (0..3).map(|byte| Bytes(vec![byte])).collect::<Vec<_>>()
    );
    let died = Err(RecvError::Closed(Closed::PublisherDied));
    assert_eq!(subscriber.try_recv(), died);
    assert_eq!(subscriber.try_latest(), died);
}

/// Whether the service manager is Android 10's, which has no notices and refuses a name subscriber.
fn without_notices(name: &str) -> bool {
    if rsbinder::sdk_at_least(30) {
        return false;
    }
    let refused = NamedSubscriber::<Bytes>::subscribe(name, &SubscriberPolicy::default());
    let refused = refused.err().map(|status| status.transaction_error());
    assert_eq!(refused, Some(StatusCode::UnknownTransaction));
    true
}

/// AC-15.2.20: across a publisher's restart, one `Disconnected`, then the new one's first record.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_name_subscriber_outlives_a_killed_publisher_and_reads_the_next_from_its_first_record() {
    init();
    let name = name("named");
    if without_notices(&name) {
        return;
    }
    let policy = SubscriberPolicy::default();
    let mut subscriber = NamedSubscriber::<Bytes>::subscribe(&name, &policy).unwrap();
    assert!(!subscriber.wait_connected(Duration::from_millis(100)));
    let mut peer = Peer::spawn("advertise", &name);
    assert_eq!(peer.says(), "ready");
    assert!(subscriber.wait_connected(WAIT));
    peer.tell("publish 2");
    assert_eq!(peer.says(), "published");
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![0])));
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    peer.kill();
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Disconnected(Disconnected::Ended(
            Closed::PublisherDied
        )))
    );
    let mut peer = Peer::spawn("advertise", &name);
    assert_eq!(peer.says(), "ready");
    // Published whether or not the subscriber has connected yet.
    peer.tell("publish 3");
    assert_eq!(peer.says(), "published");
    for byte in 0..3 {
        assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![byte])));
    }
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
}

/// What each of several subscribers received, in order.
type Received = Vec<Vec<Result<Bytes, RecvError>>>;

/// One thread, a poll loop of `wait_any`: receive into `got` until `done` holds of it.
fn poll_until(
    subscribers: &mut [NamedSubscriber<Bytes>],
    got: &mut Received,
    done: impl Fn(&Received) -> bool,
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

/// AC-15.2.36: three name subscribers on one poll loop go on through the same fds past a restart.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn name_subscribers_on_one_poll_loop_go_on_through_a_killed_publisher() {
    init();
    let names = [name("loop.a"), name("loop.b"), name("loop.c")];
    if without_notices(&names[0]) {
        return;
    }
    let mut peers: Vec<Peer> = names
        .iter()
        .map(|name| Peer::spawn("advertise", name))
        .collect();
    for peer in &peers {
        assert_eq!(peer.says(), "ready");
    }
    let policy = SubscriberPolicy {
        wake: Wake::Fd,
        ..SubscriberPolicy::default()
    };
    let mut subscribers: Vec<_> = names
        .iter()
        .map(|name| NamedSubscriber::<Bytes>::subscribe(name, &policy).unwrap())
        .collect();
    for subscriber in &subscribers {
        assert!(subscriber.wait_connected(WAIT));
    }
    let fds = |subscribers: &[NamedSubscriber<Bytes>]| -> Vec<i32> {
        let fds = subscribers.iter().map(|s| s.wait_fd().unwrap());
        fds.map(|fd| fd.as_raw_fd()).collect()
    };
    let before = fds(&subscribers);
    let mut got: Received = vec![Vec::new(), Vec::new(), Vec::new()];
    for peer in &mut peers {
        peer.tell("publish 1");
        assert_eq!(peer.says(), "published");
    }
    poll_until(&mut subscribers, &mut got, |got| {
        got.iter().all(|got| got.len() == 1)
    });
    peers[1].kill();
    poll_until(&mut subscribers, &mut got, |got| got[1].len() == 2);
    peers[1] = Peer::spawn("advertise", &names[1]);
    assert_eq!(peers[1].says(), "ready");
    peers[1].tell("publish 2");
    assert_eq!(peers[1].says(), "published");
    poll_until(&mut subscribers, &mut got, |got| got[1].len() == 4);
    let died = Err(RecvError::Disconnected(Disconnected::Ended(
        Closed::PublisherDied,
    )));
    assert_eq!(
        got[1],
        [
            Ok(Bytes(vec![0])),
            died,
            Ok(Bytes(vec![0])),
            Ok(Bytes(vec![1]))
        ]
    );
    assert_eq!((got[0].len(), got[2].len()), (1, 1));
    assert_eq!(fds(&subscribers), before);
}

/// AC-15.2.5, 2.11: 8 sleeping subscribers of a killed publisher end within 1 s, 1000 times.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn sleeping_subscribers_learn_within_a_second_that_their_publisher_died() {
    sleepers_of_a_killed_publisher(Wake::Futex);
}

/// The same with the fd wake: the death notice or the socket's hang-up, whichever comes first.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn sleeping_fd_subscribers_learn_within_a_second_that_their_publisher_died() {
    sleepers_of_a_killed_publisher(Wake::Fd);
}

fn sleepers_of_a_killed_publisher(wake: Wake) {
    init();
    let deaths: u32 = std::env::var("RSBINDER_TOPIC_DEATHS")
        .ok()
        .and_then(|deaths| deaths.parse().ok())
        .unwrap_or(1000);
    let policy = SubscriberPolicy {
        wake,
        ..SubscriberPolicy::default()
    };
    for death in 0..deaths {
        let name = name(&format!("deaths.{death}"));
        let mut peer = Peer::spawn("publish", &name);
        assert_eq!(peer.says(), "ready");
        let topic = hub::check_service(&name).expect("the publisher's binder");
        let receiving: Vec<_> = (0..8)
            .map(|_| {
                let mut subscriber = Subscriber::<Bytes>::request(&topic, &policy).unwrap();
                std::thread::spawn(move || {
                    let first = subscriber.recv();
                    let killed = subscriber.recv();
                    (first, killed, Instant::now())
                })
            })
            .collect();
        peer.tell("publish 1");
        assert_eq!(peer.says(), "published");
        // Past the spin of 20 µs: each second `recv` sleeps by now.
        std::thread::sleep(Duration::from_millis(5));
        let killed = Instant::now();
        peer.kill();
        eventually("every subscriber is out of its sleep", || {
            receiving.iter().all(|r| r.is_finished())
        });
        for receiving in receiving {
            let (first, then, at) = receiving.join().unwrap();
            assert_eq!(first, Ok(Bytes(vec![0])));
            assert_eq!(then, Err(RecvError::Closed(Closed::PublisherDied)));
            let took = at.saturating_duration_since(killed);
            assert!(took < Duration::from_secs(1), "death {death}: {took:?}");
        }
    }
}

/// AC-15.2.17 across processes: `wait_any` on two publishers; one's death ends its subscription.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn fd_subscribers_of_two_publishers_wait_on_one_thread_and_learn_of_a_death() {
    init();
    let names = [name("fd.first"), name("fd.second")];
    let mut peers = names.clone().map(|name| Peer::spawn("publish", &name));
    for peer in &peers {
        assert_eq!(peer.says(), "ready");
    }
    let policy = SubscriberPolicy {
        wake: Wake::Fd,
        ..SubscriberPolicy::default()
    };
    let [mut first, mut second] = names.map(|name| {
        let topic = hub::check_service(&name).expect("the publisher's binder");
        Subscriber::<Bytes>::request(&topic, &policy).unwrap()
    });
    for round in 0..200 {
        let from = round % 2;
        peers[from].tell("publish 1");
        let got = loop {
            assert!(wait_any(&mut [&mut first, &mut second], Some(WAIT)).unwrap());
            match (first.try_recv(), second.try_recv()) {
                (Ok(value), Err(RecvError::Empty)) => break (0, value),
                (Err(RecvError::Empty), Ok(value)) => break (1, value),
                (Err(RecvError::Empty), Err(RecvError::Empty)) => {}
                other => panic!("round {round}: {other:?}"),
            }
        };
        assert_eq!(got, (from, Bytes(vec![0])), "round {round}");
        assert_eq!(peers[from].says(), "published");
    }
    let killed = Instant::now();
    peers[0].kill();
    loop {
        assert!(wait_any(&mut [&mut first, &mut second], Some(WAIT)).unwrap());
        match first.try_recv() {
            Err(RecvError::Empty) => {}
            Err(RecvError::Closed(Closed::PublisherDied)) => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(
        killed.elapsed() < Duration::from_secs(1),
        "{:?}",
        killed.elapsed()
    );
    peers[1].tell("publish 1");
    assert!(wait_any(&mut [&mut second], Some(WAIT)).unwrap());
    assert_eq!(second.try_recv(), Ok(Bytes(vec![0])));
}

/// AC-15.2.41: receives after dropped `recv_async`s get each record, then `PublisherDied`.
#[cfg(feature = "tokio")]
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_receive_after_a_dropped_async_receive_learns_that_the_publisher_died() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let name = name("joined");
    let mut peer = Peer::spawn("publish", &name);
    assert_eq!(peer.says(), "ready");
    let topic = hub::check_service(&name).expect("the publisher's binder");
    let mut subscriber =
        Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default()).unwrap();
    let abandon = |subscriber: &mut Subscriber<Bytes>| {
        let _enter = runtime.enter();
        let mut future = std::pin::pin!(subscriber.recv_async());
        let polled = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(polled, Poll::Pending), "nothing to receive yet");
    };
    for round in 0..100 {
        abandon(&mut subscriber);
        peer.tell("publish 1");
        let got = subscriber.recv_timeout(WAIT);
        assert_eq!(got, Ok(Bytes(vec![0])), "round {round}");
        assert_eq!(peer.says(), "published");
    }
    abandon(&mut subscriber);
    let killing = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        peer.kill();
    });
    let started = Instant::now();
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Closed(Closed::PublisherDied))
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    killing.join().unwrap();
    drop(subscriber);
    runtime.shutdown_timeout(Duration::from_secs(1));
}

/// A topic of the smallest ring that waits up to `timeout`, registered with the hub.
fn waiting(tag: &str, timeout: Duration) -> (String, Publisher<Bytes>) {
    init();
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy {
        ring_bytes: 4096,
        backpressure: Some(Backpressure {
            timeout,
            ..Backpressure::default()
        }),
        ..PublisherPolicy::default()
    })
    .unwrap();
    let name = name(tag);
    hub::add_service(&name, publisher.as_binder()).unwrap();
    (name, publisher)
}

/// 64 bytes that carry `n`.
fn numbered(n: u32) -> Bytes {
    let mut bytes = vec![0; 64];
    bytes[..4].copy_from_slice(&n.to_le_bytes());
    Bytes(bytes)
}

/// Publish two rings' worth of numbered values; the longest a publish took.
fn overrun(publisher: &Publisher<Bytes>) -> Duration {
    (0..2 * 4096 / 72)
        .map(|n| {
            let began = Instant::now();
            publisher.publish(&numbered(n)).unwrap();
            began.elapsed()
        })
        .max()
        .unwrap()
}

/// AC-15.3.9: a subscriber stopped past the timeout is taken out and told; one keeping up isn't.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_subscriber_stopped_past_the_timeout_is_taken_out_and_told() {
    let timeout = Duration::from_millis(300);
    let (name, publisher) = waiting("stall", timeout);
    let mut peer = Peer::spawn("stall", &name);
    assert_eq!(peer.says(), "subscribed");
    let mut keeper =
        Subscriber::<Bytes>::request(&publisher.as_binder(), &SubscriberPolicy::default()).unwrap();
    signal(&peer, rustix::process::Signal::STOP);
    let longest = std::thread::scope(|scope| {
        let receiving = scope.spawn(move || {
            (0..2 * 4096 / 72)
                .map(|_| keeper.recv_timeout(WAIT).unwrap())
                .collect::<Vec<_>>()
        });
        let longest = overrun(&publisher);
        let got = receiving.join().unwrap();
        assert_eq!(got, (0..2 * 4096 / 72).map(numbered).collect::<Vec<_>>());
        longest
    });
    assert!(longest >= timeout, "{longest:?}");
    assert!(longest < timeout + Duration::from_secs(1), "{longest:?}");
    assert_eq!(publisher.stats().evicted, 1);
    signal(&peer, rustix::process::Signal::CONT);
    peer.tell("receive");
    assert_eq!(peer.says(), "closed Evicted");
}

/// AC-15.3.11: a publish waiting for a killed subscriber ends at its death, before the timeout.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_wait_for_a_subscriber_that_dies_ends_with_its_death() {
    let timeout = Duration::from_secs(10);
    let (name, publisher) = waiting("die", timeout);
    let mut peer = Peer::spawn("stall", &name);
    assert_eq!(peer.says(), "subscribed");
    let began = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            peer.kill();
        });
        overrun(&publisher);
    });
    let took = began.elapsed();
    assert!(took >= Duration::from_millis(300), "none waited: {took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert_eq!(publisher.stats().evicted, 0);
}

/// The other process's side of the tests above.
#[test]
#[ignore = "run by the tests above as their other process"]
fn child() {
    let Some(role) = std::env::var_os(ROLE) else {
        return;
    };
    init();
    let name = std::env::var(NAME).unwrap();
    let say = |line: &str| println!("topic: {line}");
    let wait_for_eof = || {
        let mut line = String::new();
        while std::io::stdin()
            .read_line(&mut line)
            .is_ok_and(|read| read != 0)
        {
            line.clear();
        }
    };
    match role.to_str().unwrap() {
        "subscribe" => {
            let topic = hub::check_service(&name).unwrap();
            let subscriber =
                Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default()).unwrap();
            say("subscribed");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
            assert_eq!(line.trim(), "drop");
            drop(subscriber);
            say("dropped");
            wait_for_eof();
        }
        "pin" => {
            let topic = hub::check_service(&name).unwrap();
            let mut subscriber =
                Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default()).unwrap();
            say("subscribed");
            let sample = subscriber.recv_sample().unwrap();
            let value = String::from_utf8(sample.to_owned()).unwrap();
            say(&format!("pinned {value}"));
            // Held until the process is killed.
            wait_for_eof();
            drop(sample);
        }
        "stall" => {
            let topic = hub::check_service(&name).unwrap();
            let policy = SubscriberPolicy::default();
            let mut subscriber = Subscriber::<Bytes>::request(&topic, &policy).unwrap();
            say("subscribed");
            // Reads nothing until told, then receives until the subscription ends.
            let mut told = String::new();
            std::io::stdin().read_line(&mut told).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match subscriber.recv_timeout(Duration::from_millis(100)) {
                    Err(RecvError::Closed(closed)) => {
                        say(&format!("closed {closed:?}"));
                        break;
                    }
                    _ if Instant::now() > deadline => {
                        say("not closed");
                        break;
                    }
                    _ => {}
                }
            }
            wait_for_eof();
        }
        "subscribe-loop" => {
            let topic = hub::check_service(&name).unwrap();
            let policy = SubscriberPolicy::default();
            drop(Subscriber::<Bytes>::request(&topic, &policy).unwrap());
            say("looping");
            loop {
                drop(Subscriber::<Bytes>::request(&topic, &policy));
            }
        }
        "refuse" => {
            let topic = hub::check_service(&name).unwrap();
            let service = topic.clone().into_interface::<dyn ITopic>().unwrap();
            let token = || rsbinder::Binder::new(Inert).as_binder();
            let mut request = raw_request(token());
            request.version = 99;
            say(&format!(
                "version: {}",
                verdict(service.r#subscribe(&request))
            ));
            let other = Subscriber::<Other>::request(&topic, &SubscriberPolicy::default());
            say(&format!("type: {}", verdict(other)));
            let request = raw_request(topic.clone());
            say(&format!(
                "foreign token: {}",
                verdict(service.r#subscribe(&request))
            ));
            say("done");
            wait_for_eof();
        }
        "nofile" => {
            let topic = hub::check_service(&name).unwrap();
            // Two free fds: the control page and the parcel's copy; the reply's needs a third.
            let lowest = File::open("/dev/null").unwrap().as_raw_fd();
            let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
            let lowered = rustix::process::Rlimit {
                current: Some(lowest as u64 + 2),
                maximum: limit.maximum,
            };
            rustix::process::setrlimit(rustix::process::Resource::Nofile, lowered).unwrap();
            let refused = Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default());
            say(&format!("failed: {}", verdict(refused)));
            wait_for_eof();
        }
        "small-ring" => {
            let topic = hub::check_service(&name).unwrap();
            let policy = SubscriberPolicy {
                max_ring_bytes: 4096,
                ..SubscriberPolicy::default()
            };
            let refused = Subscriber::<Bytes>::request(&topic, &policy);
            say(&format!("refused: {}", verdict(refused)));
            wait_for_eof();
        }
        role @ ("publish" | "advertise") => {
            let policy = PublisherPolicy::default();
            let publisher = if role == "advertise" {
                Publisher::<Bytes>::advertise(&name, &policy).unwrap()
            } else {
                let publisher = Publisher::<Bytes>::new(&policy).unwrap();
                hub::add_service(&name, publisher.as_binder()).unwrap();
                publisher
            };
            say("ready");
            // `publish N` publishes the bytes 0 to N - 1, as many times as told, until EOF.
            for line in std::io::stdin().lines().map_while(Result::ok) {
                let count: u8 = line
                    .trim()
                    .strip_prefix("publish ")
                    .unwrap()
                    .parse()
                    .unwrap();
                for byte in 0..count {
                    publisher.publish(&Bytes(vec![byte])).unwrap();
                }
                say("published");
            }
        }
        other => panic!("no role {other}"),
    }
}

/// A refusal as one word: an `ITopic` code, or the status code.
fn verdict<T>(result: rsbinder::BinderResult<T>) -> String {
    match result {
        Ok(_) => "accepted".to_owned(),
        Err(status) if status.exception_code() == ExceptionCode::ServiceSpecific => {
            format!("specific {}", status.service_specific_error())
        }
        Err(status) => format!("{:?}", status.transaction_error()),
    }
}

/// The crate's layout version (`layout.rs`), which a test outside it cannot name.
const LAYOUT: u32 = 3;

/// A request as `Subscriber::request` makes it for `Bytes`, with `token` given.
fn raw_request(token: SIBinder) -> SubscribeRequest {
    SubscribeRequest {
        version: LAYOUT as i32,
        r#type: Bytes::type_name().to_owned(),
        token: Some(token),
        control: Some(ParcelFileDescriptor::from(control_page())),
        pins: 0,
        wake: 0,
        wakeFd: None,
    }
}

/// A control page of layout 0.2 with no pin words, as a subscriber makes it.
fn control_page() -> OwnedFd {
    let options = CreateOptions::new("rsbinder-topic-test").populate(true);
    let file = File::from(shm::create(0x80, &options).unwrap());
    file.write_at(b"RSTC", 0).unwrap();
    file.write_at(&LAYOUT.to_ne_bytes(), 4).unwrap();
    let fd = OwnedFd::from(file);
    let seals = Seals::GROW | Seals::SHRINK | Seals::FUTURE_WRITE | Seals::SEAL;
    shm::add_seals(fd.as_fd(), seals).unwrap();
    fd
}

/// A payload of another name.
#[derive(Debug)]
struct Other;

impl Payload for Other {
    fn type_name() -> &'static str {
        "other"
    }

    fn encode(&self, _: &mut rsbinder_topic::Encoding<'_>) -> rsbinder::Result<()> {
        Ok(())
    }

    fn decode(_: &mut rsbinder_topic::Record<'_>) -> rsbinder::Result<Self> {
        Ok(Self)
    }
}

/// A binder of the subscriber's process that answers nothing.
struct Inert;

impl rsbinder::Remotable for Inert {
    fn descriptor() -> &'static str {
        "rsbinder.topic.test.Inert"
    }

    fn on_transact(
        &self,
        _: rsbinder::TransactionCode,
        _: &mut rsbinder::Parcel,
        _: &mut rsbinder::Parcel,
    ) -> rsbinder::Result<()> {
        Err(StatusCode::UnknownTransaction)
    }

    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> rsbinder::Result<()> {
        Ok(())
    }
}
