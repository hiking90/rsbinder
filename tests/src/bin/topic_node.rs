// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A node of the topic fleet (plan 15 fleet test), driven by `tests/topic_fleet.rs`.
//!
//! ```text
//! topic_node --node NAME --start N ROLE
//!   --pub TOPIC:HZ   advertise TOPIC and publish a record HZ times a second
//!   --sub TOPIC      subscribe to TOPIC as registered now, `recv` with `Wake::Futex`
//! ```
//!
//! `N` is the controller's start count of the node, half of the incarnation a publisher stamps
//! on its records. The node prints one event line per change on stdout
//! (`tests::topic_fleet::event`) and reads commands from stdin:
//!
//! - `exit`: end every wait, drop the publisher (its end record), print the last counts, exit 0.
//!   An end of stdin does the same.
//! - `crash`: panic on the main thread, for the controller's own test.
//!
//! The node dies with the controller (`PR_SET_PDEATHSIG`), which it checks by
//! `FLEET_CONTROLLER_PID`, and leaves no core file unless `FLEET_CORE=1`.

#[cfg(any(target_os = "linux", target_os = "android"))]
mod node {
    use std::io::{BufRead, Write};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use rsbinder::ProcessState;
    use rsbinder_topic::{
        Bytes, Publisher, PublisherPolicy, RecvError, Subscriber, SubscriberPolicy,
    };
    use rustix::process::{Pid, Resource, Rlimit, Signal};
    use tests::topic_fleet::event::{closed_reason, Event, Kind};
    use tests::topic_fleet::record::{Incarnation, Stamp};
    use tests::topic_fleet::transport;

    const COUNT_EVERY: Duration = Duration::from_secs(1);

    enum Role {
        Pub { topic: String, hz: u32 },
        Sub { topic: String },
    }

    struct Args {
        node: String,
        start: u32,
        role: Role,
    }

    fn args() -> Args {
        const USAGE: &str = "topic_node --node NAME --start N (--pub TOPIC:HZ | --sub TOPIC)";
        let mut args = std::env::args().skip(1);
        let (mut node, mut start, mut role) = (None, None, None);
        while let Some(flag) = args.next() {
            let value = args.next().expect(USAGE);
            match flag.as_str() {
                "--node" => node = Some(value),
                "--start" => start = Some(value.parse().expect(USAGE)),
                "--pub" => {
                    let (topic, hz) = value.rsplit_once(':').expect(USAGE);
                    let hz = hz.parse().ok().filter(|&hz| hz > 0).expect(USAGE);
                    role = Some(Role::Pub {
                        topic: topic.into(),
                        hz,
                    });
                }
                "--sub" => role = Some(Role::Sub { topic: value }),
                _ => panic!("{USAGE}"),
            }
        }
        Args {
            node: node.expect(USAGE),
            start: start.expect(USAGE),
            role: role.expect(USAGE),
        }
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

    /// The event lines of one node, written by its writer thread.
    #[derive(Clone)]
    struct Out {
        node: String,
        events: mpsc::Sender<Event>,
    }

    impl Out {
        fn send(&self, kind: Kind) {
            let _ = self.events.send(Event::new(&self.node, kind));
        }
    }

    /// One line per event, flushed at once: a role thread never waits on the pipe.
    fn write_lines(events: mpsc::Receiver<Event>) {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        for event in events {
            if writeln!(stdout, "{}", event.line())
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return;
            }
        }
    }

    /// A role's thread and how to make it end.
    struct Running {
        stop: Box<dyn FnOnce()>,
        thread: JoinHandle<()>,
    }

    impl Running {
        fn stop(self) {
            (self.stop)();
            let _ = self.thread.join();
        }
    }

    fn publish(
        out: &Out,
        topic: String,
        hz: u32,
        incarnation: Incarnation,
    ) -> Result<Running, String> {
        let publisher: Publisher<Bytes> = transport::advertise(&topic, &PublisherPolicy::default())
            .map_err(|status| format!("advertise {topic}: {status}"))?;
        let (stop, stopped) = mpsc::channel::<()>();
        let period = Duration::from_secs(1) / hz;
        let out = out.clone();
        let thread = thread::spawn(move || {
            let mut next = Instant::now();
            for seq in 0.. {
                let stamp = Stamp { incarnation, seq };
                if let Err(err) = publisher.publish(&stamp.encode()) {
                    out.send(Kind::Fault {
                        text: format!("publish {topic} seq {seq}: {err}"),
                    });
                    return;
                }
                next += period;
                match stopped.recv_timeout(next.saturating_duration_since(Instant::now())) {
                    Err(RecvTimeoutError::Timeout) => {}
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            // Writes the end record.
            drop(publisher);
        });
        Ok(Running {
            stop: Box::new(move || drop(stop)),
            thread,
        })
    }

    /// What one connection has received, for `CONNECTED` and `COUNT`.
    struct Connection {
        topic: String,
        incarnation: Option<Incarnation>,
        received: u64,
        lagged: u64,
        first_seq: u64,
        last_seq: u64,
        counted: Instant,
    }

    impl Connection {
        fn new(topic: String) -> Self {
            Self {
                topic,
                incarnation: None,
                received: 0,
                lagged: 0,
                first_seq: 0,
                last_seq: 0,
                counted: Instant::now(),
            }
        }

        fn violation(&self, out: &Out, text: String) {
            out.send(Kind::Violation {
                topic: self.topic.clone(),
                text,
            });
        }

        fn record(&mut self, out: &Out, stamp: Stamp) {
            match self.incarnation {
                None => {
                    let Some(first_seq) = stamp.seq.checked_sub(self.lagged) else {
                        self.violation(
                            out,
                            format!("{} lagged before seq {}", self.lagged, stamp.seq),
                        );
                        return;
                    };
                    self.incarnation = Some(stamp.incarnation);
                    self.first_seq = first_seq;
                    out.send(Kind::Connected {
                        topic: self.topic.clone(),
                        incarnation: stamp.incarnation,
                        first_seq: stamp.seq,
                    });
                }
                Some(incarnation) if incarnation != stamp.incarnation => {
                    self.violation(out, format!("{} within {incarnation}", stamp.incarnation));
                    return;
                }
                Some(_) if stamp.seq <= self.last_seq => {
                    self.violation(out, format!("seq {} after {}", stamp.seq, self.last_seq));
                    return;
                }
                Some(_) => {}
            }
            self.received += 1;
            self.last_seq = stamp.seq;
        }

        fn count(&mut self, out: &Out) {
            self.counted = Instant::now();
            if self.incarnation.is_some() {
                out.send(Kind::Count {
                    topic: self.topic.clone(),
                    received: self.received,
                    lagged: self.lagged,
                    first_seq: self.first_seq,
                    last_seq: self.last_seq,
                });
            }
        }

        fn count_if_due(&mut self, out: &Out) {
            if self.counted.elapsed() >= COUNT_EVERY {
                self.count(out);
            }
        }
    }

    fn subscribe(out: &Out, topic: String) -> Result<Running, String> {
        let mut subscriber: Subscriber<Bytes> =
            transport::subscribe(&topic, &SubscriberPolicy::default())
                .map_err(|status| format!("subscribe {topic}: {status}"))?;
        let interrupter = subscriber.interrupter();
        let out = out.clone();
        let thread = thread::spawn(move || {
            let mut connection = Connection::new(topic);
            loop {
                match subscriber.recv() {
                    Ok(Bytes(bytes)) => match Stamp::decode(&bytes) {
                        Some(stamp) => connection.record(&out, stamp),
                        None => {
                            connection.violation(&out, format!("a record of {} bytes", bytes.len()))
                        }
                    },
                    Err(RecvError::Lagged(records)) => connection.lagged += records,
                    Err(RecvError::Closed(closed)) => {
                        connection.count(&out);
                        out.send(Kind::Ended {
                            topic: connection.topic.clone(),
                            reason: closed_reason(&closed).into(),
                        });
                        return;
                    }
                    Err(RecvError::Cancelled) => {
                        connection.count(&out);
                        return;
                    }
                    Err(err) => {
                        out.send(Kind::Fault {
                            text: format!("recv {}: {err}", connection.topic),
                        });
                        return;
                    }
                }
                connection.count_if_due(&out);
            }
        });
        Ok(Running {
            stop: Box::new(move || interrupter.interrupt()),
            thread,
        })
    }

    /// Commands until `exit` or the end of stdin.
    fn commands(out: &Out) {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                return;
            };
            match line.trim() {
                "exit" => return,
                "crash" => panic!("crash, as the controller asked"),
                other => out.send(Kind::Fault {
                    text: format!("unknown command {other:?}"),
                }),
            }
        }
    }

    pub fn main() {
        let args = args();
        limit_core();
        die_with_controller();
        ProcessState::init_default().expect("a binder device");
        ProcessState::start_thread_pool();

        let (events, lines) = mpsc::channel();
        let writer = thread::spawn(move || write_lines(lines));
        let out = Out {
            node: args.node,
            events,
        };
        let incarnation = Incarnation {
            pid: std::process::id(),
            start: args.start,
        };
        let running = match args.role {
            Role::Pub { topic, hz } => publish(&out, topic, hz, incarnation),
            Role::Sub { topic } => subscribe(&out, topic),
        };
        let running = match running {
            Ok(running) => running,
            Err(text) => {
                out.send(Kind::Fault { text });
                drop(out);
                let _ = writer.join();
                std::process::exit(1);
            }
        };
        out.send(Kind::Ready {
            pid: std::process::id(),
        });

        commands(&out);
        running.stop();
        drop(out);
        let _ = writer.join();
    }
}

fn main() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    node::main();
}
