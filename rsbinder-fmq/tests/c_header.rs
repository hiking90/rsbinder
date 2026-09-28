// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! This crate against `c/rsbinder_fmq.h`, the C header an NDK peer uses:
//! the peer in `tests/c/rsbfmq_peer.c` is compiled here with the system C
//! compiler (`$CC`, else `cc`) and driven the way `libfmq_host.rs` drives
//! libfmq — both creators, both roles, the descriptor over a unix socket.
//! It also records the header's answer to a descriptor or counters that
//! break the rules, and compiles the header as C++ (`$CXX`, else `c++`)
//! with its NDK-type templates instantiated.
//!
//! Linux only: the header targets Android too, but this harness needs a C
//! compiler where the test runs, which a device does not have.

#![cfg(target_os = "linux")]

mod common;

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use rsbinder_fmq::{AttachPolicy, Descriptor, Flavor, Grantor, MessageQueue, NOT_EMPTY, NOT_FULL};

const COUNT: u32 = 5000;
const CAPACITY: usize = 64;
const WAIT: Option<Duration> = Some(Duration::from_secs(10));

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Compile one source with warnings as errors; panics with the compiler's output.
fn compile(compiler_env: &str, default: &str, args: &[&str], src: &str, out: &str) -> PathBuf {
    let compiler = std::env::var(compiler_env).unwrap_or_else(|_| default.to_string());
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join(out);
    let result = Command::new(&compiler)
        .args(args)
        .args(["-Wall", "-Wextra", "-Werror", "-O1", "-I"])
        .arg(crate_dir().join("c"))
        .arg(crate_dir().join("tests/c").join(src))
        .arg("-o")
        .arg(&out)
        .output()
        .unwrap_or_else(|e| panic!("{compiler}: {e} (set ${compiler_env})"));
    assert!(
        result.status.success(),
        "{compiler} {src}:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    out
}

fn peer() -> Command {
    static PEER: OnceLock<PathBuf> = OnceLock::new();
    let path = PEER.get_or_init(|| {
        compile(
            "CC",
            "cc",
            &["-std=c11", "-pedantic"],
            "rsbfmq_peer.c",
            "rsbfmq_peer",
        )
    });
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

/// The peer in one of its four `<origin> <role>` modes, connected.
fn start(
    tag: &str,
    origin: &str,
    role: &str,
) -> (std::process::Child, std::os::unix::net::UnixStream, PathBuf) {
    let path = common::sock_path(tag);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let mut child = common::spawn(
        peer()
            .args([origin, role])
            .arg(&path)
            .args([COUNT.to_string(), CAPACITY.to_string()]),
    );
    let sock = common::accept_from(&listener, &mut child);
    (child, sock, path)
}

/// Rust creates; the C header attaches and reads.
#[test]
fn rust_creates_c_reads() {
    let (child, mut sock, path) = start("c1r", "attach", "read");
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

/// Rust creates and reads; the C header attaches and writes, waiting on `NOT_FULL` when full.
#[test]
fn rust_creates_c_writes() {
    let (child, mut sock, path) = start("c1w", "attach", "write");
    let mut q = MessageQueue::<u32>::create(CAPACITY, true).unwrap();
    common::send_descriptor(&sock, &q.descriptor().unwrap());
    let mut ack = [0u8];
    sock.read_exact(&mut ack).unwrap();
    sock.write_all(&[1]).unwrap();
    read_all(&mut q);
    sock.write_all(&[1]).unwrap();

    let out = common::finish(child);
    assert!(out.contains("done"), "{out}");
    let _ = std::fs::remove_file(&path);
}

/// The C header creates and writes; Rust attaches under the strict policy and reads.
#[test]
fn c_creates_rust_reads() {
    let (child, mut sock, path) = start("c2r", "create", "write");
    let desc = common::recv_descriptor(&sock);
    let mine = MessageQueue::<u32>::create(CAPACITY, true)
        .unwrap()
        .descriptor()
        .unwrap();
    assert_eq!(
        desc.grantors, mine.grantors,
        "the C layout differs from ours"
    );
    assert_eq!(desc.quantum, 4);
    assert_eq!(desc.fds.len(), 1);
    assert!(rsbinder_fmq::shm::shrink_sealed(desc.fds[0].as_fd()));
    let mut ack = [0u8];
    sock.read_exact(&mut ack).unwrap();

    let mut q = MessageQueue::<u32>::attach(&desc, &policy()).unwrap();
    sock.write_all(&[1]).unwrap();
    read_all(&mut q);
    sock.write_all(&[1]).unwrap();

    let out = common::finish(child);
    assert!(out.contains("done"), "{out}");
    let _ = std::fs::remove_file(&path);
}

/// The C header creates and reads; Rust attaches and writes.
#[test]
fn c_creates_rust_writes() {
    let (child, mut sock, path) = start("c2w", "create", "read");
    let desc = common::recv_descriptor(&sock);
    let mut ack = [0u8];
    sock.read_exact(&mut ack).unwrap();
    let mut q = MessageQueue::<u32>::attach(&desc, &policy()).unwrap();
    write_all(&mut q);

    let out = common::finish(child);
    assert!(out.contains(&format!("sum={}", expected_sum())), "{out}");
    let _ = std::fs::remove_file(&path);
}

/// `probe` mode: the header's answer to `desc`, then to the counters after `damage` ran.
fn probe(tag: &str, desc: &Descriptor, max_capacity: usize, damage: impl FnOnce()) -> String {
    let path = common::sock_path(tag);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let mut child = common::spawn(peer().arg("probe").arg(&path).arg(max_capacity.to_string()));
    let mut sock = common::accept_from(&listener, &mut child);
    common::send_descriptor(&sock, desc);
    let mut ack = [0u8];
    sock.read_exact(&mut ack).unwrap();
    damage();
    let _ = sock.write_all(&[1]);
    let out = common::finish(child);
    let _ = std::fs::remove_file(&path);
    out
}

fn errno(e: rustix::io::Errno) -> i32 {
    -e.raw_os_error()
}

fn rust_queue() -> (MessageQueue<u32>, Descriptor) {
    let q = MessageQueue::<u32>::create(CAPACITY, true).unwrap();
    let desc = q.descriptor().unwrap();
    (q, desc)
}

// The page-rounded memfd size, not a hard-coded 4096: pages are 16/64 KiB on some aarch64 hosts.
fn region_end(d: &Descriptor) -> u32 {
    let size = rsbinder_fmq::shm::region_size(d.fds[0].as_fd()).unwrap();
    u32::try_from(size).unwrap()
}

/// The descriptor checks `validate` makes, answered `-EINVAL` by the header.
#[test]
fn c_refuses_what_rust_refuses() {
    let inval = format!("attach={}", errno(rustix::io::Errno::INVAL));

    let (_q, desc) = rust_queue();
    let out = probe("ok", &desc, CAPACITY, || {});
    assert!(out.contains("attach=0"), "a valid descriptor: {out}");

    // Each edit breaks one rule: a moved word stays clear of the ring, in the fd but for past-end.
    type Case = (&'static str, fn(&mut Descriptor), usize);
    let cases: [Case; 8] = [
        ("misaligned", |d| d.grantors[3].offset += 4, CAPACITY),
        ("no-word", |d| d.grantors.truncate(3), CAPACITY),
        (
            "past-end",
            |d| d.grantors[3].offset = region_end(d),
            CAPACITY,
        ),
        ("overlap", |d| d.grantors[1].offset = 0, CAPACITY),
        ("fd-index", |d| d.grantors[3].fd_index = 1, CAPACITY),
        ("min-extent", |d| d.grantors[0].extent = 4, CAPACITY),
        ("quantum", |d| d.quantum = 8, CAPACITY),
        (
            "flavor",
            |d| d.flavor = Flavor::UnsynchronizedWrite,
            CAPACITY,
        ),
    ];
    for (tag, edit, max) in cases {
        let (_q, mut desc) = rust_queue();
        edit(&mut desc);
        let out = probe(tag, &desc, max, || {});
        assert!(out.contains(&inval), "{tag}: {out}");
    }

    let (_q, desc) = rust_queue();
    let out = probe("too-big", &desc, CAPACITY - 1, || {});
    assert!(out.contains(&inval), "capacity above the policy: {out}");

    // A memfd nobody sealed: the peer could shrink it under the mapping.
    let fd = rustix::fs::memfd_create(
        "unsealed",
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )
    .unwrap();
    rustix::fs::ftruncate(&fd, 4096).unwrap();
    let (_q, sealed) = rust_queue();
    let unsealed = Descriptor {
        fds: vec![fd],
        ints: Vec::new(),
        grantors: sealed.grantors.clone(),
        quantum: 4,
        flavor: Flavor::SynchronizedReadWrite,
    };
    let out = probe("unsealed", &unsealed, CAPACITY, || {});
    assert!(out.contains(&inval), "an unsealed memfd: {out}");
}

/// Counters the peer moved out of their invariant read as `-EBADMSG`.
#[test]
fn c_refuses_hostile_counters() {
    let bad = errno(rustix::io::Errno::BADMSG);
    let near_wrap = u64::MAX - 255;
    let damages: [(&str, u64, u64); 4] = [
        ("read-ahead", 8, 0),
        ("overfull", 0, 4 * CAPACITY as u64 + 4),
        ("unaligned-counters", 2, 2),
        ("near-wrap", near_wrap, near_wrap),
    ];
    for (tag, read, write) in damages {
        let (_q, desc) = rust_queue();
        let fd = desc.fds[0].try_clone().unwrap();
        let out = probe(tag, &desc, CAPACITY, move || {
            // SAFETY: a fresh shared map of the counters' page (offsets 0, 8); unmapped after.
            unsafe {
                let base = rustix::mm::mmap(
                    std::ptr::null_mut(),
                    4096,
                    rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                    rustix::mm::MapFlags::SHARED,
                    &fd,
                    0,
                )
                .unwrap()
                .cast::<u64>();
                base.write_volatile(read);
                base.add(1).write_volatile(write);
                rustix::mm::munmap(base.cast(), 4096).unwrap();
            }
        });
        assert!(
            out.contains(&format!(
                "available={bad} begin_read={bad} begin_write={bad}"
            )),
            "{tag}: {out}"
        );
    }
}

/// The header as C++: NDK-type templates round-trip a created queue and refuse a negative field.
#[test]
fn cxx_templates_round_trip() {
    let exe = compile(
        "CXX",
        "c++",
        &["-std=c++17"],
        "cxx_check.cpp",
        "rsbfmq_cxx_check",
    );
    let out = Command::new(&exe).output().unwrap();
    assert!(
        out.status.success() && out.stdout == b"ok\n",
        "{}: exit {:?}, {}",
        exe.display(),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn layout_constant_matches() {
    // The Rust side's single-fd layout; the C side's is exercised only by the peer tests.
    let (_q, d) = rust_queue();
    assert_eq!(
        d.grantors[3],
        Grantor {
            fd_index: 0,
            offset: 272,
            extent: 4
        }
    );
}
