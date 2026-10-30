// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A node of the topic fleet (plan 15 fleet test), driven by `tests/topic_fleet.rs`.
//!
//! ```text
//! topic_node --node NAME --start N ROLE...
//! ```
//!
//! The roles are `tests::topic_fleet::spec`'s; one process may hold any number of them, as a
//! daemon that publishes one topic and subscribes to others does. `N` is the controller's start
//! count of the node, half of the incarnation its publishers stamp on their records. The node
//! prints one event line per change on stdout (`tests::topic_fleet::event`) and reads commands
//! from stdin:
//!
//! - `stats`: each role's statistics, then `STATSDONE`.
//! - `exit`: end every role (a publisher writes its end record and prints `PUBLISHED`, a
//!   subscription its last counts), exit 0. An end of stdin does the same.
//! - `crash`: panic on the main thread, for the controller's own test.
//!
//! The node dies with the controller (`PR_SET_PDEATHSIG`), which it checks by
//! `FLEET_CONTROLLER_PID`, and leaves no core file unless `FLEET_CORE=1`.

#[cfg(any(target_os = "linux", target_os = "android"))]
mod publish;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod relay;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod subscribe;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod node {
    use std::io::{BufRead, Write};
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use rsbinder::ProcessState;
    use rustix::process::{Pid, Resource, Rlimit, Signal};
    use tests::topic_fleet::event::{Event, Kind};
    use tests::topic_fleet::record::Incarnation;
    use tests::topic_fleet::spec::Role;

    /// How long `stats` waits for one role's answer.
    const STATS_WAIT: Duration = Duration::from_secs(5);

    /// The event lines of one node, written by its writer thread until [`close`](Self::close).
    ///
    /// The writer ends at `close`, not when every copy is dropped: a binder object the node
    /// serves holds one for as long as another process holds the binder.
    #[derive(Clone)]
    pub struct Out {
        node: String,
        events: mpsc::Sender<Option<Event>>,
    }

    impl Out {
        pub fn node(&self) -> &str {
            &self.node
        }

        pub fn send(&self, kind: Kind) {
            let _ = self.events.send(Some(Event::new(&self.node, kind)));
        }

        /// The writer writes what came before this, then ends.
        fn close(self, writer: JoinHandle<()>) {
            let _ = self.events.send(None);
            let _ = writer.join();
        }

        pub fn fault(&self, text: String) {
            self.send(Kind::Fault { text });
        }
    }

    /// What the main thread asks of a role's thread.
    pub enum Request {
        /// Answer with the role's statistics.
        Stats(mpsc::Sender<Vec<Kind>>),
        /// A command of the controller, and the topic it named: a group holds several.
        Command(Command, String),
        Stop,
    }

    /// `<command> TOPIC` on stdin, sent to the roles of `TOPIC`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Command {
        /// A publisher: close the topic and print `PUBLISHED`, then publish nothing more (X2).
        Close,
        /// A publisher: register its binder with the service manager again (X3b, X10).
        Readvertise,
        /// A publisher made with `prepub`: start publishing (X1).
        Resume,
        /// A `recv_sample` subscription: receive a sample and keep it until killed (X7).
        HoldSample,
        /// A role found through a registry: make it again through the registry's host now
        /// registered (plan 15 D8; X9, X14).
        Rebind,
    }

    impl Command {
        fn parse(word: &str) -> Option<Self> {
            Some(match word {
                "close" => Self::Close,
                "readvertise" => Self::Readvertise,
                "resume" => Self::Resume,
                "hold-sample" => Self::HoldSample,
                "rebind" => Self::Rebind,
                _ => return None,
            })
        }
    }

    /// A role's threads, and how to reach them.
    pub struct Running {
        pub requests: mpsc::Sender<Request>,
        /// Makes the threads look at their requests: interrupts the waits of a subscription.
        pub wake: Box<dyn Fn() + Send>,
        pub threads: Vec<JoinHandle<()>>,
        /// The topics a command names to reach this role: a group's every subscription's.
        pub topics: Vec<String>,
    }

    impl Running {
        /// A role whose one thread waits on its requests alone.
        pub fn thread(requests: mpsc::Sender<Request>, thread: JoinHandle<()>) -> Self {
            Self {
                requests,
                wake: Box::new(|| {}),
                threads: vec![thread],
                topics: Vec::new(),
            }
        }

        pub fn for_topic(mut self, topic: &str) -> Self {
            self.topics.push(topic.into());
            self
        }

        /// Send `command` if this role is of `topic`.
        fn command(&self, topic: &str, command: Command) -> bool {
            if !self.topics.iter().any(|t| t == topic) {
                return false;
            }
            let _ = self.requests.send(Request::Command(command, topic.into()));
            (self.wake)();
            true
        }

        fn stats(&self) -> Vec<Kind> {
            let (answer, answered) = mpsc::channel();
            if self.requests.send(Request::Stats(answer)).is_err() {
                return Vec::new();
            }
            (self.wake)();
            answered.recv_timeout(STATS_WAIT).unwrap_or_default()
        }

        fn stop(self) {
            let _ = self.requests.send(Request::Stop);
            (self.wake)();
            for thread in self.threads {
                let _ = thread.join();
            }
        }
    }

    /// Whether `requests` so far held a `Stop`: each `Stats` is answered with `stats` and each
    /// `Command` handed to `command`.
    pub fn serve(
        requests: &mpsc::Receiver<Request>,
        mut stats: impl FnMut() -> Vec<Kind>,
        mut command: impl FnMut(Command, String),
    ) -> bool {
        let mut stop = false;
        while let Ok(request) = requests.try_recv() {
            match request {
                Request::Stats(answer) => {
                    let _ = answer.send(stats());
                }
                Request::Command(order, topic) => command(order, topic),
                Request::Stop => stop = true,
            }
        }
        stop
    }

    fn args() -> (String, u32, Vec<Role>) {
        const USAGE: &str = "topic_node --node NAME --start N ROLE...";
        let mut args = std::env::args().skip(1);
        let (mut node, mut start, mut roles) = (None, None, Vec::new());
        while let Some(flag) = args.next() {
            let value = args.next().expect(USAGE);
            match flag.as_str() {
                "--node" => node = Some(value),
                "--start" => start = Some(value.parse().expect(USAGE)),
                _ => roles.push(
                    Role::parse(&flag, &value)
                        .unwrap_or_else(|err| panic!("{flag} {value}: {err}")),
                ),
            }
        }
        assert!(!roles.is_empty(), "{USAGE}");
        (node.expect(USAGE), start.expect(USAGE), roles)
    }

    /// No core file in the run directory, the node's cwd, unless `FLEET_CORE=1`.
    fn limit_core() {
        if std::env::var_os("FLEET_CORE").is_some_and(|core| core == "1") {
            return;
        }
        let limit = rustix::process::getrlimit(Resource::Core);
        let none = Rlimit {
            current: Some(0),
            maximum: limit.maximum,
        };
        rustix::process::setrlimit(Resource::Core, none).expect("setrlimit(RLIMIT_CORE)");
    }

    /// Killed with the controller; ends at once if the controller died before the `prctl`.
    fn die_with_controller() {
        rustix::process::set_parent_process_death_signal(Some(Signal::KILL))
            .expect("prctl(PR_SET_PDEATHSIG)");
        let Some(controller) = std::env::var("FLEET_CONTROLLER_PID")
            .ok()
            .and_then(|pid| pid.parse::<i32>().ok())
        else {
            return;
        };
        if Pid::as_raw(rustix::process::getppid()) != controller {
            eprintln!("topic_node: controller {controller} is gone");
            std::process::exit(1);
        }
    }

    /// One line per event, flushed at once: a role thread never waits on the pipe.
    fn write_lines(events: mpsc::Receiver<Option<Event>>) {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        while let Ok(Some(event)) = events.recv() {
            if writeln!(stdout, "{}", event.line())
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return;
            }
        }
    }

    fn start(out: &Out, role: Role, incarnation: Incarnation) -> Result<Running, String> {
        match role {
            Role::Pub(spec) => super::publish::start(out, spec, incarnation),
            Role::Sub(spec) => super::subscribe::start(out, vec![spec]),
            Role::Fuse {
                out: topic,
                frames,
                readings,
            } => super::subscribe::fuse(out, topic, frames, readings, incarnation),
            Role::Relay { topic, service } => super::relay::relay(out, topic, service, incarnation),
            Role::Report { service, hz } => Ok(super::relay::report(out, service, hz, incarnation)),
            Role::Registry { service } => super::relay::registry(service),
        }
    }

    /// Subscriptions of one `group` share a thread; the others each have one.
    fn start_all(
        out: &Out,
        roles: Vec<Role>,
        incarnation: Incarnation,
    ) -> Result<Vec<Running>, String> {
        let mut running = Vec::new();
        let mut groups: Vec<(String, Vec<tests::topic_fleet::spec::SubSpec>)> = Vec::new();
        // Registries and publishers first: a node's own subscriptions may need them.
        let (first, rest): (Vec<Role>, Vec<Role>) = roles.into_iter().partition(|role| {
            matches!(
                role,
                Role::Registry { .. } | Role::Pub(_) | Role::Relay { .. }
            )
        });
        for role in first.into_iter().chain(rest) {
            match role {
                Role::Sub(spec) if spec.group.is_some() => {
                    let group = spec.group.clone().unwrap_or_default();
                    match groups.iter_mut().find(|(name, _)| *name == group) {
                        Some((_, specs)) => specs.push(spec),
                        None => groups.push((group, vec![spec])),
                    }
                }
                role => running.push(start(out, role, incarnation)?),
            }
        }
        for (_, specs) in groups {
            running.push(super::subscribe::start(out, specs)?);
        }
        Ok(running)
    }

    /// Commands until `exit` or the end of stdin.
    fn commands(out: &Out, running: &[Running]) {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                return;
            };
            match line.trim() {
                "exit" => return,
                "crash" => panic!("crash, as the controller asked"),
                "stats" => {
                    for role in running {
                        for kind in role.stats() {
                            out.send(kind);
                        }
                    }
                    out.send(Kind::StatsDone);
                }
                other => {
                    let order = other
                        .split_once(' ')
                        .and_then(|(word, topic)| Some((Command::parse(word)?, topic)));
                    let sent = order.is_some_and(|(command, topic)| {
                        running
                            .iter()
                            .filter(|role| role.command(topic, command))
                            .count()
                            > 0
                    });
                    if !sent {
                        out.fault(format!("no role takes {other:?}"));
                    }
                }
            }
        }
    }

    pub fn main() {
        let (node, start, roles) = args();
        limit_core();
        die_with_controller();
        ProcessState::init_default().expect("a binder device");
        ProcessState::start_thread_pool();

        let (events, lines) = mpsc::channel();
        let writer = thread::spawn(move || write_lines(lines));
        let out = Out { node, events };
        let incarnation = Incarnation {
            pid: std::process::id(),
            start,
        };
        let running = match start_all(&out, roles, incarnation) {
            Ok(running) => running,
            Err(text) => {
                out.fault(text);
                out.close(writer);
                std::process::exit(1);
            }
        };
        out.send(Kind::Ready {
            pid: std::process::id(),
        });

        commands(&out, &running);
        for role in running {
            role.stop();
        }
        out.close(writer);
    }
}

fn main() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    node::main();
}
