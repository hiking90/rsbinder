// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// STAGE3 gate for the Android 15 service-manager split: drive the whole
// `hub` surface against a **real AOSP `servicemanager`** and check that
// every method that `android-15.0.0_r6` renumbered still lands on the
// method it names.
//
// The check is behavioural, and that is what makes it decisive. rsbinder
// picks the protocol from a probe, and the two candidates disagree from
// `checkService` on, so a wrong pick does not fail at the probe — it fails
// one method at a time:
//
//   * wrong `android_14` on an r6+ peer: `addService` (2) lands on
//     `checkService`, which reads the name and leaves the rest of the
//     parcel, and AOSP's generated `onTransact` rejects the leftovers as
//     `BAD_PARCELABLE`.
//   * wrong `android_15` on a pre-r6 peer: `getServiceDebugInfo` (14) is
//     past the end of the interface — `UnknownTransaction`.
//
// So a green run over the codes below proves the probe chose right, and the
// same binary is the gate for *both* numberings: run it against an r6+
// servicemanager and against a pre-r6 one. See
// `example-hello/cpp/run_a15_qpr_stage3.sh`, which does both.
//
//   cargo ndk -t arm64-v8a build -p example-hello --bin a15_qpr_interop
//   ./a15_qpr_interop [driver] [pre-r6|r6+]
//
// `driver` is the binder node ('' for the default). The optional second
// argument pins which numbering the probe must have picked: without it a
// green run proves the probe and the chosen module agree with the peer, but
// not *which* peer that was — a pre-r6 servicemanager put where the r6+ one
// belongs would pass both halves of the gate on the same numbering.
//
// Exit status is the verdict: 0 all passed.

use std::process::ExitCode;

use example_hello::*;
use rsbinder::hub::{self, BnClientCallback, IClientCallback};
use rsbinder::*;

const ECHO_NAME: &str = "rsbinder.test.a15.echo";
/// A second name, never looked up, so `tryUnregisterService` meets AOSP's
/// "no clients" precondition and reports on the transaction rather than on
/// the reference count.
const LAZY_NAME: &str = "rsbinder.test.a15.lazy";

struct HelloService;
impl Interface for HelloService {}
impl IHello for HelloService {
    fn echo(&self, echo: &str) -> rsbinder::BinderResult<String> {
        Ok(echo.to_owned())
    }
}

/// Only has to be a live binder the service manager accepts.
/// `registerClientCallback` is the transaction under test; whether
/// `onClients` ever arrives is the lazy gate's business, not this one's
/// (`run_lazy_service_stage3.sh` covers that).
struct NoopClientCallback;
impl Interface for NoopClientCallback {}
impl IClientCallback for NoopClientCallback {
    fn onClients(&self, _registered: &SIBinder, _has_clients: bool) -> rsbinder::BinderResult<()> {
        Ok(())
    }
}

struct Report {
    pass: u32,
    fail: u32,
}

impl Report {
    /// `codes` is the transaction code this method has on the two
    /// numberings, written `pre-r6/r6+`. Which one a given run sends is the
    /// thing under test, so neither may be printed as *the* code.
    fn check(&mut self, codes: &str, what: &str, outcome: std::result::Result<String, String>) {
        match outcome {
            Ok(detail) => {
                self.pass += 1;
                println!("  PASS  {codes:>5}  {what}{detail}");
            }
            Err(why) => {
                self.fail += 1;
                println!("  FAIL  {codes:>5}  {what} — {why}");
            }
        }
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let driver = std::env::args()
        .nth(1)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BINDER_PATH.to_string());
    println!("binder driver: {driver}");

    if let Err(e) = ProcessState::init(&driver, DEFAULT_MAX_BINDER_THREADS) {
        // stdout on purpose: the gate script discards stderr.
        println!("ProcessState::init({driver}) failed: {e}");
        return ExitCode::from(2);
    }
    // `onClients` and `echo` are inbound transactions.
    ProcessState::start_thread_pool();

    // The probe runs here. A device whose numbering has no module compiled
    // in is refused, and that is a distinct outcome from any failure below.
    let sm = match hub::default() {
        Ok(sm) => sm,
        Err(e) => {
            // stdout on purpose: the gate script discards stderr.
            println!("hub::default() refused this service manager: {e:?}");
            return ExitCode::from(2);
        }
    };
    #[cfg(target_os = "android")]
    let numbering = match &*sm {
        hub::ServiceManager::Android14(_) => "pre-r6",
        hub::ServiceManager::Android15(_) => "r6+",
        _ => "neither Android 15 numbering",
    };
    // Off-device builds only ever see the Android 16 fallback variant.
    #[cfg(not(target_os = "android"))]
    let numbering = match &*sm {
        hub::ServiceManager::Android16(_) => "neither Android 15 numbering",
    };
    println!("probe picked: the {numbering} module");
    if let Some(expected) = std::env::args().nth(2) {
        if expected != numbering {
            println!("this run had to probe {expected}, but the peer speaks {numbering}");
            return ExitCode::from(2);
        }
    }

    let mut r = Report { pass: 0, fail: 0 };
    let echo_service = BnHello::new_binder(HelloService);
    let lazy_service = BnHello::new_binder(HelloService);

    println!("checks are labelled with the transaction code on each numbering, pre-r6/r6+\n");

    // ---- addService ------------------------------------------------------
    r.check(
        "2/3",
        "addService",
        hub::add_service(ECHO_NAME, &echo_service)
            .map(|()| String::new())
            .map_err(|e| format!("{e:?}")),
    );

    // ---- listServices ----------------------------------------------------
    let listed = hub::list_services(hub::DUMP_FLAG_PRIORITY_ALL);
    r.check(
        "3/4",
        "listServices lists it",
        if listed.iter().any(|n| n == ECHO_NAME) {
            Ok(format!(" ({} names)", listed.len()))
        } else {
            Err(format!("{ECHO_NAME} not among {} names", listed.len()))
        },
    );

    // ---- getService (the one code that did not move; every A15 lookup routes through it) ----
    r.check(
        "0/0",
        "getService round-trips the service",
        match hub::check_interface::<dyn IHello>(ECHO_NAME) {
            Ok(proxy) => match proxy.echo("a15") {
                Ok(s) if s == "a15" => Ok(String::new()),
                Ok(s) => Err(format!("echo returned {s:?}")),
                Err(e) => Err(format!("echo failed: {e:?}")),
            },
            Err(e) => Err(format!("{e:?}")),
        },
    );

    // ---- isDeclared (answer is `false`; under test is a bool reply, not a parcel error) ----
    r.check(
        "6/7",
        "isDeclared answers",
        hub::try_is_declared(ECHO_NAME)
            .map(|d| format!(" (declared={d})"))
            .map_err(|e| format!("{e:?}")),
    );

    // ---- getServiceDebugInfo ---------------------------------------------
    // 14 on an r6+ peer is the code the protocol probe sends, so this is
    // also the check that the probe's target is a real, harmless method.
    r.check(
        "13/14",
        "getServiceDebugInfo lists it",
        match hub::try_get_service_debug_info() {
            Ok(infos) => {
                if infos.iter().any(|i| i.name == ECHO_NAME) {
                    Ok(format!(" ({} entries)", infos.len()))
                } else {
                    Err(format!("{ECHO_NAME} not among {} entries", infos.len()))
                }
            }
            Err(e) => Err(format!("{e:?}")),
        },
    );

    // ---- registerClientCallback ------------------------------------------
    r.check(
        "2/3",
        "addService (second name)",
        hub::add_service(LAZY_NAME, &lazy_service)
            .map(|()| String::new())
            .map_err(|e| format!("{e:?}")),
    );
    let callback = BnClientCallback::new_binder(NoopClientCallback);
    r.check(
        "11/12",
        "registerClientCallback",
        hub::register_client_callback(LAZY_NAME, &lazy_service.as_binder(), &callback)
            .map(|()| String::new())
            .map_err(|e| format!("{e:?}")),
    );

    // ---- tryUnregisterService (nothing looked `LAZY_NAME` up, so no "known client" refusal) ----
    r.check(
        "12/13",
        "tryUnregisterService",
        hub::try_unregister_service(LAZY_NAME, &lazy_service.as_binder())
            .map(|()| String::new())
            .map_err(|e| format!("{e:?}")),
    );
    r.check(
        "0/0",
        "the unregistered name is gone",
        match hub::try_get_service(LAZY_NAME) {
            Ok(None) => Ok(String::new()),
            Ok(Some(_)) => Err("still registered".to_string()),
            Err(e) => Err(format!("{e:?}")),
        },
    );

    // Leave the registry as we found it. A failure here is not a verdict on
    // the wire — the name may already be gone — so it only warns.
    if let Err(e) = hub::try_unregister_service(ECHO_NAME, &echo_service.as_binder()) {
        println!("  note  cleanup of {ECHO_NAME} returned {e:?}");
    }

    println!("\n===== {} passed, {} failed =====", r.pass, r.fail);
    if r.fail == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
