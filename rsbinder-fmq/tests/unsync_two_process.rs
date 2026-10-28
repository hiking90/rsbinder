// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The unsynchronized flavor across processes: the parent writes, child processes read through
//! the descriptor they were sent over a unix socket (`SCM_RIGHTS`). Plan 15 AC-15.1.1, 15.1.2
//! and 15.1.4.
//!
//! A child is this test binary re-run with `RSBINDER_FMQ_CHILD` set, filtered to the same test
//! function. Every wait has a deadline, and a child that hangs is killed and reported.
//!
//! `RSBINDER_FMQ_SOAK_SECS` sets how long the overwrite test runs (2 by default):
//!
//! ```text
//! RSBINDER_FMQ_SOAK_SECS=60 cargo test -p rsbinder-fmq --test unsync_two_process --release
//! ```
//!
//! The `cfg` is that of `tests/unsync.rs`: the targets on which `UnsyncReader` exists.

#![cfg(all(
    any(target_os = "linux", target_os = "android"),
    target_has_atomic = "64",
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        target_arch = "arm"
    )
))]

mod common;

use common::SockFile;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use rsbinder_fmq::shm::{Access, Mapping};
use rsbinder_fmq::{
    AttachPolicy, Error, UnsyncCreate, UnsyncRead, UnsyncReader, UnsyncStart, UnsyncWriter,
};

const CHILD_ENV: &str = "RSBINDER_FMQ_CHILD";
const SOCK_ENV: &str = "RSBINDER_FMQ_SOCK";
const BATCH_ENV: &str = "RSBINDER_FMQ_BATCH";
const SOAK_ENV: &str = "RSBINDER_FMQ_SOAK_SECS";
const CAPACITY: usize = 64;
const LIMIT: Duration = Duration::from_secs(20);

const POLICY: AttachPolicy = AttachPolicy {
    max_capacity: 1 << 16,
    require_seal: true,
    require_event_flag: false,
};

fn spawn_child(test_name: &str, sock: &PathBuf, batch: usize) -> Child {
    let exe = std::env::current_exe().expect("current_exe");
    common::spawn(
        Command::new(exe)
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .env(SOCK_ENV, sock)
            .env(BATCH_ENV, batch.to_string()),
    )
}

fn is_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

fn child_sock() -> UnixStream {
    let path: PathBuf = std::env::var_os(SOCK_ENV).unwrap().into();
    common::connect_with_retry(&path)
}

fn child_batch() -> usize {
    std::env::var(BATCH_ENV).unwrap().parse().unwrap()
}

/// The parent's side of the start: a queue, and one attached child per batch size.
fn start(
    test_name: &str,
    tag: &str,
    batches: &[usize],
) -> (UnsyncWriter<u64>, Vec<Peer>, SockFile) {
    let (path, listener) = SockFile::bind(tag);
    // The file is removed when the caller drops what this returns, a failed assertion included.
    let options = UnsyncCreate::new().read_only_readers(true);
    let writer = UnsyncWriter::<u64>::create(CAPACITY, &options).expect("create");
    let mut peers = Vec::new();
    for &batch in batches {
        let mut child = spawn_child(test_name, &path.0, batch);
        let mut sock = common::accept_from(&listener, &mut child);
        sock.set_read_timeout(Some(LIMIT)).expect("read timeout");
        common::send_descriptor(&sock, &writer.descriptor().expect("descriptor"));
        let mut attached = [0u8; 1];
        sock.read_exact(&mut attached).expect("attached");
        peers.push(Peer { child, sock });
    }
    (writer, peers, path)
}

struct Peer {
    child: Child,
    sock: UnixStream,
}

fn number_after(out: &str, key: &str) -> u64 {
    out.split(key)
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no `{key}` in the child's output: {out}"))
}

/// AC-15.1.1: three readers, each reads all the writer wrote, in order, through many wraps.
#[test]
fn three_readers_each_read_everything_in_order() {
    const ROUNDS: u64 = 300;
    const PER_ROUND: u64 = 40;
    if is_child() {
        let mut sock = child_sock();
        let desc = common::recv_descriptor(&sock);
        let mut reader =
            UnsyncReader::<u64>::attach(&desc, &POLICY, UnsyncStart::Oldest).expect("attach");
        sock.write_all(&[1]).expect("attached");
        let batch = child_batch();
        let started = Instant::now();
        let (mut next, mut sum) = (0u64, 0u64);
        let mut out = vec![0u64; batch];
        while next < ROUNDS * PER_ROUND {
            assert!(started.elapsed() < LIMIT, "stuck at element {next}");
            let n = reader.available_to_read().expect("available").min(batch);
            if n == 0 {
                std::thread::yield_now();
                continue;
            }
            let outcome = reader.read(&mut out[..n]).expect("read");
            assert_eq!(outcome, UnsyncRead::Done, "at element {next}");
            for &value in &out[..n] {
                assert_eq!(value, next);
                sum += value;
                next += 1;
                // The writer waits for this before it writes the next round.
                if next % PER_ROUND == 0 {
                    sock.write_all(&[1]).expect("round read");
                }
            }
        }
        println!("sum={sum}");
        return;
    }

    let name = "three_readers_each_read_everything_in_order";
    let (mut writer, mut peers, _path) = start(name, "u1", &[1, 7, 40]);
    let mut next = 0u64;
    for _ in 0..ROUNDS {
        // Three writes a round, of sizes that put the wrap inside a write.
        for n in [13u64, 26, 1] {
            let items: Vec<u64> = (next..next + n).collect();
            writer.write(&items).expect("write");
            next += n;
        }
        for peer in &mut peers {
            let mut round_read = [0u8; 1];
            peer.sock.read_exact(&mut round_read).expect("round read");
        }
    }
    assert_eq!(next, ROUNDS * PER_ROUND);
    let expected: u64 = (0..next).sum();
    for peer in peers {
        let out = common::finish(peer.child);
        assert_eq!(number_after(&out, "sum="), expected, "child output: {out}");
    }
}

/// AC-15.1.2: the writer overwrites without pause; what a reader gets as `Done` is whole.
#[test]
fn elements_read_while_the_writer_overwrites_are_whole() {
    if is_child() {
        let mut sock = child_sock();
        let desc = common::recv_descriptor(&sock);
        let mut reader =
            UnsyncReader::<u64>::attach(&desc, &POLICY, UnsyncStart::Oldest).expect("attach");
        sock.write_all(&[1]).expect("attached");
        sock.set_nonblocking(true).expect("nonblocking");
        let mut out = vec![0u64; child_batch()];
        // An element is its own index, so a stale or overwritten one breaks the run.
        let (mut expected, mut done, mut lost) = (0u64, 0u64, 0u64);
        let mut stop = [0u8; 1];
        'run: loop {
            for _ in 0..4096 {
                match reader.read(&mut out).expect("read") {
                    UnsyncRead::Done => {
                        for &value in &out {
                            if value != expected {
                                println!("broken: element {expected} read as {value}");
                                break 'run;
                            }
                            expected += 1;
                        }
                        done += 1;
                    }
                    UnsyncRead::Lost { elements } => {
                        expected += elements;
                        lost += 1;
                    }
                    UnsyncRead::Empty => std::hint::spin_loop(),
                }
            }
            match sock.read(&mut stop) {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("stop: {e}"),
            }
        }
        println!("done={done} lost={lost} upto={expected}");
        return;
    }

    let soak = std::env::var(SOAK_ENV).map_or(2, |secs| secs.parse().expect(SOAK_ENV));
    let name = "elements_read_while_the_writer_overwrites_are_whole";
    let (mut writer, mut peers, _path) = start(name, "u2", &[1, 16]);
    let started = Instant::now();
    let mut next = 0u64;
    let mut items = [0u64; 13];
    while started.elapsed() < Duration::from_secs(soak) {
        for _ in 0..4096 {
            let n = (next % 13 + 1) as usize;
            for slot in &mut items[..n] {
                *slot = next;
                next += 1;
            }
            writer.write(&items[..n]).expect("write");
        }
    }
    for peer in &mut peers {
        peer.sock.write_all(&[1]).expect("stop");
    }
    for peer in peers {
        let out = common::finish(peer.child);
        // For `--nocapture`: how much of each path the run covered.
        let counts = out.lines().find(|line| line.contains("done="));
        println!(
            "the writer wrote {next}; a reader: {}",
            counts.unwrap_or("?")
        );
        assert!(!out.contains("broken"), "child output: {out}");
        // Both paths ran: a run with no `Done` or no `Lost` would have checked nothing.
        assert!(number_after(&out, "done=") > 0, "child output: {out}");
        assert!(number_after(&out, "lost=") > 0, "child output: {out}");
        assert!(number_after(&out, "upto=") <= next, "child output: {out}");
    }
}

/// AC-15.1.4: with `read_only_readers` a reader process cannot map the queue writable, and reads.
#[test]
fn a_reader_process_cannot_map_the_queue_writable() {
    const ITEMS: u64 = 50;
    if is_child() {
        let mut sock = child_sock();
        let desc = common::recv_descriptor(&sock);
        let fd = desc.fds[0].as_fd();
        for grantor in &desc.grantors {
            let offset = u64::from(grantor.offset);
            assert_eq!(
                Mapping::map(fd, offset, grantor.extent, Access::ReadWrite).err(),
                Some(Error::Os(rustix::io::Errno::PERM)),
                "grantor at {offset}"
            );
        }
        println!("writable=EPERM");
        let mut reader =
            UnsyncReader::<u64>::attach(&desc, &POLICY, UnsyncStart::Oldest).expect("attach");
        sock.write_all(&[1]).expect("attached");
        // The parent writes everything before this byte arrives.
        let mut written = [0u8; 1];
        sock.read_exact(&mut written).expect("written");
        let mut out = vec![0u64; ITEMS as usize];
        assert_eq!(reader.read(&mut out).expect("read"), UnsyncRead::Done);
        println!("sum={}", out.iter().sum::<u64>());
        return;
    }

    let name = "a_reader_process_cannot_map_the_queue_writable";
    let (mut writer, mut peers, _path) = start(name, "u4", &[1]);
    let items: Vec<u64> = (1..=ITEMS).collect();
    writer.write(&items).expect("write");
    let mut peer = peers.pop().expect("one child");
    peer.sock.write_all(&[1]).expect("written");
    let out = common::finish(peer.child);
    assert!(out.contains("writable=EPERM"), "child output: {out}");
    assert_eq!(number_after(&out, "sum="), items.iter().sum::<u64>());
}
