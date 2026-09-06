// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Access-control probe for `rsb_hub` (Plan 6-1 acceptance gates).
//!
//! Drives a scripted sequence of service-manager calls over real kernel
//! binder and prints one `RESULT <op> <arg> <outcome>` line per step. The
//! interesting cases are *denials*, and a denial is only meaningful across a
//! real transaction: the caller uid the policy matches on is filled in by the
//! binder driver, so nothing in-process can stand in for it.
//!
//! ```text
//! ac61_probe add:NAME addsame:NAME find:NAME get:NAME notify:NAME unnotify:NAME
//!            declared:NAME instances:IFACE conninfo:NAME list debuginfo hold:SECS
//! ```
//!
//! `addsame` registers *one* binder under every name it is given, which is
//! what exercises the hub's per-binder death-subscription accounting: the
//! hub must take a single kernel subscription and still drop every name when
//! that one binder dies.
//!
//! Every step runs in **one** process on purpose. A registered binder lives
//! only as long as the process that owns it — registering from a throwaway
//! invocation would have the hub's death recipient drop the entry before the
//! next invocation could list it.

use rsbinder::*;

/// Minimal registrable object: the probe publishes it and never receives a
/// call on it.
struct ProbeBinder;

impl Interface for ProbeBinder {}

impl Remotable for ProbeBinder {
    fn descriptor() -> &'static str {
        "rsbinder.test.ac61.IProbe"
    }
    /// Handles nothing, and says so. Returning `Ok(())` here would be a
    /// trap: `Inner::transact` only falls back to its built-in handling of
    /// `INTERFACE_TRANSACTION` when the remotable answers
    /// `UnknownTransaction`. Swallowing the code instead sends an empty
    /// reply, and the hub — which resolves every freshly received binder by
    /// asking it for its descriptor — then fails `addService` while reading
    /// that reply, which reads as a parcel bug rather than as this.
    fn on_transact(
        &self,
        _code: TransactionCode,
        _reader: &mut Parcel,
        _reply: &mut Parcel,
    ) -> Result<()> {
        Err(StatusCode::UnknownTransaction)
    }
    fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

struct Callback;
impl Interface for Callback {}
impl hub::IServiceCallback for Callback {
    fn onRegistration(&self, _name: &str, _binder: &SIBinder) -> BinderResult<()> {
        Ok(())
    }
}

/// Classify a rich [`Status`], keeping `EX_SECURITY` distinct — that is what
/// the policy gates assert on.
fn outcome_of_status(err: &Status) -> &'static str {
    eprintln!("  status: {err:?}");
    match err.exception_code() {
        ExceptionCode::Security => "SECURITY",
        _ => "ERROR",
    }
}

/// Classify a [`StatusCode`]. The helpers returning this have already
/// flattened the exception away (`Status -> StatusCode` maps an exception
/// carrying an `Ok` code to `FailedTransaction`), so a denial surfaces here
/// as `FAILEDTXN` rather than as `SECURITY`.
fn outcome_of_code(err: StatusCode) -> &'static str {
    eprintln!("  status code: {err:?}");
    match err {
        StatusCode::FailedTransaction => "FAILEDTXN",
        _ => "ERROR",
    }
}

fn main() {
    let steps: Vec<String> = std::env::args().skip(1).collect();
    if steps.is_empty() {
        eprintln!("usage: ac61_probe <add:NAME|find:NAME|notify:NAME|list|debuginfo>...");
        std::process::exit(2);
    }

    ProcessState::init_default().expect("binder device");
    // Required before registering anything: the hub resolves a freshly
    // received binder by sending it an `INTERFACE_TRANSACTION`
    // (`ProcessState::strong_proxy_for_handle` -> `query_interface`). With no
    // pooled thread to answer it, the hub's `addService` fails while reading
    // an empty reply, and the failure looks like a parcel bug rather than a
    // missing thread pool.
    ProcessState::start_thread_pool();

    // Registered binders are kept alive for the whole run by this vector.
    let mut registered: Vec<Binder<ProbeBinder>> = Vec::new();
    // The one binder every `addsame` step shares.
    let shared = Binder::new(ProbeBinder);
    let callback = hub::BnServiceCallback::new_binder(Callback);

    for step in &steps {
        let (op, arg) = match step.split_once(':') {
            Some((op, arg)) => (op, arg),
            None => (step.as_str(), ""),
        };
        let outcome = match op {
            "add" => {
                let binder = Binder::new(ProbeBinder);
                let result = match hub::add_service(arg, Interface::as_binder(&binder)) {
                    Ok(()) => "OK",
                    Err(err) => outcome_of_status(&err),
                };
                registered.push(binder);
                result
            }
            "addsame" => match hub::add_service(arg, Interface::as_binder(&shared)) {
                Ok(()) => "OK",
                Err(err) => outcome_of_status(&err),
            },
            "find" => match hub::check_service(arg) {
                Some(_) => "OK",
                None => "NOTFOUND",
            },
            // `get` is the half that may start a declared service;
            // `find` (checkService) must never have that side effect.
            // `try_get_service` is the `getService` wire call, which is
            // what carries that side effect.
            "get" => match hub::try_get_service(arg).ok().flatten() {
                Some(_) => "OK",
                None => "NOTFOUND",
            },
            "declared" => {
                if hub::is_declared(arg) {
                    "YES"
                } else {
                    "NO"
                }
            }
            "instances" => {
                for instance in hub::get_declared_instances(arg) {
                    println!("INSTANCE {instance}");
                }
                "OK"
            }
            "conninfo" => {
                let reported = hub::get_connection_info(arg)
                    .map(|c| format!("{}:{}", c.ipAddress, c.port))
                    .unwrap_or_else(|| "none".to_owned());
                println!("RESULT conninfo {arg} {reported}");
                continue;
            }
            "notify" => match hub::register_for_notifications(arg, &callback) {
                Ok(()) => "OK",
                Err(err) => outcome_of_code(err),
            },
            "unnotify" => match hub::unregister_for_notifications(arg, &callback) {
                Ok(()) => "OK",
                Err(err) => outcome_of_code(err),
            },
            "list" => {
                // `hub::list_services` swallows a transport error into an
                // empty vec, so this step cannot distinguish "denied" from
                // "nothing registered" — assert the global gate through
                // `debuginfo` and use these lines for the per-name filter.
                for name in hub::list_services(hub::DUMP_FLAG_PRIORITY_ALL) {
                    println!("LIST {name}");
                }
                "OK"
            }
            "debuginfo" => match hub::get_service_debug_info() {
                Ok(entries) => {
                    for entry in entries {
                        println!("DBG {}", entry.name);
                    }
                    "OK"
                }
                Err(err) => outcome_of_code(err),
            },
            // Stay alive with everything registered so far, so a *second*
            // process can observe the registry. Every other step registers
            // and exits, which is what the death-cleanup gates depend on and
            // what makes a live registry impossible to inspect from outside
            // without this.
            "hold" => {
                let secs: u64 = arg.parse().unwrap_or_else(|_| {
                    eprintln!("hold takes a number of seconds, got {arg:?}");
                    std::process::exit(2);
                });
                println!("RESULT hold {arg} OK");
                // Flushed before sleeping: whoever is waiting on this line
                // is the process that will inspect the registry.
                use std::io::Write;
                let _ = std::io::stdout().flush();
                std::thread::sleep(std::time::Duration::from_secs(secs));
                continue;
            }
            other => {
                eprintln!("unknown op: {other}");
                std::process::exit(2);
            }
        };
        println!("RESULT {op} {arg} {outcome}");
    }
}
