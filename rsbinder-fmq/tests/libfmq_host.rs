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
//! RSBINDER_FMQ_LIBFMQ_PEER=$PWD/target/libfmq-host/libfmq_peer \
//!     cargo test -p rsbinder-fmq --test libfmq_host -- --ignored
//! ```

#![cfg(any(target_os = "linux", target_os = "android"))]

mod common;

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::time::Duration;

use rsbinder_fmq::{
    AttachPolicy, Error, Flavor, Grantor, MessageQueue, UnsyncCreate, UnsyncRead, UnsyncReader,
    UnsyncStart, UnsyncWriter, NOT_EMPTY, NOT_FULL,
};

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
    let (path, listener) = common::SockFile::bind("s1");
    let mut child = common::spawn(
        peer()
            .args(["attach", "read"])
            .arg(&path.0)
            .args([COUNT.to_string(), CAPACITY.to_string()]),
    );
    let mut sock = common::accept_from(&listener, &mut child);

    let mut q = MessageQueue::<u32>::create(CAPACITY, true).unwrap();
    common::send_descriptor(&sock, &q.descriptor().unwrap());
    let mut ack = [0u8];
    sock.read_exact(&mut ack).unwrap();
    write_all(&mut q);

    let out = common::finish(child);
    assert!(out.contains(&format!("sum={}", expected_sum())), "{out}");
    assert!(out.contains("capacity=64 grantors=4"), "{out}");
}

/// S1, the other role: rsbinder creates and reads; libfmq writes, blocking on `NOT_FULL`.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn s1_rsbinder_creates_libfmq_writes() {
    let (path, listener) = common::SockFile::bind("s1w");
    let mut child = common::spawn(
        peer()
            .args(["attach", "write"])
            .arg(&path.0)
            .args([COUNT.to_string(), CAPACITY.to_string()]),
    );
    let mut sock = common::accept_from(&listener, &mut child);

    let mut q = MessageQueue::<u32>::create(CAPACITY, true).unwrap();
    common::send_descriptor(&sock, &q.descriptor().unwrap());
    let mut ack = [0u8];
    sock.read_exact(&mut ack).unwrap(); // attached
    sock.write_all(&[1]).unwrap(); // go
    read_all(&mut q);
    sock.write_all(&[1]).unwrap(); // all read

    let out = common::finish(child);
    assert!(out.contains("done"), "{out}");
}

/// S2: libfmq creates (libcutils memfd, sealed `GROW | SHRINK`) and writes; rsbinder reads.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn s2_libfmq_creates_rsbinder_reads() {
    let (path, listener) = common::SockFile::bind("s2");
    let mut child = common::spawn(
        peer()
            .args(["create", "write"])
            .arg(&path.0)
            .args([COUNT.to_string(), CAPACITY.to_string()]),
    );
    let mut sock = common::accept_from(&listener, &mut child);

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
}

/// S2, the other role: libfmq creates and reads; rsbinder attaches and writes.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn s2_libfmq_creates_rsbinder_writes() {
    let (path, listener) = common::SockFile::bind("s2w");
    let mut child = common::spawn(
        peer()
            .args(["create", "read"])
            .arg(&path.0)
            .args([COUNT.to_string(), CAPACITY.to_string()]),
    );
    let mut sock = common::accept_from(&listener, &mut child);

    let desc = common::recv_descriptor(&sock);
    let mut ack = [0u8];
    sock.read_exact(&mut ack).unwrap(); // created
    let mut q = MessageQueue::<u32>::attach(&desc, &policy()).unwrap();
    write_all(&mut q);

    let out = common::finish(child);
    assert!(out.contains(&format!("sum={}", expected_sum())), "{out}");
}

// ---- The unsynchronized flavor (plan 15 AC-15.1.6) ---------------------------

fn unsync_policy() -> AttachPolicy {
    AttachPolicy {
        require_event_flag: false,
        ..policy()
    }
}

/// libfmq's unsynchronized queue in a child process, driven by the commands of `libfmq_peer.cpp`.
struct UnsyncPeer {
    child: Child,
    sock: UnixStream,
    _path: common::SockFile,
}

impl UnsyncPeer {
    /// Start `libfmq_peer unsync <origin>` and connect; nothing is exchanged yet.
    fn spawn(origin: &str, tag: &str) -> Self {
        let (path, listener) = common::SockFile::bind(tag);
        let mut child = common::spawn(
            peer()
                .args(["unsync", origin])
                .arg(&path.0)
                .args(["0", &CAPACITY.to_string()]),
        );
        let sock = common::accept_from(&listener, &mut child);
        sock.set_read_timeout(WAIT).unwrap();
        Self {
            child,
            sock,
            _path: path,
        }
    }

    /// A libfmq reader of `writer`'s queue, attached with `resetPointers = false`.
    fn attach(writer: &UnsyncWriter<u32>, tag: &str) -> Self {
        let mut peer = Self::spawn("attach", tag);
        common::send_descriptor(&peer.sock, &writer.descriptor().unwrap());
        peer.wait_attached();
        peer
    }

    fn wait_attached(&mut self) {
        let mut attached = [0u8];
        self.sock.read_exact(&mut attached).expect("attached");
    }

    fn command(&mut self, command: u8, arguments: &[u32]) -> u32 {
        let mut bytes = vec![command];
        for argument in arguments {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        self.sock.write_all(&bytes).unwrap();
        self.word()
    }

    fn word(&mut self) -> u32 {
        let mut word = [0u8; 4];
        self.sock.read_exact(&mut word).expect("reply");
        u32::from_le_bytes(word)
    }

    fn words(&mut self, n: u32) -> Vec<u32> {
        (0..n).map(|_| self.word()).collect()
    }

    /// `availableToRead()`.
    fn available(&mut self) -> usize {
        self.command(b'A', &[]) as usize
    }

    /// `read(buf, n)`: the elements, or `None` where libfmq returns `false`.
    fn read(&mut self, n: u32) -> Option<Vec<u32>> {
        (self.command(b'R', &[n]) != 0).then(|| self.words(n))
    }

    /// `readLatest(buf, n)`.
    fn latest(&mut self, n: u32) -> Vec<u32> {
        let count = self.command(b'L', &[n]);
        self.words(count)
    }

    /// `write(start .. start + n)`.
    fn write(&mut self, start: u32, n: u32) -> bool {
        self.command(b'W', &[n, start]) != 0
    }

    /// `beginWrite(n)` and `start .. start + n` copied into its regions, without a commit.
    fn begin(&mut self, start: u32, n: u32) -> bool {
        self.command(b'B', &[n, start]) != 0
    }

    /// `commitWrite(n)`.
    fn commit(&mut self, n: u32) -> bool {
        self.command(b'C', &[n]) != 0
    }

    fn quit(mut self) -> String {
        self.sock.write_all(b"Q").unwrap();
        let out = common::finish(self.child);
        assert!(out.contains("done"), "{out}");
        out
    }
}

fn write_range(writer: &mut UnsyncWriter<u32>, range: std::ops::Range<u32>) {
    for chunk in range.collect::<Vec<u32>>().chunks(20) {
        writer.write(chunk).unwrap();
    }
}

fn read_n(reader: &mut UnsyncReader<u32>, n: usize) -> (UnsyncRead, Vec<u32>) {
    let mut out = vec![0u32; n];
    (reader.read(&mut out).unwrap(), out)
}

fn latest_n(reader: &mut UnsyncReader<u32>, n: usize) -> Vec<u32> {
    let mut out = vec![0u32; n];
    let count = reader.read_latest(&mut out).unwrap();
    out.truncate(count);
    out
}

/// (a), (c), and (d) with `read_only`: one writer of this crate, a libfmq reader and one of ours.
fn rsbinder_writes_both_read(read_only: bool, tag: &str) {
    let options = UnsyncCreate::new().read_only_readers(read_only);
    let mut writer = UnsyncWriter::<u32>::create(CAPACITY, &options).unwrap();
    let desc = writer.descriptor().unwrap();
    let mut mine =
        UnsyncReader::<u32>::attach(&desc, &unsync_policy(), UnsyncStart::Oldest).unwrap();
    let mut theirs = UnsyncPeer::attach(&writer, tag);

    // In order, with nothing lost.
    write_range(&mut writer, 0..40);
    assert_eq!(theirs.available(), 40);
    assert_eq!(mine.available_to_read().unwrap(), 40);
    assert_eq!(theirs.read(40), Some((0..40).collect()));
    assert_eq!(read_n(&mut mine, 40), (UnsyncRead::Done, (0..40).collect()));
    assert_eq!(theirs.read(1), None);
    assert_eq!(read_n(&mut mine, 1).0, UnsyncRead::Empty);

    // Lapped: both count the same backlog, lose it, and resume half a ring behind the writer.
    write_range(&mut writer, 40..200);
    assert_eq!(theirs.available(), 160);
    assert_eq!(mine.available_to_read().unwrap(), 160);
    assert_eq!(theirs.read(8), None);
    assert_eq!(read_n(&mut mine, 8).0, UnsyncRead::Lost { elements: 128 });
    assert_eq!(theirs.available(), 32);
    assert_eq!(mine.available_to_read().unwrap(), 32);
    assert_eq!(theirs.read(8), Some((168..176).collect()));
    assert_eq!(
        read_n(&mut mine, 8),
        (UnsyncRead::Done, (168..176).collect())
    );

    // The latest elements: of a short backlog, of a lapped one, and of none.
    assert_eq!(theirs.latest(3), [197, 198, 199]);
    assert_eq!(latest_n(&mut mine, 3), [197, 198, 199]);
    assert_eq!(
        (theirs.available(), mine.available_to_read().unwrap()),
        (0, 0)
    );
    write_range(&mut writer, 200..207);
    assert_eq!(theirs.latest(16), (200..207).collect::<Vec<u32>>());
    assert_eq!(latest_n(&mut mine, 16), (200..207).collect::<Vec<u32>>());
    write_range(&mut writer, 207..407);
    assert_eq!(theirs.available(), 200);
    assert_eq!(mine.available_to_read().unwrap(), 200);
    assert_eq!(theirs.latest(5), [402, 403, 404, 405, 406]);
    assert_eq!(latest_n(&mut mine, 5), [402, 403, 404, 405, 406]);
    write_range(&mut writer, 407..607);
    let ring: Vec<u32> = (607 - CAPACITY as u32..607).collect();
    assert_eq!(theirs.latest(100), ring);
    assert_eq!(latest_n(&mut mine, 100), ring);
    assert!(theirs.latest(1).is_empty());
    assert!(latest_n(&mut mine, 1).is_empty());

    let out = theirs.quit();
    assert!(out.contains("resetPointers=0"), "{out}");
    assert!(out.contains("capacity=64 grantors=3"), "{out}");
}

/// (a) and (c): a libfmq reader, attached with `resetPointers = false`, reads our writer's queue.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn u1_rsbinder_writes_libfmq_reads() {
    rsbinder_writes_both_read(false, "u1");
}

/// (d): the same on a queue libfmq can only map read-only (its `EPERM` fallback).
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn u2_libfmq_reads_a_read_only_queue() {
    rsbinder_writes_both_read(true, "u2");
}

/// (b) and (c): libfmq creates and writes; a reader of ours and libfmq's own read the same.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn u3_libfmq_writes_rsbinder_reads() {
    let mut theirs = UnsyncPeer::spawn("create", "u3");
    let desc = common::recv_descriptor(&theirs.sock);
    theirs.wait_attached();
    let ours = UnsyncWriter::<u32>::create(CAPACITY, &UnsyncCreate::new()).unwrap();
    assert_eq!(
        desc.grantors,
        ours.descriptor().unwrap().grantors,
        "libfmq layout differs from ours"
    );
    assert_eq!(desc.flavor, Flavor::UnsynchronizedWrite);
    assert_eq!((desc.quantum, desc.fds.len()), (4, 1));
    assert!(rsbinder_fmq::shm::shrink_sealed(desc.fds[0].as_fd()));
    let mut mine =
        UnsyncReader::<u32>::attach(&desc, &unsync_policy(), UnsyncStart::Oldest).unwrap();

    assert!(theirs.write(0, 40));
    assert_eq!(mine.available_to_read().unwrap(), theirs.available());
    assert_eq!(read_n(&mut mine, 40), (UnsyncRead::Done, (0..40).collect()));
    assert_eq!(theirs.read(40), Some((0..40).collect()));

    // Lapped by libfmq's writer.
    for start in (40..200).step_by(20) {
        assert!(theirs.write(start, 20));
    }
    assert_eq!(mine.available_to_read().unwrap(), 160);
    assert_eq!(theirs.available(), 160);
    assert_eq!(read_n(&mut mine, 8).0, UnsyncRead::Lost { elements: 128 });
    assert_eq!(theirs.read(8), None);
    assert_eq!(
        read_n(&mut mine, 8),
        (UnsyncRead::Done, (168..176).collect())
    );
    assert_eq!(theirs.read(8), Some((168..176).collect()));
    assert_eq!(latest_n(&mut mine, 3), [197, 198, 199]);
    assert_eq!(theirs.latest(3), [197, 198, 199]);

    // libfmq accepts a write of the whole capacity; both readers were caught up (`readLatest`).
    assert!(theirs.write(200, CAPACITY as u32));
    assert_eq!(
        read_n(&mut mine, CAPACITY),
        (UnsyncRead::Done, (200..264).collect())
    );
    assert_eq!(theirs.read(CAPACITY as u32), Some((200..264).collect()));
    // One element more and both are lapped, by a write that is one ring long.
    assert!(theirs.write(264, CAPACITY as u32));
    assert!(theirs.write(328, 1));
    assert_eq!(read_n(&mut mine, 8).0, UnsyncRead::Lost { elements: 33 });
    assert_eq!(theirs.read(8), None);
    assert_eq!(
        read_n(&mut mine, 8),
        (UnsyncRead::Done, (297..305).collect())
    );
    assert_eq!(theirs.read(8), Some((297..305).collect()));
    assert!(!theirs.write(329, CAPACITY as u32 + 1));

    assert!(theirs.quit().contains("capacity=64 grantors=3"));
}

/// (e): libfmq's default attach stores to the counters, which faults on a read-only queue.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn u4_a_default_attach_to_a_read_only_queue_ends_that_reader() {
    use std::os::unix::process::ExitStatusExt;
    let options = UnsyncCreate::new().read_only_readers(true);
    let mut writer = UnsyncWriter::<u32>::create(CAPACITY, &options).unwrap();
    let desc = writer.descriptor().unwrap();
    let mut mine =
        UnsyncReader::<u32>::attach(&desc, &unsync_policy(), UnsyncStart::Oldest).unwrap();
    write_range(&mut writer, 0..10);
    assert_eq!(read_n(&mut mine, 5), (UnsyncRead::Done, (0..5).collect()));

    let peer = UnsyncPeer::spawn("attach-reset", "u4");
    common::send_descriptor(&peer.sock, &desc);
    let (status, out) = common::finish_with_status(peer.child);
    assert_eq!(status.signal(), Some(11), "{status}: {out}");
    assert!(out.contains("resetPointers=1"), "{out}");
    assert!(!out.contains("capacity="), "the attach returned: {out}");

    // Nothing was stored: the other reader and the writer go on.
    assert_eq!(read_n(&mut mine, 5), (UnsyncRead::Done, (5..10).collect()));
    write_range(&mut writer, 10..30);
    assert_eq!(
        read_n(&mut mine, 20),
        (UnsyncRead::Done, (10..30).collect())
    );
}

/// (f): on a queue it can write, the same attach zeroes the counters under the other readers.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn u5_a_default_attach_to_a_writable_queue_ends_our_readers() {
    let mut writer = UnsyncWriter::<u32>::create(CAPACITY, &UnsyncCreate::new()).unwrap();
    let desc = writer.descriptor().unwrap();
    let attach = || UnsyncReader::<u32>::attach(&desc, &unsync_policy(), UnsyncStart::Oldest);
    let mut mine = attach().unwrap();
    write_range(&mut writer, 0..10);
    assert_eq!(read_n(&mut mine, 5), (UnsyncRead::Done, (0..5).collect()));

    let mut theirs = UnsyncPeer::spawn("attach-reset", "u5");
    common::send_descriptor(&theirs.sock, &desc);
    theirs.wait_attached();

    // A reader that had seen the write counter at 10 elements now sees it at zero.
    let corrupted = Error::Corrupted("write counter moved backwards");
    assert_eq!(mine.read(&mut [0u32; 1]), Err(corrupted));
    // One that attaches now has seen nothing else, and reads an empty queue.
    let mut late = attach().unwrap();
    assert_eq!(late.available_to_read().unwrap(), 0);
    assert_eq!(theirs.available(), 0);

    // The writer stores its own counters again; the reader that failed stays failed.
    write_range(&mut writer, 10..12);
    assert_eq!(late.available_to_read().unwrap(), 12);
    assert_eq!(theirs.available(), 12);
    assert_eq!(read_n(&mut late, 12), (UnsyncRead::Done, (0..12).collect()));
    assert_eq!(theirs.read(12), Some((0..12).collect()));
    assert_eq!(mine.available_to_read(), Err(corrupted));

    assert!(theirs.quit().contains("resetPointers=1"));
}

/// The write region end where it is ahead of the write counter: our writer, libfmq's reader.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn u6_libfmq_reads_the_region_end_our_writer_stores() {
    let mut writer = UnsyncWriter::<u32>::create(CAPACITY, &UnsyncCreate::new()).unwrap();
    let desc = writer.descriptor().unwrap();
    let mut mine =
        UnsyncReader::<u32>::attach(&desc, &unsync_policy(), UnsyncStart::Oldest).unwrap();
    let mut theirs = UnsyncPeer::attach(&writer, "u6");
    let both = |theirs: &mut UnsyncPeer, mine: &UnsyncReader<u32>| {
        (theirs.available(), mine.available_to_read().unwrap())
    };

    // A reservation moves the region end alone: the count of committed elements stands.
    write_range(&mut writer, 0..64);
    writer.begin_write(8).unwrap();
    assert_eq!(both(&mut theirs, &mine), (64, 64));
    // The end is at element 72, more than one ring past element 0: both readers keep half a ring.
    assert_eq!(theirs.read(1), None);
    assert_eq!(read_n(&mut mine, 1).0, UnsyncRead::Lost { elements: 32 });
    assert_eq!(both(&mut theirs, &mine), (32, 32));

    // Three of the eight committed: libfmq reads on with the end five ahead of the counter.
    let reserved: Vec<u32> = (64..72).collect();
    writer
        .begin_write(8)
        .unwrap()
        .write_at(0, &reserved)
        .unwrap();
    writer.commit_write(3).unwrap();
    assert_eq!(both(&mut theirs, &mine), (35, 35));
    assert_eq!(theirs.read(35), Some((32..67).collect()));
    assert_eq!(
        read_n(&mut mine, 35),
        (UnsyncRead::Done, (32..67).collect())
    );
    writer.commit_write(5).unwrap();
    assert_eq!(theirs.read(5), Some((67..72).collect()));
    assert_eq!(read_n(&mut mine, 5), (UnsyncRead::Done, (67..72).collect()));

    // Twenty written over elements 64 to 83, four committed, then a shorter reservation.
    write_range(&mut writer, 72..128);
    let reserved: Vec<u32> = (128..148).collect();
    writer
        .begin_write(20)
        .unwrap()
        .write_at(0, &reserved)
        .unwrap();
    writer.commit_write(4).unwrap();
    writer.begin_write(1).unwrap();
    assert_eq!(both(&mut theirs, &mine), (60, 60));
    // Both stand at element 72, within one ring of the counter and overwritten: the end says so.
    assert_eq!(theirs.read(1), None);
    assert_eq!(read_n(&mut mine, 1).0, UnsyncRead::Lost { elements: 28 });
    assert_eq!(theirs.read(32), Some((100..132).collect()));
    assert_eq!(
        read_n(&mut mine, 32),
        (UnsyncRead::Done, (100..132).collect())
    );
    writer.commit_write(16).unwrap();
    assert_eq!(theirs.read(16), Some((132..148).collect()));
    assert_eq!(
        read_n(&mut mine, 16),
        (UnsyncRead::Done, (132..148).collect())
    );

    theirs.quit();
}

/// The same word as libfmq's writer stores it, lowered when part of a reservation is committed.
#[test]
#[ignore = "needs RSBINDER_FMQ_LIBFMQ_PEER (tests/cpp/build_host_peer.sh)"]
fn u7_rsbinder_reads_the_region_end_libfmq_stores() {
    let mut theirs = UnsyncPeer::spawn("create", "u7");
    let desc = common::recv_descriptor(&theirs.sock);
    theirs.wait_attached();
    let attach = || UnsyncReader::<u32>::attach(&desc, &unsync_policy(), UnsyncStart::Oldest);
    let (mut mine, mut behind) = (attach().unwrap(), attach().unwrap());

    for start in (0..60).step_by(20) {
        assert!(theirs.write(start, 20));
    }
    assert!(theirs.write(60, 4));
    assert_eq!(read_n(&mut behind, 4), (UnsyncRead::Done, (0..4).collect()));

    // `beginWrite` alone stores the region end: the count stands, and element 0 is lost.
    assert!(theirs.begin(64, 8));
    assert_eq!(mine.available_to_read().unwrap(), 64);
    assert_eq!(theirs.available(), 64);
    assert_eq!(read_n(&mut mine, 1).0, UnsyncRead::Lost { elements: 32 });
    assert_eq!(theirs.read(1), None);
    assert_eq!(mine.available_to_read().unwrap(), 32);
    assert_eq!(theirs.available(), 32);

    // A commit of three lowers libfmq's end to the counter; what is committed reads the same.
    assert!(theirs.commit(3));
    assert_eq!(
        read_n(&mut mine, 35),
        (UnsyncRead::Done, (32..67).collect())
    );
    assert_eq!(theirs.read(35), Some((32..67).collect()));
    // Nothing reserved is left to commit, and no counter covers the five that were written.
    assert!(!theirs.commit(1));
    assert_eq!(read_n(&mut behind, 1), (UnsyncRead::Done, vec![68]));

    theirs.quit();
}

/// Pins the Rust side's single-fd layout; libfmq's own is exercised only by the tests above.
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
