// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A publisher and a subscriber in two processes over a Unix domain socket, the scenarios of
//! `two_process.rs` that say what a transport has to do. They need no binder device: nothing here
//! opens one, and a container without one runs them (plan 15-pubsub-uds §7).
//!
//! The other process is this test binary again, running `child` with `RSBINDER_TOPIC_ROLE` set.

#![cfg(all(feature = "uds", any(target_os = "linux", target_os = "android")))]

mod common;

use std::io::BufRead;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use common::{eventually, say, wait_for_eof, Peer, NAME, ROLE, WAIT};
use rsbinder::rpc::{PeerIdentity, RpcClientConfig, RpcServer, RpcServerHandle, RpcSession};
use rsbinder::StatusCode;
use rsbinder_topic::uds::{self, Endpoint, Options};
use rsbinder_topic::{
    wait_any, Backpressure, Bytes, Closed, Publisher, PublisherPolicy, RecvError, Subscriber,
    SubscriberPolicy, Wait, Wake,
};

fn path(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rsb_uds2_{tag}_{pid}_{serial}.sock"))
}

/// Run test `name` again in a process of its own, where no other test makes or drops a memfd;
/// `true` in that process.
fn alone(name: &str) -> bool {
    const ALONE: &str = "RSBINDER_TOPIC_ALONE";
    if std::env::var_os(ALONE).is_some() {
        return true;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--test-threads=1", "--nocapture"])
        .env(ALONE, "1")
        .status()
        .unwrap();
    assert!(status.success(), "{name} in a process of its own");
    false
}

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

/// `publisher` served at a new path.
fn serve(publisher: &Publisher<Bytes>, tag: &str) -> (PathBuf, RpcServerHandle) {
    let path = path(tag);
    let server = RpcServer::setup_unix_server(&path).unwrap();
    (
        path.clone(),
        uds::serve(&server, publisher.as_binder()).unwrap(),
    )
}

fn publish(tag: &str) -> (PathBuf, Publisher<Bytes>, RpcServerHandle) {
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).unwrap();
    let (path, handle) = serve(&publisher, tag);
    (path, publisher, handle)
}

fn to_str(path: &std::path::Path) -> &str {
    path.to_str().unwrap()
}

fn subscribe(path: &std::path::Path) -> Subscriber<Bytes> {
    let endpoint = Endpoint::Path(path.to_owned());
    uds::subscribe(endpoint, &SubscriberPolicy::default()).unwrap()
}

/// Receive until the topic ends: the values, and how it ended.
fn drain(subscriber: &mut Subscriber<Bytes>) -> (Vec<Bytes>, Closed) {
    let mut values = Vec::new();
    loop {
        match subscriber.recv_timeout(WAIT) {
            Ok(value) => values.push(value),
            Err(RecvError::Closed(closed)) => return (values, closed),
            Err(other) => panic!("{other}"),
        }
    }
}

#[test]
fn a_killed_or_dropped_subscriber_is_taken_out_and_its_page_unmapped() {
    if !alone("a_killed_or_dropped_subscriber_is_taken_out_and_its_page_unmapped") {
        return;
    }
    let (path, publisher, _handle) = publish("gone");
    let before = memfds();
    let mut peer = Peer::spawn("subscribe", to_str(&path));
    assert_eq!(peer.says(), "subscribed");
    assert_eq!(publisher.subscriber_count(), 1);
    assert_eq!(memfds(), (before.0, before.1 + 1));
    peer.kill();
    eventually("the session's end to unmap the page", || memfds() == before);
    assert_eq!(publisher.subscriber_count(), 0);

    let mut peer = Peer::spawn("subscribe", to_str(&path));
    assert_eq!(peer.says(), "subscribed");
    assert_eq!(publisher.subscriber_count(), 1);
    peer.tell("drop");
    assert_eq!(peer.says(), "dropped");
    assert_eq!(publisher.subscriber_count(), 0);
    eventually("the session's end to unmap the page", || memfds() == before);
}

#[test]
fn a_publishers_stats_name_a_subscriber_of_another_process() {
    let (path, publisher, _handle) = publish("stats");
    let peer = Peer::spawn("subscribe", to_str(&path));
    assert_eq!(peer.says(), "subscribed");
    publisher.publish(&Bytes(vec![7; 9])).unwrap();
    let stats = publisher.stats();
    let [info] = stats.subscribers.as_slice() else {
        panic!("{stats:?}");
    };
    assert_eq!(info.pid, peer.child.id() as i32);
    assert_eq!(info.uid, rustix::process::geteuid().as_raw());
}

#[test]
fn subscribers_killed_while_they_subscribe_leave_nothing() {
    if !alone("subscribers_killed_while_they_subscribe_leave_nothing") {
        return;
    }
    let (path, publisher, _handle) = publish("killed");
    let before = memfds();
    let kills: u32 = std::env::var("RSBINDER_TOPIC_KILLS")
        .ok()
        .and_then(|kills| kills.parse().ok())
        .unwrap_or(100);
    let mut seed = u64::from(std::process::id()) | 1;
    for _ in 0..kills {
        let mut peer = Peer::spawn("subscribe-loop", to_str(&path));
        assert_eq!(peer.says(), "looping");
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        std::thread::sleep(Duration::from_micros(seed % 2000));
        peer.kill();
        publisher.publish(&Bytes(Vec::new())).unwrap();
    }
    eventually("every page unmapped", || {
        publisher.publish(&Bytes(Vec::new())).unwrap();
        memfds() == before
    });
    assert_eq!(publisher.subscriber_count(), 0);
}

#[test]
fn a_subscriber_reads_what_a_dead_publisher_committed_then_learns_it_died() {
    let path = path("died");
    let mut peer = Peer::spawn("publish", to_str(&path));
    assert_eq!(peer.says(), "ready");
    let mut subscriber = subscribe(&path);
    peer.tell("publish 3");
    assert_eq!(peer.says(), "published");
    peer.kill();
    let (values, closed) = drain(&mut subscriber);
    assert_eq!(closed, Closed::PublisherDied);
    let expected: Vec<_> = (0..3).map(|byte| Bytes(vec![byte])).collect();
    assert_eq!(values, expected);
}

/// A process that exits right behind the end record: the record wins over the session's end.
#[test]
fn a_publisher_that_closes_and_exits_ends_with_its_end_record() {
    let path = path("exit");
    let mut peer = Peer::spawn("publish", to_str(&path));
    assert_eq!(peer.says(), "ready");
    let mut subscriber = subscribe(&path);
    peer.tell("publish 2");
    assert_eq!(peer.says(), "published");
    peer.tell("exit");
    let (values, closed) = drain(&mut subscriber);
    assert_eq!(closed, Closed::End);
    assert_eq!(values.len(), 2);
    let status = peer.child.wait().unwrap();
    assert!(status.success(), "{status}");
    assert!(
        uds::subscribe::<Bytes>(Endpoint::Path(path), &SubscriberPolicy::default()).is_err(),
        "the path of a publisher that exited takes no connection"
    );
}

fn sleepers_learn_within_a_second(wake: Wake) {
    let path = path("sleep");
    let mut peer = Peer::spawn("publish", to_str(&path));
    assert_eq!(peer.says(), "ready");
    let policy = SubscriberPolicy {
        wake,
        ..SubscriberPolicy::default()
    };
    let endpoint = Endpoint::Path(path);
    let mut subscriber = uds::subscribe::<Bytes>(endpoint, &policy).unwrap();
    let sleeping = std::thread::spawn(move || {
        let ended = subscriber.recv_timeout(WAIT);
        (ended, Instant::now())
    });
    std::thread::sleep(Duration::from_millis(100));
    let killed = Instant::now();
    peer.kill();
    let (ended, at) = sleeping.join().unwrap();
    assert_eq!(ended, Err(RecvError::Closed(Closed::PublisherDied)));
    assert!(at - killed < Duration::from_secs(1), "{:?}", at - killed);
}

#[test]
fn sleeping_subscribers_learn_within_a_second_that_their_publisher_died() {
    sleepers_learn_within_a_second(Wake::Futex);
}

#[test]
fn sleeping_fd_subscribers_learn_within_a_second_that_their_publisher_died() {
    sleepers_learn_within_a_second(Wake::Fd);
}

#[test]
fn fd_subscribers_wait_on_one_thread_and_learn_of_a_death() {
    let path_a = path("poll_a");
    let path_b = path("poll_b");
    let mut peer_a = Peer::spawn("publish", to_str(&path_a));
    let peer_b = Peer::spawn("publish", to_str(&path_b));
    assert_eq!(peer_a.says(), "ready");
    assert_eq!(peer_b.says(), "ready");
    let policy = SubscriberPolicy {
        wake: Wake::Fd,
        ..SubscriberPolicy::default()
    };
    let mut a = uds::subscribe::<Bytes>(Endpoint::Path(path_a), &policy).unwrap();
    let mut b = uds::subscribe::<Bytes>(Endpoint::Path(path_b), &policy).unwrap();
    peer_a.kill();
    assert!(wait_any(&mut [&mut a as &mut dyn Wait, &mut b], Some(WAIT)).unwrap());
    assert_eq!(a.try_recv(), Err(RecvError::Closed(Closed::PublisherDied)));
    assert_eq!(b.try_recv(), Err(RecvError::Empty));
}

/// A stopped publisher with room in its accept queue: the handshake finds it out.
#[test]
fn a_stopped_publisher_is_given_up_on_at_the_timeout() {
    let path = path("stopped");
    let peer = Peer::spawn("publish", to_str(&path));
    assert_eq!(peer.says(), "ready");
    let stopped = peer.stop();
    let options = Options {
        timeout: Some(Duration::from_millis(200)),
        ..Options::default()
    };
    let refused = uds::subscribe_with::<Bytes>(
        Endpoint::Path(path.clone()),
        &SubscriberPolicy::default(),
        &options,
    );
    assert_eq!(
        refused.err().map(|status| status.transaction_error()),
        Some(StatusCode::TimedOut)
    );
    drop(stopped);
}

/// The session keeps no deadline: a publisher stopped after the setup is not its death.
#[test]
fn a_publisher_stopped_after_the_setup_is_not_a_death() {
    let path = path("paused");
    let peer = Peer::spawn("publish", to_str(&path));
    assert_eq!(peer.says(), "ready");
    let options = Options {
        timeout: Some(Duration::from_millis(100)),
        ..Options::default()
    };
    let mut subscriber =
        uds::subscribe_with::<Bytes>(Endpoint::Path(path), &SubscriberPolicy::default(), &options)
            .unwrap();
    let stopped = peer.stop();
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(400)),
        Err(RecvError::Timeout)
    );
    drop(stopped);
    drop(peer);
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Closed(Closed::PublisherDied))
    );
}

#[test]
fn a_subscriber_stopped_past_the_timeout_is_taken_out_and_told() {
    let policy = PublisherPolicy {
        backpressure: Some(Backpressure {
            publish_timeout: Duration::from_millis(100),
            ..Backpressure::default()
        }),
        ring_bytes: 4096,
        ..PublisherPolicy::default()
    };
    let publisher = Publisher::<Bytes>::new(&policy).unwrap();
    let (path, _handle) = serve(&publisher, "evict");
    let mut peer = Peer::spawn("subscribe-idle", to_str(&path));
    assert_eq!(peer.says(), "subscribed");
    let value = Bytes(vec![0; 256]);
    while !publisher.stats().subscribers.iter().any(|sub| sub.evicted) {
        publisher.publish(&value).unwrap();
    }
    peer.tell("receive");
    assert_eq!(peer.says(), "evicted");
    peer.tell("drop");
    assert_eq!(peer.says(), "dropped");
    eventually("the subscription's end", || {
        publisher.subscriber_count() == 0
    });
}

/// `SO_PEERCRED` of a socket the subscriber connects through a descriptor names who listens.
#[test]
fn a_subscriber_sees_the_process_that_listens() {
    let path = path("peer");
    let peer = Peer::spawn("publish", to_str(&path));
    assert_eq!(peer.says(), "ready");
    let flags = rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC;
    let address: OwnedFd = rustix::fs::open(&path, flags, rustix::fs::Mode::empty()).unwrap();
    std::fs::remove_file(&path).unwrap();
    let config = RpcClientConfig::unix_fd(address, 2).incoming_connections(1);
    let session = RpcSession::setup_client_android13plus_with_config(config).unwrap();
    let PeerIdentity::Local { uid, pid, .. } = session.peer_identity() else {
        panic!("{:?}", session.peer_identity());
    };
    assert_eq!(uid, rustix::process::geteuid().as_raw());
    assert_eq!(pid, peer.child.id() as i32);
    session.close_session();
}

/// The other process of a test.
#[test]
#[ignore = "run by the tests of this file, as the other process"]
fn child() {
    let path = PathBuf::from(std::env::var(NAME).unwrap());
    let endpoint = || Endpoint::Path(path.clone());
    let policy = SubscriberPolicy::default();
    let lines = || std::io::stdin().lock().lines().map_while(Result::ok);
    match std::env::var(ROLE).unwrap().as_str() {
        "subscribe" => {
            let subscriber = uds::subscribe::<Bytes>(endpoint(), &policy).unwrap();
            say("subscribed");
            if lines().next().as_deref() == Some("drop") {
                drop(subscriber);
                say("dropped");
            }
            wait_for_eof();
        }
        "subscribe-idle" => {
            let mut subscriber = uds::subscribe::<Bytes>(endpoint(), &policy).unwrap();
            say("subscribed");
            for line in lines() {
                match line.as_str() {
                    "receive" => loop {
                        match subscriber.recv_timeout(WAIT) {
                            Ok(_) | Err(RecvError::Lagged(_)) => {}
                            Err(RecvError::Closed(Closed::Evicted)) => {
                                say("evicted");
                                break;
                            }
                            Err(other) => panic!("{other}"),
                        }
                    },
                    "drop" => {
                        drop(subscriber);
                        say("dropped");
                        wait_for_eof();
                        return;
                    }
                    other => panic!("{other}"),
                }
            }
        }
        "subscribe-loop" => {
            say("looping");
            loop {
                let _ = uds::subscribe::<Bytes>(endpoint(), &policy);
            }
        }
        "publish" => {
            let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).unwrap();
            let server = RpcServer::setup_unix_server(&path).unwrap();
            let handle = uds::serve(&server, publisher.as_binder()).unwrap();
            say("ready");
            for line in lines() {
                let (command, count) = line.split_once(' ').unwrap_or((&line, "0"));
                match command {
                    "publish" => {
                        for byte in 0..count.parse::<u8>().unwrap() {
                            publisher.publish(&Bytes(vec![byte])).unwrap();
                        }
                        say("published");
                    }
                    "exit" => {
                        drop(publisher);
                        // As a process that ends: the socket and every session go with it.
                        std::mem::forget(handle);
                        std::process::exit(0);
                    }
                    other => panic!("{other}"),
                }
            }
        }
        other => panic!("{other}"),
    }
}
