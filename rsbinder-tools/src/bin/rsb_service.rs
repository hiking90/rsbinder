// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `rsb_service` — ask the running service manager what it knows.
//!
//! The Linux counterpart of Android's `service` and `dumpsys -l`: what is
//! registered, who registered it, what is declared but not started, and
//! what a service says about itself when asked to dump.
//!
//! Every question goes over binder as an ordinary client, so `rsb_hub`'s
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
    /// The question was answered, and the answer is no: not registered,
    /// not declared, no connection info. `test`-style, so
    /// `rsb_service check foo || start-foo` reads correctly.
    pub const NO: u8 = 1;
    /// The question could not be answered: no service manager, or its
    /// policy denied the request.
    pub const ERR: u8 = 2;
}

/// Report a failed request, with a hint when the policy is the reason.
///
/// `rsb_hub` puts the denied permission, the name and the caller's uid in
/// the status message, so the message is worth showing verbatim; the hint
/// says where to change it.
fn report(what: &str, status: &Status) -> ExitCode {
    eprintln!("rsb_service: {what} failed: {status}");
    if status.exception_code() == ExceptionCode::Security {
        eprintln!(
            "  The service manager's policy denies this to uid {}. Grant it in \
             the hub's configuration (`rsb_hub --config <DIR>`, default \
             /etc/rsbinder/hub.d) or run as a user the policy allows.",
            rustix::process::getuid().as_raw()
        );
    }
    ExitCode::from(exit::ERR)
}

/// The `--priority` values, mapped to the `dumpPriority` bitmask
/// `listServices` filters on. `all` is the default because an operator
/// asking "what is registered" means all of it; AOSP's `service list`
/// makes the same choice.
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

/// Non-blocking registration probe.
///
/// Uses `checkService`, never `getService`: a lookup that misses can start
/// a declared service (`rsb_hub`'s stand-in for `ctl.interface_start`), and
/// a tool whose job is to *report* state must not change it.
///
/// A caller the policy denies `find` sees "not registered" here, the same
/// as for a name that truly is not registered. That is deliberate in the
/// hub — an error would let a denied caller enumerate which names exist —
/// so it is not something this tool can, or should, see through.
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

/// ` (<descriptor>)` when the binder carries one, empty otherwise.
///
/// A proxy learns its descriptor when it is cast to an interface; one
/// handed straight back by `checkService` has not been, so this is
/// usually empty and must not be presented as "unknown interface".
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

/// Send `DUMP_TRANSACTION` to a registered service and let it write to our
/// stdout — Android's `dumpsys <service>`.
///
/// The service writes into a *duplicate* of stdout rather than stdout
/// itself: the parcel takes ownership of the descriptor it carries, so
/// handing over the real one would close this process's stdout when the
/// transaction completes.
fn cmd_dump(name: &str, args: &[String]) -> ExitCode {
    let Some(binder) = hub::check_service(name) else {
        eprintln!("rsb_service: {name}: not registered (or not visible to this caller)");
        return ExitCode::from(exit::NO);
    };
    let Some(proxy) = binder.as_proxy() else {
        // Only reachable if the name resolved to a binder living in this
        // very process, which `rsb_service` never registers.
        eprintln!("rsb_service: {name}: not a remote binder, nothing to dump");
        return ExitCode::from(exit::ERR);
    };
    // Anything already buffered has to reach the terminal before the
    // service starts writing to the same file, or the output interleaves
    // in the wrong order.
    let _ = std::io::stdout().flush();
    let fd = match rustix::io::dup(std::io::stdout()) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("rsb_service: cannot duplicate stdout: {e}");
            return ExitCode::from(exit::ERR);
        }
    };
    match proxy.dump(fd, args) {
        Ok(()) => ExitCode::from(exit::OK),
        Err(code) => {
            eprintln!("rsb_service: dump of {name} failed: {code:?}");
            if code == rsbinder::StatusCode::PermissionDenied {
                eprintln!(
                    "  The service refused the dump. For rsb_hub itself this is the \
                     `list` permission; see `rsb_hub --help`."
                );
            }
            ExitCode::from(exit::ERR)
        }
    }
}

/// The whole command-line surface, built separately from [`main`] so the
/// tests can hand it to `clap`'s own `debug_assert` — misdeclared
/// positionals are a debug-time panic, which is a failing test here rather
/// than a broken release binary.
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
}

fn main() -> ExitCode {
    let matches = cli().get_matches();

    env_logger::init();

    let device = matches
        .get_one::<String>("device")
        .expect("device has a default value");
    // 0 threads: `checkService` and friends are synchronous round trips
    // answered on this thread, and `rsb_service` never receives an inbound
    // transaction, so the kernel has no reason to ask for a worker.
    if let Err(e) = ProcessState::init(&format!("{DEFAULT_BINDERFS_PATH}/{device}"), 0) {
        eprintln!(
            "rsb_service: cannot open the binder device {DEFAULT_BINDERFS_PATH}/{device}: {e:?}"
        );
        return ExitCode::from(exit::ERR);
    }

    let name_of = |m: &clap::ArgMatches| {
        m.get_one::<String>("name")
            .expect("name is a required argument")
            .clone()
    };

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
        Some(("check", m)) => cmd_check(&name_of(m)),
        Some(("info", _)) => cmd_info(),
        Some(("declared", m)) => cmd_declared(&name_of(m)),
        Some(("instances", m)) => cmd_instances(&name_of(m)),
        Some(("connection", m)) => cmd_connection(&name_of(m)),
        Some(("dump", m)) => {
            let args: Vec<String> = m
                .get_many::<String>("args")
                .map(|v| v.cloned().collect())
                .unwrap_or_default();
            cmd_dump(&name_of(m), &args)
        }
        // Unreachable: `subcommand_required` rejects this before we get here.
        _ => ExitCode::from(exit::ERR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every subcommand's arguments are well-formed. `clap` only checks
    /// this in debug builds, and only when the offending subcommand is
    /// actually invoked — a positional index collision in `dump` would
    /// otherwise ship undetected by any test that never runs `dump`.
    #[test]
    fn the_command_line_surface_is_well_formed() {
        cli().debug_assert();
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
        // Case matters: accepting "ALL" silently would invite `--priority
        // Default` to mean something else on a future flag.
        assert_eq!(dump_priority("All"), None);
    }

    /// The exit codes are a documented interface (`--help` promises them),
    /// so pin them.
    #[test]
    fn exit_codes_are_yes_no_error() {
        assert_eq!((exit::OK, exit::NO, exit::ERR), (0, 1, 2));
    }
}
