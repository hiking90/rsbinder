// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `rsb_service` — ask the running service manager what it knows.
//!
//! The Linux counterpart of Android's `service` and `dumpsys -l`: what is
//! registered, who registered it, what is declared but not started, and
//! what a service says about itself when asked to dump. `wait` holds a
//! script until a name is registered.
//!
//! `topic` (feature `topic`) looks at a topic of `rsbinder-topic` from
//! outside its publisher: its status, the rate of its records, the records
//! themselves (`rsbinder_tools::topic_cmd`).
//!
//! Every question goes over binder, as an ordinary client, so `rsb_hub`'s
//! policy applies to `rsb_service` exactly as it does to anything else —
//! running it does not grant a view a normal caller would not have. Where
//! the policy withholds an answer, the tool says so rather than reporting
//! an empty result (see [`report`]); the one deliberate exception is
//! [`check`](cmd_check), because a denied lookup is *designed* to be
//! indistinguishable from "not registered".
//!
//! Composing `service call`'s ability to assemble an arbitrary parcel by
//! hand is out of scope: without an AIDL description at the other end there
//! is nothing to name the arguments, and every question an operator
//! actually arrives with ("what is up? who owns it? why is my client
//! blocked?") is answered above.

use std::io::Write;
use std::process::ExitCode;

use rsbinder::{hub, ExceptionCode, ProcessState, SIBinder, Status, DEFAULT_BINDERFS_PATH};

/// Exit codes, so a script can branch on the answer without parsing text.
mod exit {
    /// The question was answered, and the answer is yes.
    pub const OK: u8 = 0;
    /// Answered no; `test`-style so `rsb_service check foo || start-foo` reads right.
    pub const NO: u8 = 1;
    /// Unanswerable: no hub to ask, or its policy denied the request.
    pub const ERR: u8 = 2;
}

/// rsb_hub's message says why and for which uid, which only it knows; the hint says where to grant.
fn report(what: &str, status: &Status) -> ExitCode {
    eprintln!("rsb_service: {what} failed: {status}");
    if status.exception_code() == ExceptionCode::Security {
        eprintln!(
            "  If the hub's policy is the reason, grant it in the hub's \
             configuration (`rsb_hub --config <DIR>`, default \
             /etc/rsbinder/hub.d) or run as a user the policy allows."
        );
    }
    ExitCode::from(exit::ERR)
}

/// `--priority` values as `listServices` bitmasks; `all` is the default, as in `service list`.
fn dump_priority(spec: &str) -> Option<i32> {
    Some(match spec {
        "all" => hub::DUMP_FLAG_PRIORITY_ALL,
        "default" => hub::DUMP_FLAG_PRIORITY_DEFAULT,
        "critical" => hub::DUMP_FLAG_PRIORITY_CRITICAL,
        "high" => hub::DUMP_FLAG_PRIORITY_HIGH,
        "normal" => hub::DUMP_FLAG_PRIORITY_NORMAL,
        "proto" => hub::DUMP_FLAG_PROTO,
        _ => return None,
    })
}

fn cmd_list(priority: i32) -> ExitCode {
    match hub::try_list_services(priority) {
        Ok(names) => {
            for name in &names {
                println!("{name}");
            }
            ExitCode::from(exit::OK)
        }
        Err(status) => report("listServices", &status),
    }
}

/// `checkService`, not `getService`: a missed `getService` starts a declared service.
fn cmd_check(name: &str) -> ExitCode {
    match hub::check_service(name) {
        Some(binder) => {
            println!("{name}: registered{}", descriptor_suffix(&binder));
            ExitCode::from(exit::OK)
        }
        None => {
            println!("{name}: not registered");
            ExitCode::from(exit::NO)
        }
    }
}

/// No delay a script shows, and five transactions a second to the hub.
const WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// Polls, so the command needs no thread pool for a callback; `start` looks up with `getService`.
fn cmd_wait(name: &str, timeout: Option<std::time::Duration>, start: bool) -> ExitCode {
    let began = std::time::Instant::now();
    for poll in 0u64.. {
        // A lookup answers a denial, and without `start` a hub that is gone, as "not registered";
        // `isDeclared` takes the same `find` and reports both. Once a second is enough.
        if poll % 5 == 0 {
            if let Err(status) = hub::try_is_declared(name) {
                return report("isDeclared", &status);
            }
        }
        let found = if start {
            match hub::try_get_service(name) {
                Ok(found) => found,
                Err(status) => return report("getService", &status),
            }
        } else {
            hub::check_service(name)
        };
        if let Some(binder) = found {
            println!("{name}: registered{}", descriptor_suffix(&binder));
            return ExitCode::from(exit::OK);
        }
        let left = timeout.map(|timeout| timeout.saturating_sub(began.elapsed()));
        if left.is_some_and(|left| left.is_zero()) {
            break;
        }
        std::thread::sleep(left.map_or(WAIT_POLL, |left| left.min(WAIT_POLL)));
    }
    println!("{name}: not registered");
    ExitCode::from(exit::NO)
}

/// Empty only for a binder whose interface descriptor is empty.
fn descriptor_suffix(binder: &SIBinder) -> String {
    let descriptor = binder.descriptor();
    if descriptor.is_empty() {
        String::new()
    } else {
        format!(" ({descriptor})")
    }
}

fn cmd_info() -> ExitCode {
    match hub::try_get_service_debug_info() {
        Ok(infos) => {
            let width = infos.iter().map(|i| i.name.len()).max().unwrap_or(0);
            for info in &infos {
                println!("{:width$}  pid={}", info.name, info.debugPid);
            }
            ExitCode::from(exit::OK)
        }
        Err(status) => report("getServiceDebugInfo", &status),
    }
}

fn cmd_declared(name: &str) -> ExitCode {
    match hub::try_is_declared(name) {
        Ok(true) => {
            println!("{name}: declared");
            ExitCode::from(exit::OK)
        }
        Ok(false) => {
            println!("{name}: not declared");
            ExitCode::from(exit::NO)
        }
        Err(status) => report("isDeclared", &status),
    }
}

fn cmd_instances(iface: &str) -> ExitCode {
    match hub::try_get_declared_instances(iface) {
        Ok(instances) if instances.is_empty() => {
            println!("{iface}: no declared instances");
            ExitCode::from(exit::NO)
        }
        Ok(instances) => {
            for instance in &instances {
                println!("{iface}/{instance}");
            }
            ExitCode::from(exit::OK)
        }
        Err(status) => report("getDeclaredInstances", &status),
    }
}

fn cmd_connection(name: &str) -> ExitCode {
    match hub::try_get_connection_info(name) {
        Ok(Some(info)) => {
            println!("{name}: {}:{}", info.ipAddress, info.port);
            ExitCode::from(exit::OK)
        }
        Ok(None) => {
            println!("{name}: no declared connection info");
            ExitCode::from(exit::NO)
        }
        Err(status) => report("getConnectionInfo", &status),
    }
}

/// The names under `prefix` of the topic registry `rsb_hub` serves with its `topic` feature.
#[cfg(feature = "topic")]
fn cmd_topics(prefix: &str) -> ExitCode {
    let resolver = match rsbinder_topic::registry::RegistryResolver::find() {
        Ok(resolver) => resolver,
        Err(status) => {
            eprintln!(
                "rsb_service: no topic registry: {status} (rsb_hub serves one with its `topic` \
                 feature, to callers its policy lets find it)"
            );
            return ExitCode::from(exit::ERR);
        }
    };
    print_topics(resolver.list(prefix))
}

#[cfg(feature = "topic")]
fn print_topics(
    listed: rsbinder::BinderResult<Vec<rsbinder_topic::registry::TopicListing>>,
) -> ExitCode {
    match listed {
        Ok(mut topics) => {
            topics.sort_by(|a, b| a.name.cmp(&b.name));
            let width = topics
                .iter()
                .map(|t| t.name.len())
                .max()
                .unwrap_or(0)
                .max(4);
            println!(
                "{:width$}  {:>6}  {:>8}  {:>9}  WATCHERS",
                "NAME", "UID", "PID", "AGE"
            );
            for topic in &topics {
                let age = format!("{:.1}s", topic.age.as_secs_f64());
                println!(
                    "{:width$}  {:>6}  {:>8}  {:>9}  {}",
                    topic.name, topic.uid, topic.pid, age, topic.watchers
                );
            }
            ExitCode::from(exit::OK)
        }
        Err(status) => {
            let code = report("ITopicRegistry.list", &status);
            if status.exception_code() == ExceptionCode::Security {
                eprintln!("  Listing topic names is `[global] topics`.");
            }
            code
        }
    }
}

/// What `reader` gives until end-of-file, and whether that came before `deadline`.
fn read_dump(reader: std::os::fd::OwnedFd, deadline: std::time::Instant) -> (Vec<u8>, bool) {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    use std::io::Read;
    let mut reader = std::fs::File::from(reader);
    let mut text = Vec::new();
    let mut chunk = vec![0; 64 << 10];
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return (text, false);
        }
        let mut fds = [PollFd::new(&reader, PollFlags::IN)];
        match poll(&mut fds, Timespec::try_from(left).ok().as_ref()) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Ok(_) => {}
            Err(e) => {
                eprintln!("rsb_service: waiting for the dump failed: {e}");
                return (text, false);
            }
        }
        match reader.read(&mut chunk) {
            Ok(0) => return (text, true),
            Ok(n) => text.extend_from_slice(chunk.get(..n).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                eprintln!("rsb_service: reading the dump failed: {e}");
                return (text, false);
            }
        }
    }
}

/// `dumpsys <service>`: the service writes to a pipe only this process reads, as `dumpsys` has it.
fn cmd_dump(name: &str, args: &[String], timeout: std::time::Duration) -> ExitCode {
    let Some(binder) = hub::check_service(name) else {
        eprintln!("rsb_service: {name}: not registered (or not visible to this caller)");
        return ExitCode::from(exit::NO);
    };
    // A kernel proxy, or the RPC proxy of a service the hub reaches through an accessor.
    if binder.as_remote().is_none() {
        // Only a binder local to this process lacks a proxy; `rsb_service` registers none.
        eprintln!("rsb_service: {name}: not a remote binder, nothing to dump");
        return ExitCode::from(exit::ERR);
    }
    let (reader, writer) = match rustix::pipe::pipe() {
        Ok(pipe) => pipe,
        Err(e) => {
            eprintln!("rsb_service: cannot make the pipe the dump is read from: {e}");
            return ExitCode::from(exit::ERR);
        }
    };
    let deadline = std::time::Instant::now() + timeout;
    let (sent, answered) = std::sync::mpsc::channel();
    let passed = args.to_vec();
    // A kernel binder call has no deadline: one to a stopped service waits for ever.
    let asking = std::thread::Builder::new()
        .name("rsb_service-dump".into())
        .spawn(move || {
            // The parcel closes `writer` as the call returns, so end-of-file is the service's.
            let reply = match binder.as_remote() {
                Some(proxy) => proxy.dump(writer, &passed),
                None => Err(rsbinder::StatusCode::BadType),
            };
            let _ = sent.send(reply);
        });
    if asking.is_err() {
        eprintln!("rsb_service: cannot start the thread that asks");
        return ExitCode::from(exit::ERR);
    }
    // Read whole before any goes to stdout: a pager the user has not paged would stall the service.
    let (text, ended) = read_dump(reader, deadline);
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    let reply = ended.then(|| answered.recv_timeout(left).ok()).flatten();
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = stdout.write_all(&text).and_then(|()| stdout.flush()) {
        // `rsb_service dump NAME | head -1`: the reader has what it wanted.
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            eprintln!("rsb_service: writing the dump to stdout failed: {e}");
            return ExitCode::from(exit::ERR);
        }
    }
    match reply {
        // The asking thread may be left waiting; the process ends with it.
        None => {
            eprintln!("rsb_service: dump of {name} timed out after {timeout:?}");
            ExitCode::from(exit::ERR)
        }
        Some(Ok(())) => ExitCode::from(exit::OK),
        Some(Err(code)) => {
            eprintln!("rsb_service: dump of {name} failed: {code:?}");
            if code == rsbinder::StatusCode::PermissionDenied {
                eprintln!(
                    "  The service refused the dump. For rsb_hub itself this is the \
                     `list` permission; see `rsb_hub --help`."
                );
            }
            if code == rsbinder::StatusCode::WouldBlock {
                eprintln!(
                    "  The service is still writing an earlier dump. rsb_hub writes one at a \
                     time, and its log names the caller whose dump it is."
                );
            }
            if code == rsbinder::StatusCode::FdsNotAllowed {
                eprintln!(
                    "  The service is reached over an RPC session that carries no file \
                     descriptors (an accessor's session, as in AOSP), so it cannot be sent \
                     the output to write to."
                );
            }
            ExitCode::from(exit::ERR)
        }
    }
}

/// Separate from `main` so a test can run clap's `debug_assert` on it.
fn cli() -> clap::Command {
    let name_arg = |help: &'static str| {
        clap::Arg::new("name")
            .value_name("NAME")
            .help(help)
            .required(true)
            .index(1)
    };

    clap::Command::new("rsb_service")
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about("Inspect the services registered with the binder service manager")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg(
            clap::Arg::new("device")
                .short('d')
                .long("device")
                .value_name("NAME")
                .global(true)
                .help("Name of the binder device to use (e.g. 'binder', 'mybinder')")
                .default_value("binder"),
        )
        .subcommand(
            clap::Command::new("list")
                .about("List the names of the registered services")
                .arg(
                    clap::Arg::new("priority")
                        .short('p')
                        .long("priority")
                        .value_name("CLASS")
                        .default_value("all")
                        .help(
                            "Only list services registered with this dump-priority \
                             class: all, default, critical, high, normal, proto",
                        ),
                ),
        )
        .subcommand(
            clap::Command::new("check")
                .about("Report whether a service is registered (exit 1 if it is not)")
                .arg(name_arg("Service name to look up")),
        )
        .subcommand(
            clap::Command::new("wait")
                .about(
                    "Wait until a service is registered (exit 1 if --timeout passes first, \
                     2 if the hub denies `find` on the name or cannot be reached)",
                )
                .arg(name_arg("Service name to wait for"))
                .arg(
                    clap::Arg::new("timeout")
                        .long("timeout")
                        .value_name("SECS")
                        .value_parser(clap::value_parser!(f64))
                        .help("Give up after this many seconds (default: wait without end)"),
                )
                .arg(
                    clap::Arg::new("start")
                        .long("start")
                        .action(clap::ArgAction::SetTrue)
                        .help(
                            "Look the name up with getService, which starts a declared \
                             service that is not registered. Without it nothing is started",
                        ),
                ),
        )
        .subcommand(
            clap::Command::new("info").about("List the registered services with their owning pid"),
        )
        .subcommand(
            clap::Command::new("declared")
                .about("Report whether a service is declared in the hub's configuration")
                .arg(name_arg("Instance name, e.g. com.example.IFoo/default")),
        )
        .subcommand(
            clap::Command::new("instances")
                .about("List the declared instances of an interface")
                .arg(name_arg("Interface name, e.g. com.example.IFoo").value_name("INTERFACE")),
        )
        .subcommand(
            clap::Command::new("connection")
                .about("Print the connection info declared for a service, if any")
                .arg(name_arg("Instance name, e.g. com.example.IFoo/default")),
        )
        .subcommand(
            clap::Command::new("dump")
                .about("Ask a service to dump its state to stdout")
                .arg(name_arg("Service name to dump"))
                .arg(
                    clap::Arg::new("timeout")
                        .long("timeout")
                        .value_name("SECS")
                        .default_value("10")
                        .value_parser(clap::value_parser!(f64))
                        .help(
                            "Exit 2, after printing what arrived, if the dump has not ended \
                             by then; a `--timeout` for the service itself goes after `--`",
                        ),
                )
                .arg(
                    clap::Arg::new("args")
                        .value_name("ARGS")
                        .index(2)
                        .num_args(0..)
                        .allow_hyphen_values(true)
                        .help("Arguments passed through to the service's dump handler"),
                ),
        )
        .after_help(
            "Examples:\n    \
            What is registered right now?\n    \
            $ rsb_service list\n\n    \
            Who owns each of them?\n    \
            $ rsb_service info\n\n    \
            Is a service up yet? (exit status 0 = yes, 1 = no)\n    \
            $ rsb_service check com.example.IFoo/default\n\n    \
            Is it even installed? (`declared` answers before anything registers)\n    \
            $ rsb_service declared com.example.IFoo/default\n    \
            $ rsb_service instances com.example.IFoo\n\n    \
            What does rsb_hub itself see?\n    \
            $ rsb_service dump manager\n    \
            $ rsb_service dump manager com.example    # narrow it by substring\n\n    \
            Exit status: 0 the answer is yes, 1 the answer is no, 2 the question \n    \
            could not be answered (no service manager, or its policy denied it).",
        )
        // Also without the feature, hidden: a usage error would exit 2 with nothing said of why.
        .subcommand(
            list_command("topics")
                .about(
                    "List the topic names of rsb_hub's topic registry: name uid pid age watchers \
                     (as `topic list`)",
                )
                .hide(!cfg!(feature = "topic")),
        )
        .subcommand(
            clap::Command::new("topic")
                .about("Look at a topic from outside its publisher")
                .hide(!cfg!(feature = "topic"))
                .subcommand_required(true)
                .subcommand(list_command("list").about(
                    "List the topic names of rsb_hub's topic registry: name uid pid age watchers",
                ))
                .subcommand(
                    named_topic("stat")
                        .about("Print the topic's status: one line, then one per subscriber")
                        .arg(
                            clap::Arg::new("timeout")
                                .long("timeout")
                                .value_name("SECS")
                                .default_value("2")
                                .value_parser(clap::value_parser!(f64))
                                .help("Exit 2 if the publisher has not answered by then"),
                        ),
                )
                .subcommand(counted(named_topic("hz").about(
                    "Each second, the records received, their intervals and those lost",
                )))
                .subcommand(counted(
                    named_topic("echo").about("Print each record's length and bytes"),
                ))
                .subcommand(counted(
                    named_topic("record")
                        .about(
                            "Write each record to FILE (format: the book's \"Inspecting a topic\")",
                        )
                        .arg(
                            clap::Arg::new("file")
                                .value_name("FILE")
                                .index(2)
                                .required(true)
                                .help("The file to write"),
                        ),
                )),
        )
}

/// `topics` and `topic list`.
fn list_command(name: &'static str) -> clap::Command {
    clap::Command::new(name).arg(
        clap::Arg::new("prefix")
            .value_name("PREFIX")
            .index(1)
            .default_value("")
            .help("Only list the names that start with this"),
    )
}

/// A topic command that takes a topic's name and where to look it up.
fn named_topic(name: &'static str) -> clap::Command {
    clap::Command::new(name)
        .arg(
            clap::Arg::new("name")
                .value_name("NAME")
                .help("The topic's name: in rsb_hub's topic registry unless an option says")
                .required(true)
                .index(1),
        )
        .arg(
            clap::Arg::new("service")
                .long("service")
                .action(clap::ArgAction::SetTrue)
                .help("NAME is a service manager name, not a topic registry one"),
        )
}

/// A topic command that receives, up to `--count` records.
fn counted(command: clap::Command) -> clap::Command {
    command.arg(
        clap::Arg::new("count")
            .long("count")
            .value_name("N")
            .value_parser(clap::value_parser!(u64))
            .help("Stop after N records; without it, run until the topic ends"),
    )
}

/// What a `topic` subcommand names: its topic, and where to look it up.
#[cfg(feature = "topic")]
fn topic_source(m: &clap::ArgMatches) -> rsbinder_tools::topic_cmd::Source {
    use rsbinder_tools::topic_cmd::Source;
    if m.get_flag("service") {
        Source::Service
    } else {
        Source::Registry
    }
}

/// `topic stat|hz|echo|record`, once the binder device is open.
#[cfg(feature = "topic")]
fn cmd_topic(command: &str, m: &clap::ArgMatches) -> ExitCode {
    use rsbinder_tools::topic_cmd::{self, Watch};
    let name = m
        .get_one::<String>("name")
        .expect("name is a required argument");
    let source = topic_source(m);
    let count = || m.get_one::<u64>("count").copied();
    let code = match command {
        "stat" => {
            let secs = *m
                .get_one::<f64>("timeout")
                .expect("timeout has a default value");
            let Ok(timeout) = std::time::Duration::try_from_secs_f64(secs) else {
                eprintln!("rsb_service: --timeout {secs} is not a duration");
                return ExitCode::from(exit::ERR);
            };
            topic_cmd::stat(name, &source, timeout)
        }
        "hz" => topic_cmd::watch(name, &source, &Watch::Hz, count()),
        "echo" => topic_cmd::watch(name, &source, &Watch::Echo, count()),
        "record" => {
            let file = m
                .get_one::<String>("file")
                .expect("file is a required argument");
            topic_cmd::watch(name, &source, &Watch::Record(file.into()), count())
        }
        _ => exit::ERR,
    };
    ExitCode::from(code)
}

fn main() -> ExitCode {
    let matches = cli().get_matches();

    rsbinder_tools::logging::init_command();

    #[cfg(not(feature = "topic"))]
    if let Some(command @ ("topics" | "topic")) = matches.subcommand_name() {
        eprintln!("rsb_service: built without the `topic` feature, so it has no `{command}`");
        return ExitCode::from(exit::ERR);
    }

    // `topic list` is `topics`; the other topic commands are `(command, matches)`.
    let (listing, topic) = match matches.subcommand() {
        Some(("topics", m)) => (Some(m), None),
        Some(("topic", m)) => match m.subcommand() {
            Some(("list", m)) => (Some(m), None),
            Some((command, m)) => (None, Some((command, m))),
            None => (None, None),
        },
        _ => (None, None),
    };

    let device = matches
        .get_one::<String>("device")
        .expect("device has a default value");
    // 0 threads: none the kernel asks for; a topic subscription starts the pool thread it needs.
    if let Err(e) = ProcessState::init(&format!("{DEFAULT_BINDERFS_PATH}/{device}"), 0) {
        eprintln!(
            "rsb_service: cannot open the binder device {DEFAULT_BINDERFS_PATH}/{device}: {e:?}"
        );
        return ExitCode::from(exit::ERR);
    }
    // `check_service` maps a missing hub to None, which would read as exit 1, not 2.
    if let Err(e) = hub::default() {
        eprintln!("rsb_service: no service manager on {DEFAULT_BINDERFS_PATH}/{device}: {e}");
        return ExitCode::from(exit::ERR);
    }

    fn name_of(m: &clap::ArgMatches) -> &str {
        m.get_one::<String>("name")
            .expect("name is a required argument")
    }

    #[cfg(feature = "topic")]
    if let Some(m) = listing {
        return cmd_topics(
            m.get_one::<String>("prefix")
                .expect("prefix has a default value"),
        );
    }
    #[cfg(feature = "topic")]
    if let Some((command, m)) = topic {
        return cmd_topic(command, m);
    }
    #[cfg(not(feature = "topic"))]
    let _ = (listing, topic);

    match matches.subcommand() {
        Some(("list", m)) => {
            let spec = m
                .get_one::<String>("priority")
                .expect("priority has a default value");
            match dump_priority(spec) {
                Some(priority) => cmd_list(priority),
                None => {
                    eprintln!(
                        "rsb_service: unknown priority class {spec:?} (expected one of: \
                         all, default, critical, high, normal, proto)"
                    );
                    ExitCode::from(exit::ERR)
                }
            }
        }
        Some(("check", m)) => cmd_check(name_of(m)),
        Some(("wait", m)) => {
            let timeout = match m.get_one::<f64>("timeout") {
                None => None,
                Some(secs) => match std::time::Duration::try_from_secs_f64(*secs) {
                    Ok(timeout) => Some(timeout),
                    Err(_) => {
                        eprintln!("rsb_service: --timeout takes seconds, 0 or more; got {secs}");
                        return ExitCode::from(exit::ERR);
                    }
                },
            };
            cmd_wait(name_of(m), timeout, m.get_flag("start"))
        }
        Some(("info", _)) => cmd_info(),
        Some(("declared", m)) => cmd_declared(name_of(m)),
        Some(("instances", m)) => cmd_instances(name_of(m)),
        Some(("connection", m)) => cmd_connection(name_of(m)),
        Some(("dump", m)) => {
            let args: Vec<String> = m
                .get_many::<String>("args")
                .map(|v| v.cloned().collect())
                .unwrap_or_default();
            let secs = *m
                .get_one::<f64>("timeout")
                .expect("timeout has a default value");
            // Zero is refused too, as `dumpsys -t 0` is: no dump ends in no time.
            let timeout = match std::time::Duration::try_from_secs_f64(secs) {
                Ok(timeout) if !timeout.is_zero() => timeout,
                _ => {
                    eprintln!("rsb_service: --timeout takes seconds, more than 0; got {secs}");
                    return ExitCode::from(exit::ERR);
                }
            };
            cmd_dump(name_of(m), &args, timeout)
        }
        // Unreachable: `subcommand_required` rejects this before we get here.
        _ => ExitCode::from(exit::ERR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// clap validates a subcommand only when it is invoked, so check all of them here.
    #[test]
    fn the_command_line_surface_is_well_formed() {
        cli().debug_assert();
    }

    /// What `dump` parsed: its timeout in seconds and the arguments the service is passed.
    fn dump_line(line: &[&str]) -> (f64, Vec<String>) {
        let matches = cli().try_get_matches_from(line).expect("parses");
        let (_, m) = matches.subcommand().expect("a subcommand");
        let args = m.get_many::<String>("args").map(|v| v.cloned().collect());
        let timeout = *m.get_one::<f64>("timeout").expect("a default");
        (timeout, args.unwrap_or_default())
    }

    /// `--timeout` is this command's wherever it stands; after `--` it is the service's.
    #[test]
    fn dump_keeps_its_timeout_apart_from_the_arguments_of_the_service() {
        let (timeout, args) = dump_line(&["rsb_service", "dump", "--timeout", "3", "manager"]);
        assert_eq!((timeout, args.len()), (3.0, 0));
        let (timeout, args) = dump_line(&["rsb_service", "dump", "manager", "--timeout", "3"]);
        assert_eq!((timeout, args.len()), (3.0, 0));
        let line = ["rsb_service", "dump", "manager", "--", "--timeout", "3"];
        let (timeout, args) = dump_line(&line);
        assert_eq!(
            (timeout, args),
            (10.0, vec!["--timeout".into(), "3".into()])
        );
        let (timeout, args) = dump_line(&["rsb_service", "dump", "manager", "-a", "x"]);
        assert_eq!((timeout, args), (10.0, vec!["-a".into(), "x".into()]));
    }

    /// The reader's timeout ends a dump whose writer keeps the pipe open and writes no more.
    #[test]
    fn a_dump_that_does_not_end_is_read_until_the_deadline() {
        use std::time::{Duration, Instant};
        let (reader, writer) = rustix::pipe::pipe().expect("pipe");
        rustix::io::write(&writer, b"part").expect("write");
        let (text, ended) = read_dump(reader, Instant::now() + Duration::from_millis(100));
        assert_eq!((text.as_slice(), ended), (&b"part"[..], false));

        let (reader, writer) = rustix::pipe::pipe().expect("pipe");
        rustix::io::write(&writer, b"whole").expect("write");
        drop(writer);
        let (text, ended) = read_dump(reader, Instant::now() + Duration::from_secs(10));
        assert_eq!((text.as_slice(), ended), (&b"whole"[..], true));
    }

    #[test]
    fn priority_classes_map_to_their_bitmasks() {
        assert_eq!(dump_priority("all"), Some(hub::DUMP_FLAG_PRIORITY_ALL));
        assert_eq!(
            dump_priority("critical"),
            Some(hub::DUMP_FLAG_PRIORITY_CRITICAL)
        );
        assert_eq!(dump_priority("proto"), Some(hub::DUMP_FLAG_PROTO));
        assert_eq!(dump_priority("nonsense"), None);
        // Case-sensitive, so a future flag cannot silently collide with `Default`.
        assert_eq!(dump_priority("All"), None);
    }

    /// `--help` documents these values, so they are an interface.
    #[test]
    fn exit_codes_are_yes_no_error() {
        assert_eq!((exit::OK, exit::NO, exit::ERR), (0, 1, 2));
    }
}
