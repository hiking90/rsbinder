// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What the tests of two processes share: the other process is this test binary again, running
//! its `child` test with [`ROLE`] set; it answers on stdout with lines that start with `topic:`
//! and exits when its stdin closes.

#![allow(dead_code, reason = "each test binary uses a part of it")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::sync::Once;
use std::time::{Duration, Instant};

use rsbinder::ProcessState;

pub const ROLE: &str = "RSBINDER_TOPIC_ROLE";
pub const NAME: &str = "RSBINDER_TOPIC_NAME";
pub const WAIT: Duration = Duration::from_secs(10);

pub fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        ProcessState::init_default().expect("a binder device");
        ProcessState::start_thread_pool();
    });
}

pub fn name(tag: &str) -> String {
    format!("rsb.test.topic.{tag}.{}", std::process::id())
}

/// Poll `done` until it holds or [`WAIT`] passes.
pub fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {WAIT:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Say `line` to the test that spawned this child.
pub fn say(line: &str) {
    println!("topic: {line}");
}

/// The other process, killed when dropped.
pub struct Peer {
    pub child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl Peer {
    pub fn spawn(role: &str, name: &str) -> Self {
        Self::spawn_with(role, name, &[])
    }

    /// [`spawn`](Self::spawn) with more variables in the child's environment.
    pub fn spawn_with(role: &str, name: &str, env: &[(&str, &str)]) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROLE, role)
            .env(NAME, name)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // Not only at the start: libtest's `test child ... ` has no newline before it.
                if let Some((_, said)) = line.split_once("topic: ") {
                    let _ = send.send(said.to_owned());
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
        }
    }

    /// The next line the peer says.
    pub fn says(&self) -> String {
        self.lines.recv_timeout(WAIT).expect("the peer to answer")
    }

    /// The next line the peer says before `deadline`.
    pub fn says_by(&self, deadline: Instant) -> String {
        let left = deadline.saturating_duration_since(Instant::now());
        self.lines
            .recv_timeout(left)
            .expect("the peer to answer in time")
    }

    /// Stop the peer with `SIGSTOP` until the guard is dropped, also when the test fails.
    pub fn stop(&self) -> Stopped {
        signal(self, rustix::process::Signal::STOP);
        // Until every thread stopped: the call that follows meets no running thread of it.
        let pid = rustix::process::Pid::from_raw(self.child.id() as i32).unwrap();
        let stopped =
            rustix::process::WaitIdOptions::STOPPED | rustix::process::WaitIdOptions::NOWAIT;
        rustix::process::waitid(rustix::process::WaitId::Pid(pid), stopped).unwrap();
        Stopped(self.child.id())
    }

    pub fn tell(&mut self, line: &str) {
        writeln!(self.stdin.as_mut().unwrap(), "{line}").unwrap();
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.kill();
    }
}

pub fn signal(peer: &Peer, signal: rustix::process::Signal) {
    send(peer.child.id(), signal).unwrap();
}

fn send(pid: u32, signal: rustix::process::Signal) -> rustix::io::Result<()> {
    let pid = rustix::process::Pid::from_raw(pid as i32).unwrap();
    rustix::process::kill_process(pid, signal)
}

/// A peer [`Peer::stop`] stopped, sent `SIGCONT` when dropped; drop it before its `Peer`.
#[must_use = "the peer runs again when the guard is dropped"]
pub struct Stopped(u32);

impl Stopped {
    /// Let the peer run before the guard is dropped, which sends `SIGCONT` once more.
    pub fn resume(&self) {
        // No panic in a drop that may run while a failed test unwinds.
        let _ = send(self.0, rustix::process::Signal::CONT);
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        self.resume();
    }
}

/// Read stdin until it closes: the test that spawned this child ended or dropped it.
pub fn wait_for_eof() {
    let mut line = String::new();
    while std::io::stdin()
        .read_line(&mut line)
        .is_ok_and(|read| read != 0)
    {
        line.clear();
    }
}
