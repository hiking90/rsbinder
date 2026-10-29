// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The topic fleet's controller (plan 15 fleet test): it starts `topic_node` processes, sends
//! them commands and signals, and judges the event lines they print.
//!
//! Every node's lines come to one channel. A wait reads that channel until its condition holds
//! and fails at once on a `FAULT`, a `VIOLATION`, an unparsable line or a node that ended other
//! than the controller expected, so a node that dies at any point fails the next wait. Each test
//! keeps its event log and the nodes' stderr in a run directory, which stays when the test fails
//! or `FLEET_KEEP=1`.
//!
//! Requires a binder device and a service manager that accepts `addService` for any name
//! (`rsb_hub --insecure-allow-all`, or the kernel tier's permissive policy):
//!
//! ```text
//! cargo test -p tests --test topic_fleet -- --ignored --test-threads=1
//! ```
//!
//! `TOPIC_NODE_BIN` replaces the node binary and `TOPIC_FLEET_DIR` the directory the run
//! directories go in (`target/tmp/topic-fleet`), for a device the binaries were pushed to.

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};
use tests::topic_fleet::event::{Event, Kind};
use tests::topic_fleet::judge::{self, check_end, end_reason, Injected, Mode};
use tests::topic_fleet::now_ns;
use tests::topic_fleet::record::Incarnation;

/// From a spawn to the node's `READY`.
const READY: Duration = Duration::from_secs(10);
/// From an exit or a kill to the subscribers' end (plan 15 fleet test §5.4).
const END: Duration = Duration::from_secs(1);
/// From `exit` to the end of a node that has nothing left to wait for.
const EXIT: Duration = Duration::from_secs(10);
const STDERR_TAIL: usize = 50;

/// What a node's reader thread hands the controller.
enum Message {
    Line(Event),
    Garbled { node: String, line: String },
    Eof { node: String, start: u32 },
}

/// What a wait can be for: an event, or a node's end the controller expected.
#[derive(Debug)]
enum Seen {
    Event(Event),
    Ended { node: String },
}

/// How the controller expects a node to end; any other end fails the run.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    /// Not at all.
    Running,
    /// Exit 0, after `exit`.
    Exit,
    /// By this signal, which the controller sent.
    Signal(i32),
}

impl Expect {
    fn holds(self, status: ExitStatus) -> bool {
        match self {
            Self::Running => false,
            Self::Exit => status.code() == Some(0),
            Self::Signal(signal) => status.signal() == Some(signal),
        }
    }
}

struct Node {
    child: Child,
    stdin: Option<ChildStdin>,
    start: u32,
    expect: Expect,
    stderr: PathBuf,
    status: Option<ExitStatus>,
}

/// A failed wait: what was waited for, what came instead, and the stderr of the node at fault.
struct Failure(String);

impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

struct Fleet {
    tag: String,
    dir: PathBuf,
    log: File,
    nodes: HashMap<String, Node>,
    starts: HashMap<String, u32>,
    send: mpsc::Sender<Message>,
    recv: mpsc::Receiver<Message>,
    /// What earlier waits read but did not match, oldest first.
    backlog: VecDeque<Seen>,
    /// The latest `COUNT` of each node.
    counts: HashMap<String, Event>,
    /// The `PUBLISHED` of each node.
    published: HashMap<String, Event>,
}

fn node_bin() -> PathBuf {
    std::env::var_os("TOPIC_NODE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_topic_node").into())
}

fn tail(path: &Path, lines: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

impl Fleet {
    /// A fleet whose names and run directory carry `tag`, unique among this binary's tests.
    fn new(tag: &str) -> Self {
        let base = std::env::var_os("TOPIC_FLEET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_TARGET_TMPDIR")).join("topic-fleet"));
        let dir = base.join(format!("{tag}.{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = File::create(dir.join("events.log")).unwrap();
        let (send, recv) = mpsc::channel();
        Self {
            tag: tag.into(),
            dir,
            log,
            nodes: HashMap::new(),
            starts: HashMap::new(),
            send,
            recv,
            backlog: VecDeque::new(),
            counts: HashMap::new(),
            published: HashMap::new(),
        }
    }

    /// The service name of `topic` in this fleet: no other test or run shares it.
    fn name(&self, topic: &str) -> String {
        format!("rsb.fleet.{}.{topic}.{}", self.tag, std::process::id())
    }

    /// Start `node` with `role`, and wait for its `READY`.
    fn spawn(&mut self, node: &str, role: &[&str]) {
        if let Some(running) = self.nodes.get(node) {
            assert!(running.status.is_some(), "{node} is running already");
        }
        let start = self.starts.entry(node.into()).or_insert(0);
        *start += 1;
        let start = *start;
        let stderr = self.dir.join(format!("{node}.{start}.stderr"));
        let mut child = Command::new(node_bin())
            .args(["--node", node, "--start", &start.to_string()])
            .args(role)
            .current_dir(&self.dir)
            .env("FLEET_CONTROLLER_PID", std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(File::create(&stderr).unwrap())
            .spawn()
            .unwrap_or_else(|err| panic!("spawn {}: {err}", node_bin().display()));
        let stdout = child.stdout.take().unwrap();
        let send = self.send.clone();
        let name = node.to_string();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let message = match Event::parse(&line) {
                    Ok(event) => Message::Line(event),
                    Err(_) => Message::Garbled {
                        node: name.clone(),
                        line,
                    },
                };
                if send.send(message).is_err() {
                    return;
                }
            }
            let _ = send.send(Message::Eof { node: name, start });
        });
        let stdin = child.stdin.take();
        self.nodes.insert(
            node.into(),
            Node {
                child,
                stdin,
                start,
                expect: Expect::Running,
                stderr,
                status: None,
            },
        );
        self.wait_for(&format!("{node} READY"), READY, |seen| {
            matches!(seen, Seen::Event(Event { node: n, kind: Kind::Ready { .. }, .. }) if n == node)
        })
        .unwrap();
    }

    fn node(&mut self, node: &str) -> &mut Node {
        self.nodes
            .get_mut(node)
            .unwrap_or_else(|| panic!("no node {node}"))
    }

    /// Send `command` to `node`; `exit` makes exit 0 the end it is expected to have.
    fn command(&mut self, node: &str, command: &str) {
        let target = self.node(node);
        if command == "exit" {
            target.expect = Expect::Exit;
        }
        // A node that ended already fails the next wait, which says how.
        if let Some(stdin) = target.stdin.as_mut() {
            let _ = writeln!(stdin, "{command}");
        }
    }

    /// Send `signal` to `node`, after making it the end the node is expected to have: else the
    /// reader thread's end of file could be judged first.
    fn kill(&mut self, node: &str, signal: Signal) {
        let target = self.node(node);
        target.expect = Expect::Signal(signal.as_raw());
        let pid = Pid::from_raw(target.child.id() as i32).unwrap();
        rustix::process::kill_process(pid, signal).unwrap();
    }

    fn failure(&self, node: Option<&str>, text: String) -> Failure {
        let mut text = format!("{text}\nrun directory: {}", self.dir.display());
        if let Some(target) = node.and_then(|node| self.nodes.get(node)) {
            text += &format!(
                "\n--- {} (last {STDERR_TAIL} lines) ---\n{}",
                target.stderr.display(),
                tail(&target.stderr, STDERR_TAIL)
            );
        }
        Failure(text)
    }

    fn log(&mut self, line: &str) {
        let _ = writeln!(self.log, "{line}");
    }

    /// The first thing seen that `matches`, from the backlog or the nodes, within `within`.
    fn wait_for(
        &mut self,
        what: &str,
        within: Duration,
        mut matches: impl FnMut(&Seen) -> bool,
    ) -> Result<Seen, Failure> {
        if let Some(at) = self.backlog.iter().position(&mut matches) {
            return Ok(self.backlog.remove(at).unwrap());
        }
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.recv.recv_timeout(left) else {
                return Err(self.failure(None, format!("{what}: not within {within:?}")));
            };
            let seen = match message {
                Message::Line(event) => {
                    self.log(&event.line());
                    if let Kind::Fault { .. } | Kind::Violation { .. } = event.kind {
                        let node = event.node.clone();
                        return Err(self.failure(Some(&node), format!("{what}: {}", event.line())));
                    }
                    match event.kind {
                        Kind::Count { .. } => {
                            self.counts.insert(event.node.clone(), event.clone());
                        }
                        Kind::Published { .. } => {
                            self.published.insert(event.node.clone(), event.clone());
                        }
                        _ => {}
                    }
                    Seen::Event(event)
                }
                Message::Garbled { node, line } => {
                    self.log(&format!("{node} garbled: {line}"));
                    return Err(
                        self.failure(Some(&node), format!("{what}: {node} printed {line:?}"))
                    );
                }
                Message::Eof { node, start } => {
                    let target = self.node(&node);
                    if target.start != start {
                        continue;
                    }
                    let status = target.child.wait().unwrap();
                    target.status = Some(status);
                    target.stdin = None;
                    let expect = target.expect;
                    self.log(&format!("{} {node} ended {status}", now_ns()));
                    if !expect.holds(status) {
                        return Err(self.failure(
                            Some(&node),
                            format!("{what}: {node} ended with {status}, expected {expect:?}"),
                        ));
                    }
                    Seen::Ended { node }
                }
            };
            if matches(&seen) {
                return Ok(seen);
            }
            self.backlog.push_back(seen);
        }
    }

    /// `node`'s `ENDED` of `topic` with `reason`.
    fn ended(&mut self, node: &str, reason: &str, within: Duration) -> Result<Event, Failure> {
        let seen = self.wait_for(&format!("{node} ENDED {reason}"), within, |seen| {
            matches!(seen, Seen::Event(Event { node: n, kind: Kind::Ended { reason: r, .. }, .. })
                if n == node && r == reason)
        })?;
        let Seen::Event(event) = seen else {
            unreachable!()
        };
        Ok(event)
    }

    /// `exit` to every node still running, then each one's end.
    fn finish(&mut self) -> Result<(), Failure> {
        let running: Vec<String> = self
            .nodes
            .iter()
            .filter(|(_, node)| node.status.is_none())
            .map(|(name, _)| name.clone())
            .collect();
        for node in &running {
            if self.nodes[node].expect == Expect::Running {
                self.command(node, "exit");
            }
        }
        for node in &running {
            self.wait_for(
                &format!("{node} to end"),
                EXIT,
                |seen| matches!(seen, Seen::Ended { node: n, .. } if n == node),
            )?;
        }
        Ok(())
    }

    /// `(received, lagged, first_seq, last_seq)` of `node`'s latest `COUNT`.
    fn count(&self, node: &str, incarnation: Incarnation) -> judge::Count {
        match self.counts.get(node).map(|event| &event.kind) {
            Some(&Kind::Count {
                received,
                lagged,
                first_seq,
                last_seq,
                ..
            }) => judge::Count {
                incarnation,
                received,
                lagged,
                first_seq,
                last_seq,
            },
            _ => panic!("no COUNT from {node}"),
        }
    }

    /// The incarnation and last seq of `node`'s `PUBLISHED`.
    fn published(&self, node: &str) -> (Incarnation, u64) {
        match self.published.get(node).map(|event| &event.kind) {
            Some(&Kind::Published {
                incarnation,
                last_seq,
                ..
            }) => (incarnation, last_seq),
            _ => panic!("no PUBLISHED from {node}"),
        }
    }
}

impl Drop for Fleet {
    fn drop(&mut self) {
        for node in self.nodes.values_mut() {
            if node.status.is_none() {
                // Ends a stopped process too.
                let _ = node.child.kill();
                let _ = node.child.wait();
            }
        }
        let keep = std::env::var_os("FLEET_KEEP").is_some_and(|keep| keep == "1");
        if std::thread::panicking() || keep {
            eprintln!("topic fleet: kept {}", self.dir.display());
        } else {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

const SUBSCRIBERS: [&str; 2] = ["sub-1", "sub-2"];

/// A publisher at `hz` and [`SUBSCRIBERS`] of its topic, each ready.
fn one_topic(tag: &str, hz: u32) -> Fleet {
    let mut fleet = Fleet::new(tag);
    let topic = fleet.name("frames");
    fleet.spawn("pub", &["--pub", &format!("{topic}:{hz}")]);
    for sub in SUBSCRIBERS {
        fleet.spawn(sub, &["--sub", &topic]);
    }
    fleet
}

fn since(start_ns: u64, event: &Event) -> Duration {
    Duration::from_nanos(event.at_ns.saturating_sub(start_ns))
}

#[test]
#[ignore = "requires a binder device and a permissive service manager"]
fn steady_state_one_topic() {
    let mut fleet = one_topic("steady", 100);
    std::thread::sleep(Duration::from_secs(3));
    let exited = now_ns();
    fleet.command("pub", "exit");
    let reason = end_reason(Injected::Close, false);
    for sub in SUBSCRIBERS {
        let ended = fleet.ended(sub, reason, END * 2).unwrap();
        assert!(
            since(exited, &ended) < END,
            "{sub}: {:?}",
            since(exited, &ended)
        );
    }
    fleet.finish().unwrap();
    let (incarnation, last_published) = fleet.published("pub");
    for sub in SUBSCRIBERS {
        let count = fleet.count(sub, incarnation);
        check_end(
            Mode::Counted,
            false,
            Injected::Close,
            reason,
            Some(&count),
            Some(last_published),
        )
        .unwrap_or_else(|text| panic!("{sub}: {text}"));
        assert!(count.received >= 100, "{sub}: {count:?}");
    }
}

#[test]
#[ignore = "requires a binder device and a permissive service manager"]
fn a_killed_publisher_ends_its_subscribers() {
    let mut fleet = one_topic("killed", 100);
    for sub in SUBSCRIBERS {
        fleet
            .wait_for(&format!("{sub} CONNECTED"), READY, |seen| {
                matches!(seen, Seen::Event(Event { node, kind: Kind::Connected { .. }, .. }) if node == sub)
            })
            .unwrap();
    }
    let killed = now_ns();
    fleet.kill("pub", Signal::KILL);
    for sub in SUBSCRIBERS {
        let ended = fleet
            .ended(sub, end_reason(Injected::Kill, false), END * 2)
            .unwrap();
        assert!(
            since(killed, &ended) < END,
            "{sub}: {:?}",
            since(killed, &ended)
        );
    }
    fleet.finish().unwrap();
}

/// The controller's own check: a node that dies unasked fails the next wait, with its stderr.
#[test]
#[ignore = "requires a binder device and a permissive service manager"]
fn a_node_that_panics_fails_the_run() {
    let mut fleet = one_topic("panics", 100);
    fleet.command("sub-1", "crash");
    let failure = fleet
        .wait_for("nothing", READY, |_| false)
        .expect_err("sub-1's panic to fail the wait");
    assert!(
        failure.0.contains("sub-1 ended with")
            && failure.0.contains("crash, as the controller asked"),
        "{failure:?}"
    );
    fleet.finish().unwrap();
}
