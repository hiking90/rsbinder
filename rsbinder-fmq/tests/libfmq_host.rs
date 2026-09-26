// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! This crate against AOSP's libfmq (`tests/cpp/libfmq_peer.cpp`, built by
//! `tests/cpp/build_host_peer.sh`), in both directions and both roles.
//! Plan 12-fmq §6.2 S1–S3 on the host; the descriptor travels over a unix
//! socket instead of binder.
//!
//! Ignored unless `RSBINDER_FMQ_LIBFMQ_PEER` names the peer binary:
//!
//! ```text
//! RSBINDER_FMQ_LIBFMQ_PEER=target/libfmq-host/libfmq_peer \
//!     cargo test -p rsbinder-fmq --test libfmq_host -- --ignored
//! ```

#![cfg(any(target_os = "linux", target_os = "android"))]

mod common;

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::Duration;

use rsbinder_fmq::{AttachPolicy, Grantor, MessageQueue, NOT_EMPTY, NOT_FULL};

const COUNT: u32 = 5000;
const CAPACITY: usize = 64;
const WAIT: Option<Duration> = Some(Duration::from_secs(10));

fn peer() -> Command {
    let path = std::env::var_os("RSBINDER_FMQ_LIBFMQ_PEER")
        .expect("RSBINDER_FMQ_LIBFMQ_PEER: build it with tests/cpp/build_host_peer.sh");
    Command::new(path)
}

fn policy() -> AttachPolicy {
    AttachPolicy {
        max_capacity: 1 << 16,
        require_seal: true,
        require_event_flag: true,
    }
}

fn expected_sum() -> u64 {
    (0..COUNT).map(u64::from).sum()
}

fn write_all(q: &mut MessageQueue<u32>) {
    let mut next = 0u32;
    let mut batch = [0u32; 5];
    while next < COUNT {
        let n = batch.len().min((COUNT - next) as usize);
        for slot in &mut batch[..n] {
            *slot = next;
            next += 1;
        }
        q.write_blocking(&batch[..n], NOT_FULL, NOT_EMPTY, WAIT)
            .expect("write_blocking");
    }
}

fn read_all(q: &mut MessageQueue<u32>) {
    let mut next = 0u32;
    let mut buf = [0u32; 7];
    let mut sum = 0u64;
    while next < COUNT {
        let n = buf.len().min((COUNT - next) as usize);
        q.read_blocking(&mut buf[..n], NOT_EMPTY, NOT_FULL, WAIT)
            .expect("read_blocking");
        for &v in &buf[..n] {
            assert_eq!(v, next);
            sum += u64::from(v);
            next += 1;
        }
    }
    assert_eq!(sum, expected_sum());
}

/// S1: rsbinder creates; libfmq reads. Its attach resets the counters, so writes wait for it.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn s1_rsbinder_creates_libfmq_reads() {
    let path = common::sock_path("s1");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let child = common::spawn(
        peer()
            .args(["attach", "read"])
            .arg(&path)
            .args([COUNT.to_string(), CAPACITY.to_string()]),
    );
    let (mut sock, _) = listener.accept().unwrap();

    let mut q = MessageQueue::<u32>::create(CAPACITY, true).unwrap();
    common::send_descriptor(&sock, &q.descriptor().unwrap());
    let mut ack = [0u8];
    sock.read_exact(&mut ack).unwrap();
    write_all(&mut q);

    let out = common::finish(child);
    assert!(out.contains(&format!("sum={}", expected_sum())), "{out}");
    assert!(out.contains("capacity=64 grantors=4"), "{out}");
    let _ = std::fs::remove_file(&path);
}

/// S1, the other role: rsbinder creates and reads; libfmq writes, blocking on `NOT_FULL`.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn s1_rsbinder_creates_libfmq_writes() {
    let path = common::sock_path("s1w");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let child = common::spawn(
        peer()
            .args(["attach", "write"])
            .arg(&path)
            .args([COUNT.to_string(), CAPACITY.to_string()]),
    );
    let (mut sock, _) = listener.accept().unwrap();

    let mut q = MessageQueue::<u32>::create(CAPACITY, true).unwrap();
    common::send_descriptor(&sock, &q.descriptor().unwrap());
    let mut ack = [0u8];
    sock.read_exact(&mut ack).unwrap(); // attached
    sock.write_all(&[1]).unwrap(); // go
    read_all(&mut q);
    sock.write_all(&[1]).unwrap(); // all read

    let out = common::finish(child);
    assert!(out.contains("done"), "{out}");
    let _ = std::fs::remove_file(&path);
}

/// S2: libfmq creates (libcutils memfd, sealed `GROW | SHRINK`) and writes; rsbinder reads.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn s2_libfmq_creates_rsbinder_reads() {
    let path = common::sock_path("s2");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let child = common::spawn(
        peer()
            .args(["create", "write"])
            .arg(&path)
            .args([COUNT.to_string(), CAPACITY.to_string()]),
    );
    let (mut sock, _) = listener.accept().unwrap();

    let desc = common::recv_descriptor(&sock);
    let mine = MessageQueue::<u32>::create(CAPACITY, true)
        .unwrap()
        .descriptor()
        .unwrap();
    assert_eq!(
        desc.grantors, mine.grantors,
        "libfmq layout differs from ours"
    );
    assert_eq!(desc.quantum, 4);
    assert_eq!(desc.fds.len(), 1);
    assert!(rsbinder_fmq::shm::shrink_sealed(desc.fds[0].as_fd()));
    let mut ack = [0u8];
    sock.read_exact(&mut ack).unwrap(); // created

    let mut q = MessageQueue::<u32>::attach(&desc, &policy()).unwrap();
    sock.write_all(&[1]).unwrap(); // attached; write
    read_all(&mut q);
    sock.write_all(&[1]).unwrap(); // all read

    let out = common::finish(child);
    assert!(out.contains("done"), "{out}");
    let _ = std::fs::remove_file(&path);
}

/// S2, the other role: libfmq creates and reads; rsbinder attaches and writes.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn s2_libfmq_creates_rsbinder_writes() {
    let path = common::sock_path("s2w");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let child = common::spawn(
        peer()
            .args(["create", "read"])
            .arg(&path)
            .args([COUNT.to_string(), CAPACITY.to_string()]),
    );
    let (mut sock, _) = listener.accept().unwrap();

    let desc = common::recv_descriptor(&sock);
    let mut ack = [0u8];
    sock.read_exact(&mut ack).unwrap(); // created
    let mut q = MessageQueue::<u32>::attach(&desc, &policy()).unwrap();
    write_all(&mut q);

    let out = common::finish(child);
    assert!(out.contains(&format!("sum={}", expected_sum())), "{out}");
    let _ = std::fs::remove_file(&path);
}

/// Pins the single-fd grantor layout, so a libfmq layout change shows up as a diff, not a hang.
#[test]
fn expected_single_fd_layout() {
    let d = MessageQueue::<u32>::create(64, true)
        .unwrap()
        .descriptor()
        .unwrap();
    assert_eq!(
        d.grantors,
        vec![
            Grantor {
                fd_index: 0,
                offset: 0,
                extent: 8
            },
            Grantor {
                fd_index: 0,
                offset: 8,
                extent: 8
            },
            Grantor {
                fd_index: 0,
                offset: 16,
                extent: 256
            },
            Grantor {
                fd_index: 0,
                offset: 272,
                extent: 4
            },
        ]
    );
}
