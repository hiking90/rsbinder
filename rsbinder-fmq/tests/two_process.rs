// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Two processes on one queue, the fd carried over a unix socket
//! (`SCM_RIGHTS`). This is where a wrong futex flag shows: with
//! `FUTEX_PRIVATE_FLAG` every in-process test passes and the peer here
//! never wakes — so every wait below has a deadline and a hung child is
//! killed and reported.
//!
//! The child is this test binary re-run with `RSBINDER_FMQ_CHILD` set,
//! filtered to the same test function.

#![cfg(any(target_os = "linux", target_os = "android"))]

mod common;

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use rsbinder_fmq::{AttachPolicy, MessageQueue, NOT_EMPTY, NOT_FULL};

const CHILD_ENV: &str = "RSBINDER_FMQ_CHILD";
const SOCK_ENV: &str = "RSBINDER_FMQ_SOCK";
const ITEMS: u32 = 5000;
const CAPACITY: usize = 64;
const WAIT: Option<Duration> = Some(Duration::from_secs(10));

fn policy() -> AttachPolicy {
    AttachPolicy {
        max_capacity: 1 << 16,
        require_seal: true,
        require_event_flag: true,
    }
}

fn spawn_child(test_name: &str, sock: &PathBuf) -> Child {
    let exe = std::env::current_exe().expect("current_exe");
    common::spawn(
        Command::new(exe)
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .env(SOCK_ENV, sock),
    )
}

fn is_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

fn child_sock() -> std::os::unix::net::UnixStream {
    let path: PathBuf = std::env::var_os(SOCK_ENV).unwrap().into();
    common::connect_with_retry(&path)
}

fn expected_sum() -> u64 {
    (0..ITEMS).map(u64::from).sum()
}

/// Parent writes, child reads; the ring is far smaller than the stream, so both sides block.
#[test]
fn parent_creates_child_reads() {
    if is_child() {
        let sock = child_sock();
        let desc = common::recv_descriptor(&sock);
        let mut queue = MessageQueue::<u32>::attach(&desc, &policy()).expect("attach");
        assert_eq!(queue.capacity(), CAPACITY);
        let mut sum = 0u64;
        let mut next = 0u32;
        let mut buf = [0u32; 7];
        while next < ITEMS {
            let n = buf.len().min((ITEMS - next) as usize);
            queue
                .read_blocking(&mut buf[..n], NOT_EMPTY, NOT_FULL, WAIT)
                .expect("read_blocking");
            for &v in &buf[..n] {
                assert_eq!(v, next);
                sum += u64::from(v);
                next += 1;
            }
        }
        println!("sum={sum}");
        return;
    }

    let path = common::sock_path("pc");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    let child = spawn_child("parent_creates_child_reads", &path);
    let (sock, _) = listener.accept().expect("accept");

    let mut queue = MessageQueue::<u32>::create(CAPACITY, true).expect("create");
    common::send_descriptor(&sock, &queue.descriptor().expect("descriptor"));

    let mut next = 0u32;
    let mut batch = [0u32; 5];
    while next < ITEMS {
        let n = batch.len().min((ITEMS - next) as usize);
        for slot in &mut batch[..n] {
            *slot = next;
            next += 1;
        }
        queue
            .write_blocking(&batch[..n], NOT_FULL, NOT_EMPTY, WAIT)
            .expect("write_blocking");
    }
    let out = common::finish(child);
    assert!(
        out.contains(&format!("sum={}", expected_sum())),
        "child output: {out}"
    );
    let _ = std::fs::remove_file(&path);
}

/// Child allocates and writes; parent attaches, so the seal check sees a foreign memfd.
#[test]
fn child_creates_parent_reads() {
    if is_child() {
        let mut sock = child_sock();
        let mut queue = MessageQueue::<u32>::create(CAPACITY, true).expect("create");
        common::send_descriptor(&sock, &queue.descriptor().expect("descriptor"));
        // Wait for the attach before writing: a libfmq peer's attach resets the counters.
        let mut ack = [0u8; 1];
        sock.read_exact(&mut ack).expect("attach ack");
        for v in 0..ITEMS {
            queue
                .write_blocking(&[v], NOT_FULL, NOT_EMPTY, WAIT)
                .expect("write_blocking");
        }
        // Keep the mapping alive until the parent has read everything.
        let mut done = [0u8; 1];
        sock.read_exact(&mut done).expect("done ack");
        println!("child done");
        return;
    }

    let path = common::sock_path("cp");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    let child = spawn_child("child_creates_parent_reads", &path);
    let (mut sock, _) = listener.accept().expect("accept");

    let desc = common::recv_descriptor(&sock);
    assert!(desc.has_event_flag());
    assert!(rsbinder_fmq::shm::shrink_sealed(desc.fds[0].as_fd()));
    let mut queue = MessageQueue::<u32>::attach(&desc, &policy()).expect("attach");
    sock.write_all(&[1]).expect("attach ack");

    let mut sum = 0u64;
    let mut buf = [0u32; 3];
    let mut next = 0u32;
    while next < ITEMS {
        let n = buf.len().min((ITEMS - next) as usize);
        queue
            .read_blocking(&mut buf[..n], NOT_EMPTY, NOT_FULL, WAIT)
            .expect("read_blocking");
        for &v in &buf[..n] {
            assert_eq!(v, next);
            sum += u64::from(v);
            next += 1;
        }
    }
    assert_eq!(sum, expected_sum());
    sock.write_all(&[1]).expect("done ack");
    let out = common::finish(child);
    assert!(out.contains("child done"), "child output: {out}");
    let _ = std::fs::remove_file(&path);
}

/// The child parks on `NOT_EMPTY`; the parent wakes it through the `EventFlag` alone, no write.
#[test]
fn wake_crosses_the_process_boundary() {
    if is_child() {
        let mut sock = child_sock();
        let desc = common::recv_descriptor(&sock);
        let queue = MessageQueue::<u8>::attach(&desc, &policy()).expect("attach");
        let flag = queue.event_flag().expect("event flag");
        let started = Instant::now();
        sock.write_all(&[1]).expect("ready");
        let bits = flag.wait(NOT_EMPTY, WAIT).expect("wait");
        println!(
            "woke bits={bits} after_ms={}",
            started.elapsed().as_millis()
        );
        return;
    }

    let path = common::sock_path("wk");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    let child = spawn_child("wake_crosses_the_process_boundary", &path);
    let (mut sock, _) = listener.accept().expect("accept");

    let queue = MessageQueue::<u8>::create(16, true).expect("create");
    common::send_descriptor(&sock, &queue.descriptor().expect("descriptor"));
    let mut ready = [0u8; 1];
    sock.read_exact(&mut ready).expect("ready");
    std::thread::sleep(Duration::from_millis(200));
    queue.event_flag().unwrap().wake(NOT_EMPTY).expect("wake");
    let out = common::finish(child);
    assert!(
        out.contains(&format!("woke bits={NOT_EMPTY}")),
        "child output: {out}"
    );
    let after_ms: u128 = out
        .split("after_ms=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("after_ms");
    assert!(
        after_ms >= 150,
        "child woke before the wake was sent: {out}"
    );
    let _ = std::fs::remove_file(&path);
}
