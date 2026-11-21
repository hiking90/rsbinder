// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A topic's status from outside its publisher: `ITopic.status`, the binder dump that prints it,
//! and who may ask ([`PublisherBuilder::inspect`](crate::PublisherBuilder::inspect)). Within one
//! process; another process asks in `tests/two_process.rs`.

use std::time::Duration;

use rsbinder::{ExceptionCode, Strong};

use crate::aidl::ITopic::{ITopic, ERROR_CLOSED, TOPIC_EVENT};
use crate::aidl::SubscriberStatus::SubscriberStatus;
use crate::aidl::TopicStatus::TopicStatus;
use crate::publish::{may_inspect, SHELL_UID};
use crate::{Bytes, Publisher, Start, Subscriber};

fn subscribe(publisher: &Publisher<Bytes>) -> Subscriber<Bytes> {
    Subscriber::<Bytes>::builder()
        .start_at(Start::Next)
        .subscribe(&publisher.as_binder())
        .unwrap()
}

#[test]
fn the_status_is_what_the_publishers_stats_read_with_the_topics_shape() {
    let publisher = Publisher::<Bytes>::events()
        .lossless(Duration::from_secs(1))
        .build()
        .unwrap();
    let mut first = subscribe(&publisher);
    let _second = Subscriber::<Bytes>::builder()
        .unwaited()
        .subscribe(&publisher.as_binder())
        .unwrap();
    publisher.publish(&Bytes(vec![1; 10])).unwrap();
    first.try_recv().unwrap();
    // Through the publisher's own binder, as a caller in another process makes the call.
    let topic: Strong<dyn ITopic> = publisher.as_binder().into_interface().unwrap();
    let status = topic.r#status().unwrap();
    let stats = publisher.stats();
    assert_eq!(status.typeName, "bytes");
    assert_eq!(status.topicKind, TOPIC_EVENT);
    assert_eq!(status.ringBytes as usize, publisher.ring_bytes());
    assert!(status.lossless);
    assert_eq!((status.published, status.bytes, status.evicted), (1, 10, 0));
    let listed: Vec<_> = stats
        .subscribers
        .iter()
        .map(|info| (info.id as i64, info.lag_bytes as i64, info.unwaited))
        .collect();
    let sent: Vec<_> = status
        .subscribers
        .iter()
        .map(|one| (one.id, one.lagBytes, one.unwaited))
        .collect();
    assert_eq!(sent, listed);
    assert_eq!(status.subscribers[0].pinned, -1);
}

#[test]
fn a_caller_inspect_refuses_is_refused_the_status_and_told_so_in_the_dump() {
    let publisher = Publisher::<Bytes>::events()
        .inspect(|_| false)
        .build()
        .unwrap();
    let refused = publisher.acceptor().status();
    assert_eq!(
        refused.err().map(|status| status.exception_code()),
        Some(ExceptionCode::Security)
    );
    let mut text = Vec::new();
    publisher.acceptor().dump(&mut text, &[]).unwrap();
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("may not look at this topic"), "{text}");
}

#[test]
fn a_dropped_publisher_answers_closed_and_its_dump_says_so() {
    let publisher = Publisher::<Bytes>::events().build().unwrap();
    let acceptor = publisher.acceptor();
    drop(publisher);
    let closed = acceptor.status();
    assert_eq!(
        closed.err().map(|status| status.service_specific_error()),
        Some(ERROR_CLOSED)
    );
    let mut text = Vec::new();
    acceptor.dump(&mut text, &[]).unwrap();
    assert_eq!(text, b"rsbinder-topic: the publisher has been dropped\n");
    assert_eq!(dumped_to_an_fd(&acceptor), text);
}

/// What `dump_fd` writes, read to the end-of-file its writer thread leaves.
fn dumped_to_an_fd(acceptor: &crate::Acceptor) -> Vec<u8> {
    use std::io::Read;
    let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
    acceptor.dump_fd(writer.into(), &[]).unwrap();
    let mut text = Vec::new();
    { reader }.read_to_end(&mut text).unwrap();
    text
}

#[test]
fn a_dump_to_an_fd_is_the_dump_to_a_writer() {
    let publisher = Publisher::<Bytes>::events().build().unwrap();
    let _subscriber = Subscriber::<Bytes>::builder()
        .subscribe(&publisher.as_binder())
        .unwrap();
    let acceptor = publisher.acceptor();
    let mut text = Vec::new();
    acceptor.dump(&mut text, &[]).unwrap();
    assert_eq!(text.iter().filter(|byte| **byte == b'\n').count(), 2);
    assert_eq!(dumped_to_an_fd(&acceptor), text);
}

/// The slot is the topic's: a dump one acceptor writes refuses the next at another.
#[test]
fn a_topic_writes_one_dump_at_a_time_across_its_acceptors() {
    use std::io::Write;
    let publisher = Publisher::<Bytes>::events().build().unwrap();
    // Full before the dump: its writer waits whatever the size of the text.
    let (_reader, mut full) = std::os::unix::net::UnixStream::pair().unwrap();
    full.set_nonblocking(true).unwrap();
    while full.write(&[0; 4096]).is_ok() {}
    full.set_nonblocking(false).unwrap();
    assert_eq!(publisher.acceptor().dump_fd(full.into(), &[]), Ok(()));
    let (_other, writer) = std::os::unix::net::UnixStream::pair().unwrap();
    assert_eq!(
        publisher.acceptor().dump_fd(writer.into(), &[]),
        Err(rsbinder::StatusCode::WouldBlock)
    );
}

#[test]
fn by_default_the_publishers_uid_and_root_may_look_and_shell_on_android() {
    assert!(may_inspect(1000, 1000, false));
    assert!(may_inspect(0, 1000, false));
    assert!(!may_inspect(1001, 1000, false));
    assert!(!may_inspect(SHELL_UID, 1000, false));
    assert!(may_inspect(SHELL_UID, 1000, true));
    assert!(!may_inspect(1001, 1000, true));
}

#[test]
fn the_dump_is_a_line_for_the_topic_and_one_per_subscription() {
    let status = TopicStatus {
        typeName: "bytes".to_owned(),
        topicKind: TOPIC_EVENT,
        ringBytes: 4096,
        lossless: false,
        published: 3,
        bytes: 30,
        evicted: 1,
        subscribers: vec![
            SubscriberStatus {
                id: 1,
                uid: 1000,
                pid: 42,
                lagBytes: 16,
                lost: 2,
                waiting: true,
                evicted: true,
                unwaited: false,
                pinned: -1,
            },
            SubscriberStatus {
                id: 2,
                uid: 0,
                pid: 43,
                unwaited: true,
                pinned: 5,
                ..Default::default()
            },
        ],
    };
    assert_eq!(
        status.to_string(),
        "bytes event topic: ring 4096 bytes, lossy, published 3 (30 bytes), evicted 1, \
         2 subscribers\n\
         \x20 #1 uid 1000 pid 42: lag 16 bytes, lost 2, waiting, evicted\n\
         \x20 #2 uid 0 pid 43: lag 0 bytes, lost 0, unwaited, pins slot 5\n"
    );
}

#[test]
fn the_publishers_dump_prints_its_status() {
    let publisher = Publisher::<Bytes>::events().build().unwrap();
    let _subscriber = subscribe(&publisher);
    let mut text = Vec::new();
    publisher.acceptor().dump(&mut text, &[]).unwrap();
    let expected = publisher.acceptor().status().unwrap().to_string();
    assert_eq!(String::from_utf8(text).unwrap(), expected);
    assert_eq!(expected.lines().count(), 2);
}
