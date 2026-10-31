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

// Without 64-bit atomics a queue is `Unsupported` (`MessageQueue::create`).
#![cfg(all(
    any(target_os = "linux", target_os = "android"),
    target_has_atomic = "64"
))]

mod common;

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use rsbinder_fmq::{AttachPolicy, MessageQueue, NOT_EMPTY};

const CHILD_ENV: &str = "RSBINDER_FMQ_CHILD";
const SOCK_ENV: &str = "RSBINDER_FMQ_SOCK";
const ITEMS: u32 = 5000;
const CAPACITY: usize = 64;
const WAIT: Option<Duration> = Some(Duration::from_secs(10));

fn policy() -> AttachPolicy {
    AttachPolicy::new(1 << 16).require_event_flag(true)
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
                .read_blocking(&mut buf[..n], WAIT)
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
    let mut child = spawn_child("parent_creates_child_reads", &path);
    let sock = common::accept_from(&listener, &mut child);

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
            .write_blocking(&batch[..n], WAIT)
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
            queue.write_blocking(&[v], WAIT).expect("write_blocking");
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
    let mut child = spawn_child("child_creates_parent_reads", &path);
    let mut sock = common::accept_from(&listener, &mut child);

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
            .read_blocking(&mut buf[..n], WAIT)
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
    let mut child = spawn_child("wake_crosses_the_process_boundary", &path);
    let mut sock = common::accept_from(&listener, &mut child);

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

/// The child maps the parent's memfd read-only, sleeps on one `RoWord32` and wakes another.
#[test]
fn a_read_only_word_waits_and_wakes_across_processes() {
    use rsbinder_fmq::shm::{self, Access, CreateOptions, Deadline, Mapping, RoWord32, Seals};
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicU32, Ordering};

    const BITS: std::num::NonZeroU32 = match std::num::NonZeroU32::new(0x4) {
        Some(bits) => bits,
        None => panic!("a zero mask"),
    };
    let limit = Duration::from_secs(10);
    let page = rustix::param::page_size() as u64;
    let word_at = |mapping: &Mapping, index: usize| {
        // SAFETY: a 4-aligned word inside the page the mapping covers.
        unsafe { NonNull::new_unchecked(mapping.as_ptr().as_ptr().add(4 * index)) }
    };

    if is_child() {
        let mut sock = child_sock();
        let desc = common::recv_descriptor(&sock);
        let fd = desc.fds[0].as_fd();
        assert_eq!(shm::verify_received(fd).expect("verify_received"), page);
        Mapping::map(fd, 0, page, Access::ReadWrite).expect_err("the memfd is write-sealed");
        let mapping = Mapping::map(fd, 0, page, Access::ReadOnly).expect("map read-only");
        // SAFETY: words of a live mapping that this process only loads, each at one size.
        let [gate, sleeper, done] =
            [0, 1, 2].map(|index| unsafe { RoWord32::from_ptr(word_at(&mapping, index)) });

        let deadline = Deadline::after(limit);
        sock.write_all(&[1]).expect("ready");
        let mut waits = 0u32;
        while gate.load_acquire() == 0 {
            gate.wait(0, BITS, deadline.as_ref()).expect("wait");
            waits += 1;
        }
        println!("gate={} waits={waits}", gate.load());

        // The parent sleeps on a word this process cannot change; the wake needs no write.
        let started = Instant::now();
        while done.load_acquire() == 0 {
            assert!(started.elapsed() < limit, "the parent did not wake");
            sleeper.wake(BITS).expect("wake");
            std::thread::sleep(Duration::from_millis(2));
        }
        println!("child done");
        return;
    }

    let path = common::sock_path("ro");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    let mut child = spawn_child("a_read_only_word_waits_and_wakes_across_processes", &path);
    let mut sock = common::accept_from(&listener, &mut child);

    let fd = shm::create(page, &CreateOptions::new("ro-word").populate(true)).expect("create");
    let mapping = Mapping::map(fd.as_fd(), 0, page, Access::ReadWrite).expect("map");
    shm::add_seals(fd.as_fd(), Seals::SHRINK | Seals::FUTURE_WRITE).expect("seal");
    // SAFETY: words of a live mapping, reached through atomics only, each at one size.
    let [gate, sleeper, done] =
        [0, 1, 2].map(|index| unsafe { word_at(&mapping, index).cast::<AtomicU32>().as_ref() });
    common::send_descriptor(
        &sock,
        &rsbinder_fmq::Descriptor {
            fds: vec![fd],
            ints: Vec::new(),
            grantors: Vec::new(),
            quantum: 4,
            flavor: rsbinder_fmq::Flavor::SynchronizedReadWrite,
        },
    );

    // Long enough for the child to be asleep in the futex, so that the wake is what ends it.
    let mut ready = [0u8; 1];
    sock.read_exact(&mut ready).expect("ready");
    std::thread::sleep(Duration::from_millis(200));
    gate.store(7, Ordering::Release);
    shm::futex_wake(gate, BITS).expect("wake");

    let deadline = Deadline::after(limit);
    shm::futex_wait(sleeper, 0, BITS, deadline.as_ref()).expect("woken by the child");
    done.store(1, Ordering::Release);

    let out = common::finish(child);
    assert!(
        out.contains("gate=7") && out.contains("child done"),
        "child output: {out}"
    );
    // A wait that returned without sleeping would spin the child's loop for the 200 ms.
    let waits: u32 = out
        .split("waits=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("waits");
    assert!(
        waits <= 3,
        "the child's wait returned without sleeping: {out}"
    );
    let _ = std::fs::remove_file(&path);
}
