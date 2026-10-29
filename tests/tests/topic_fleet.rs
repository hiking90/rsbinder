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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rsbinder::{hub, ProcessState};
use rsbinder_topic::aidl::ITopicRegistry::SERVICE_NAME;
use rustix::process::{Pid, Signal};
use tests::topic_fleet::event::{Event, Kind, Subscribed};
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

fn is_stats(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::SubStats { .. }
            | Kind::PubStats { .. }
            | Kind::RegStats { .. }
            | Kind::Reported { .. }
            | Kind::ReportSeen { .. }
            | Kind::StatsDone
    )
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

    /// The next thing seen from the nodes before `deadline`, `None` at it. A `COUNT` is kept as
    /// its node's latest and not returned: no wait is for one.
    fn pump(&mut self, what: &str, deadline: Instant) -> Result<Option<Seen>, Failure> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.recv.recv_timeout(left) else {
                return Ok(None);
            };
            match message {
                Message::Line(event) => {
                    self.log(&event.line());
                    match event.kind {
                        Kind::Fault { .. } | Kind::Violation { .. } => {
                            let node = event.node.clone();
                            return Err(
                                self.failure(Some(&node), format!("{what}: {}", event.line()))
                            );
                        }
                        Kind::Count { .. } => {
                            self.counts.insert(event.node.clone(), event);
                            continue;
                        }
                        Kind::Published { .. } => {
                            self.published.insert(event.node.clone(), event.clone());
                        }
                        _ => {}
                    }
                    return Ok(Some(Seen::Event(event)));
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
                    return Ok(Some(Seen::Ended { node }));
                }
            }
        }
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
        while let Some(seen) = self.pump(what, deadline)? {
            if matches(&seen) {
                return Ok(seen);
            }
            self.backlog.push_back(seen);
        }
        Err(self.failure(None, format!("{what}: not within {within:?}")))
    }

    /// Read what the nodes print for `duration`, failing as a wait does; what it reads is not
    /// kept for a later wait.
    fn watch(&mut self, what: &str, duration: Duration) -> Result<(), Failure> {
        let deadline = Instant::now() + duration;
        while self.pump(what, deadline)?.is_some() {}
        Ok(())
    }

    /// `node`'s answer to `stats`: every line up to its `STATSDONE`.
    fn stats(&mut self, node: &str) -> Result<Vec<Kind>, Failure> {
        self.command(node, "stats");
        let mut kinds = Vec::new();
        loop {
            let seen = self.wait_for(&format!("{node} stats"), READY, |seen| {
                matches!(seen, Seen::Event(Event { node: n, kind, .. }) if n == node && is_stats(kind))
            })?;
            match seen {
                Seen::Event(Event {
                    kind: Kind::StatsDone,
                    ..
                }) => return Ok(kinds),
                Seen::Event(event) => kinds.push(event.kind),
                Seen::Ended { .. } => unreachable!(),
            }
        }
    }

    /// The pid of a running `node`.
    fn pid(&self, node: &str) -> i32 {
        self.nodes[node].child.id() as i32
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
    fleet.spawn("pub", &["--pub", &format!("topic={topic},hz={hz}")]);
    for sub in SUBSCRIBERS {
        fleet.spawn(sub, &["--sub", &format!("topic={topic}")]);
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

/// This process's binder, for the service manager's list of names.
fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        ProcessState::init_default().expect("a binder device");
        ProcessState::start_thread_pool();
    });
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// One subscription the topology makes.
struct Planned {
    node: String,
    topic: String,
}

/// The six topics of plan 15 fleet test §3.1, their producers, a registry host, and
/// subscriber nodes that each hold three subscriptions over the axes of §3.2.
struct Topology {
    /// In start order: a node and its role arguments.
    nodes: Vec<(String, Vec<String>)>,
    subscriptions: Vec<Planned>,
    /// Each publishing node and its topic.
    publishers: Vec<(String, String)>,
    reporters: Vec<String>,
    registry: String,
    /// The names the service manager is expected to hold for this fleet.
    registered: Vec<String>,
    /// T5 and T6.
    status: String,
    named: String,
}

impl Topology {
    fn new(fleet: &Fleet, scale: usize) -> Self {
        let name = |topic: &str| fleet.name(topic);
        let (frames, readings, control) = (name("frames"), name("readings"), name("control"));
        let (fused, status, named) = (name("fused"), name("status"), name("named.reg"));
        let service = name("status.svc");
        let registry = format!("{SERVICE_NAME}.fleet.{}.{}", fleet.tag, std::process::id());
        let report = format!("service={service}");
        let to_status = format!("topic={status},payload=record");
        let mut topology = Self {
            nodes: Vec::new(),
            subscriptions: Vec::new(),
            publishers: Vec::new(),
            reporters: Vec::new(),
            registry: registry.clone(),
            registered: vec![
                frames.clone(),
                readings.clone(),
                control.clone(),
                fused.clone(),
                status.clone(),
                service.clone(),
                registry.clone(),
            ],
            status: status.clone(),
            named: named.clone(),
        };
        topology.node("registry", &[("--registry", format!("service={registry}"))]);
        topology.node(
            "p-status",
            &[("--relay", format!("topic={status},service={service}"))],
        );
        topology
            .publishers
            .push(("p-status".into(), status.clone()));
        let producers = [
            (
                "p-frames",
                "--pub",
                format!("topic={frames},size=1048576,pool,subs=32,hz=10"),
                &frames,
            ),
            (
                "p-readings",
                "--pub",
                format!("topic={readings},payload=record,hz=1000,ring=16384"),
                &readings,
            ),
            (
                "p-control",
                "--pub",
                format!("topic={control},hz=50,bp=1000"),
                &control,
            ),
            (
                "p-named",
                "--pub",
                format!("topic={named},hz=20,via={registry}"),
                &named,
            ),
            (
                "p-fused",
                "--fuse",
                format!("out={fused},frames={frames},readings={readings}"),
                &fused,
            ),
        ];
        for (node, role, spec, topic) in producers {
            topology.node(
                node,
                &[
                    (role, spec),
                    ("--report", report.clone()),
                    ("--sub", to_status.clone()),
                ],
            );
            topology.publishers.push((node.into(), topic.clone()));
            topology.reporters.push(node.into());
            topology.planned(node, &status);
        }
        topology.planned("p-fused", &frames);
        topology.planned("p-fused", &readings);

        // (topic, payload, a pool of bytes, through the registry)
        let topics = [
            (&frames, "bytes", true, false),
            (&readings, "record", false, false),
            (&control, "bytes", false, false),
            (&fused, "record", false, false),
            (&status, "record", false, false),
            (&named, "bytes", false, true),
        ];
        for i in 0..20 * scale {
            let node = format!("s-{i}");
            let mut roles = Vec::new();
            for j in 0..3 {
                let (topic, payload, pool, via_registry) = topics[(i + 2 * j) % topics.len()];
                let s = i * 3 + j;
                let named = via_registry || s % 3 == 1;
                let recv = match s % 5 {
                    1 => "latest",
                    2 => "async",
                    4 if pool && !named => "sample",
                    _ => "recv",
                };
                let fd = s % 2 == 1;
                let start = ["latest", "last", "oldest"][s % 3];
                let mut spec = format!(
                    "topic={topic},payload={payload},api={},recv={recv},wake={},start={start}",
                    if named { "named" } else { "plain" },
                    if fd { "fd" } else { "futex" },
                );
                // At scale 1, one trusted sample and one untrusted (s = 44, 54).
                if recv == "sample" && s % 4 == 0 {
                    spec += ",trust";
                }
                if fd && recv != "async" {
                    spec += ",group=g";
                }
                if via_registry {
                    spec += &format!(",via={registry}");
                }
                roles.push(("--sub", spec));
                topology.planned(&node, topic);
            }
            topology.node(&node, &roles);
        }
        topology
    }

    fn node(&mut self, node: &str, roles: &[(&str, String)]) {
        let args = roles
            .iter()
            .flat_map(|(flag, spec)| [flag.to_string(), spec.clone()])
            .collect();
        self.nodes.push((node.into(), args));
    }

    fn planned(&mut self, node: &str, topic: &str) {
        self.subscriptions.push(Planned {
            node: node.into(),
            topic: topic.into(),
        });
    }

    /// The nodes that publish, report or host the registry: asked first at a stop point.
    fn is_producer(node: &str) -> bool {
        !node.starts_with("s-")
    }
}

/// What a stop point found of each process: memfds open, memfd mappings, threads outside the
/// binder and tokio pools; and the service manager's open files.
#[derive(Debug, PartialEq)]
struct Snapshot {
    processes: BTreeMap<String, (usize, usize, usize)>,
    hub_fds: Option<usize>,
}

fn memfd_counts(pid: i32) -> (usize, usize, usize) {
    let memfds = fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|fds| {
            fds.filter_map(Result::ok)
                .filter(|fd| {
                    fs::read_link(fd.path())
                        .is_ok_and(|target| target.to_string_lossy().starts_with("/memfd:"))
                })
                .count()
        })
        .unwrap_or(0);
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))
        .map(|maps| maps.lines().filter(|line| line.contains("/memfd:")).count())
        .unwrap_or(0);
    // The binder pool and tokio's blocking pool start threads on demand and the latter ends idle
    // ones, so neither count is fixed; what a leak leaves (a connect or retry thread of
    // rsbinder-topic, a role's) is named otherwise.
    let threads = fs::read_dir(format!("/proc/{pid}/task")).map_or(0, |tasks| {
        tasks
            .filter_map(Result::ok)
            .filter(|task| {
                let comm = fs::read_to_string(task.path().join("comm")).unwrap_or_default();
                !comm.starts_with("binder:") && !comm.starts_with("tokio-")
            })
            .count()
    });
    (memfds, maps, threads)
}

/// The open files of the `rsb_hub` this machine runs, if one.
fn hub_fds() -> Option<usize> {
    let hub = fs::read_dir("/proc")
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| {
            fs::read_to_string(entry.path().join("comm")).is_ok_and(|comm| comm.trim() == "rsb_hub")
        })?;
    Some(fs::read_dir(hub.path().join("fd")).ok()?.count())
}

/// The checks of plan 15 fleet test §5.3 at a moment no fault is being injected.
///
/// Producers are asked before subscribers, and the subscribers a second after, so that a
/// subscriber's counts, read later, are at least what its publisher saw, and the last report
/// a reporter had sent has reached every T5 subscriber.
fn stop_point(fleet: &mut Fleet, topology: &Topology) -> Result<Snapshot, Failure> {
    let mut wrong = Vec::new();
    let mut published: HashMap<String, (u64, Vec<Subscribed>)> = HashMap::new();
    let mut reported: Vec<(String, u64)> = Vec::new();
    let mut registry = None;
    let (producers, subscribers): (Vec<&String>, Vec<&String>) = topology
        .nodes
        .iter()
        .map(|(node, _)| node)
        .partition(|node| Topology::is_producer(node));
    for node in &producers {
        for kind in fleet.stats(node)? {
            match kind {
                Kind::PubStats {
                    topic,
                    evicted,
                    subscribers,
                    ..
                } => {
                    published.insert(topic, (evicted, subscribers));
                }
                Kind::Reported { sent, .. } => reported.push((node.to_string(), sent)),
                Kind::RegStats {
                    topics, watchers, ..
                } => registry = Some((topics, watchers)),
                _ => {}
            }
        }
    }
    fleet.watch("a stop point", Duration::from_secs(1))?;
    // (node, topic) → node_lagged, and each T5 subscription's latest report per reporter run.
    let mut lagged: HashMap<(String, String), (u64, u64)> = HashMap::new();
    let mut seen: HashMap<(String, Incarnation), Vec<u64>> = HashMap::new();
    for node in producers.iter().chain(&subscribers) {
        for kind in fleet.stats(node)? {
            match kind {
                Kind::SubStats {
                    topic,
                    received,
                    lost,
                    decode_errors,
                    connections,
                    node_received,
                    node_lagged,
                    connected,
                } => {
                    if (received, lost, decode_errors) != (node_received, node_lagged, 0)
                        || !connected
                    {
                        wrong.push(format!(
                            "{node} {topic}: stats received {received} lost {lost} decode errors \
                             {decode_errors} connected {connected}; the node counted {node_received} \
                             received, {node_lagged} lagged"
                        ));
                    }
                    lagged.insert((node.to_string(), topic), (node_lagged, connections));
                }
                Kind::ReportSeen {
                    reporter,
                    incarnation,
                    seq,
                    ..
                } => seen.entry((reporter, incarnation)).or_default().push(seq),
                _ => {}
            }
        }
    }

    let pid_node: HashMap<i32, String> = topology
        .nodes
        .iter()
        .map(|(node, _)| (fleet.pid(node), node.clone()))
        .collect();
    for (publisher, topic) in &topology.publishers {
        let Some((evicted, subscribed)) = published.get(topic) else {
            wrong.push(format!("{publisher}: no PSTATS of {topic}"));
            continue;
        };
        if *evicted != 0 {
            wrong.push(format!("{publisher} {topic}: {evicted} evicted"));
        }
        let mut expected: Vec<i32> = topology
            .subscriptions
            .iter()
            .filter(|planned| planned.topic == *topic)
            .map(|planned| fleet.pid(&planned.node))
            .collect();
        let mut held: Vec<i32> = subscribed.iter().map(|info| info.pid).collect();
        expected.sort_unstable();
        held.sort_unstable();
        if held != expected {
            wrong.push(format!(
                "{publisher} {topic}: subscriptions of pids {held:?}, expected {expected:?}"
            ));
        }
        for info in subscribed {
            let node = pid_node.get(&info.pid).cloned().unwrap_or_default();
            match lagged.get(&(node.clone(), topic.clone())) {
                Some(&(node_lagged, 1)) if info.lost > node_lagged => wrong.push(format!(
                    "{publisher} {topic}: {node} reported {} lost, counted {node_lagged} later",
                    info.lost
                )),
                Some(_) => {}
                None => wrong.push(format!(
                    "{publisher} {topic}: {node} ({}) has no SSTATS",
                    info.pid
                )),
            }
        }
    }

    let watchers = topology
        .subscriptions
        .iter()
        .filter(|planned| planned.topic == topology.named)
        .count();
    if registry != Some((1, watchers)) {
        wrong.push(format!(
            "registry: (topics, watchers) {registry:?}, expected (1, {watchers})"
        ));
    }

    let status_subscriptions = topology
        .subscriptions
        .iter()
        .filter(|planned| planned.topic == topology.status)
        .count();
    for (reporter, sent) in &reported {
        let Some(last) = sent.checked_sub(1) else {
            wrong.push(format!("{reporter}: no report sent"));
            continue;
        };
        let incarnation = Incarnation {
            pid: fleet.pid(reporter) as u32,
            start: fleet.starts[reporter.as_str()],
        };
        let seqs = seen
            .get(&(reporter.clone(), incarnation))
            .cloned()
            .unwrap_or_default();
        if seqs.len() != status_subscriptions || seqs.iter().any(|&seq| seq < last) {
            wrong.push(format!(
                "{reporter}: report {last} sent, the {status_subscriptions} T5 subscriptions \
                 have {seqs:?}"
            ));
        }
    }

    let prefix = format!("rsb.fleet.{}.", fleet.tag);
    let mut listed: Vec<String> = hub::list_services(hub::DUMP_FLAG_PRIORITY_ALL)
        .into_iter()
        .filter(|name| name.starts_with(&prefix) || *name == topology.registry)
        .collect();
    let mut registered = topology.registered.clone();
    listed.sort();
    registered.sort();
    if listed != registered {
        wrong.push(format!(
            "service manager: {listed:?}, expected {registered:?}"
        ));
    }

    if !wrong.is_empty() {
        return Err(fleet.failure(None, format!("stop point:\n  {}", wrong.join("\n  "))));
    }
    Ok(Snapshot {
        processes: topology
            .nodes
            .iter()
            .map(|(node, _)| (node.clone(), memfd_counts(fleet.pid(node))))
            .collect(),
        hub_fds: hub_fds(),
    })
}

/// Plan 15 fleet test F2: the six topics run with no fault; at each stop point every check of
/// §5.3 holds, and no process holds more memfds, mappings or threads than at the first.
#[test]
#[ignore = "requires a binder device and a permissive service manager"]
fn steady_state_fleet() {
    init();
    let scale = env_number("FLEET_SCALE", 1) as usize;
    let steady = Duration::from_secs(env_number("FLEET_STEADY_SECS", 30));
    let mut fleet = Fleet::new("fleet");
    let topology = Topology::new(&fleet, scale);
    for (node, args) in &topology.nodes {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        fleet.spawn(node, &args);
    }
    let mut pending: HashSet<(String, String)> = topology
        .subscriptions
        .iter()
        .map(|planned| (planned.node.clone(), planned.topic.clone()))
        .collect();
    while !pending.is_empty() {
        let seen = fleet
            .wait_for("every subscription CONNECTED", Duration::from_secs(20), |seen| {
                matches!(seen, Seen::Event(Event { node, kind: Kind::Connected { topic, .. }, .. })
                    if pending.contains(&(node.clone(), topic.clone())))
            })
            .unwrap_or_else(|failure| panic!("{failure:?}\nnot connected: {pending:?}"));
        if let Seen::Event(Event {
            node,
            kind: Kind::Connected { topic, .. },
            ..
        }) = seen
        {
            pending.remove(&(node, topic));
        }
    }
    fleet.watch("warm-up", Duration::from_secs(5)).unwrap();
    let first = stop_point(&mut fleet, &topology).unwrap();
    fleet.watch("steady state", steady).unwrap();
    let second = stop_point(&mut fleet, &topology).unwrap();
    assert_eq!(
        first, second,
        "memfds, mappings, threads or hub files changed"
    );
    fleet.finish().unwrap();
}
