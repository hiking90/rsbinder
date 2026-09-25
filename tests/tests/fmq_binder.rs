// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! An FMQ descriptor across the kernel binder, rsbinder at both ends
//! (plan 12-fmq F2, AC-12.1's other-process half): `fmq_probe serve` in a
//! child, this process as the caller, every case of `tests::fmq_peer`.
//! The driver translates the fds in the `NativeHandle` between the two
//! processes; a `Parcel` round trip in one process (`fmq_parcel.rs`)
//! never shows that.
//!
//! `#[ignore]`d: it needs `/dev/binder` and a permissive `rsb_hub`
//! (`target/debug/rsb_hub --config tests/policy/permissive.toml`), which
//! `integration-test.yml` has up. The C++ half of the same fixture,
//! against AOSP's libfmq, is `example-hello/cpp/run_fmq_interop.sh`.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tests::fmq_peer::{self, DEFAULT_CAPACITY, DEFAULT_COUNT};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_server(name: &str) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fmq_probe"))
        .args(["serve", name])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn fmq_probe");
    let stdout = child.stdout.take().unwrap();
    // Read on a thread: `lines.next()` has no deadline of its own.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(line)) if line.starts_with("SERVING ") => break,
            Ok(Ok(_)) => {}
            other => panic!("fmq_probe did not serve: {other:?}"),
        }
    }
    Server(child)
}

#[test]
#[ignore = "needs /dev/binder and a permissive rsb_hub"]
fn every_case_holds_across_the_kernel_binder() {
    let name = format!("rsbinder.test.fmq.{}", std::process::id());
    let _server = spawn_server(&name);

    let mut peer = None;
    let started = Instant::now();
    while peer.is_none() {
        peer = fmq_peer::connect(&name).ok();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "service not found"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let peer = peer.unwrap();

    let outcomes = fmq_peer::run_all(&peer, DEFAULT_COUNT, DEFAULT_CAPACITY);
    for outcome in &outcomes {
        println!("{}", outcome.line);
    }
    assert_eq!(outcomes.len(), 5);
    assert!(outcomes.iter().all(|o| o.ok), "{outcomes:#?}");

    // What rsbinder reports for each damage is its own contract:
    // `Corrupted` on every operation, so the report is -1 throughout.
    for outcome in &outcomes[2..] {
        assert!(
            outcome.line.ends_with("avail=-1/-1 read=-1 sum=0"),
            "{}",
            outcome.line
        );
    }
    let mine = outcomes[0].line.as_str();
    assert!(mine.contains("kind=memfd:sealed"), "{mine}");
}
