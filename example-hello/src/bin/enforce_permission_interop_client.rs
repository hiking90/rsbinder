// SPDX-License-Identifier: Apache-2.0
//
// Client — drives `IPermCheck` on the emulator
// alongside `enforce_permission_interop_service` to prove the `@EnforcePermission`
// codegen denies via `Status::Security` for a permission that does not
// exist in `system_server`'s `PermissionManagerService` registry.
//
// PASS conditions (printed markers + non-zero exit on any miss):
//   * `doSingle()` / `doAllOf()` / `doAnyOf()` return `Ok(true)` —
//     android.permission.{INTERNET, ACCESS_NETWORK_STATE, BLUETOOTH*}
//     are recognised permissions; root has them.
//   * `doDenied()` returns `Err(Status)` with
//     `ExceptionCode::Security` — the generated check denies before the
//     service body runs.

use std::process::ExitCode;

use rsbinder::service::{kernel, Broker as _};
use rsbinder::*;

use example_hello::permcheck::{IPermCheck, SERVICE_NAME};

fn expect_ok_true(label: &str, r: rsbinder::status::Result<bool>) -> bool {
    match r {
        Ok(true) => {
            println!("STAGE3_4_2_PASS {label}=true");
            true
        }
        Ok(false) => {
            eprintln!("STAGE3_4_2_FAIL {label} returned Ok(false)");
            false
        }
        Err(e) => {
            eprintln!("STAGE3_4_2_FAIL {label} returned Err({e:?})");
            false
        }
    }
}

fn expect_security_denial(r: rsbinder::status::Result<bool>) -> bool {
    match r {
        Err(status) if status.exception_code() == ExceptionCode::Security => {
            println!(
                "STAGE3_4_2_PASS doDenied=Security exception={:?}",
                status.exception_code()
            );
            true
        }
        Err(other) => {
            eprintln!("STAGE3_4_2_FAIL doDenied returned Err but not Security: {other:?}");
            false
        }
        Ok(_) => {
            eprintln!("STAGE3_4_2_FAIL doDenied returned Ok — check did not fire");
            false
        }
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let broker = match kernel::Broker::new() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("kernel::Broker init failed: {e}");
            return ExitCode::from(2);
        }
    };

    let svc = match broker.get_interface::<dyn IPermCheck>(SERVICE_NAME) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lookup/interface_cast failed: {e:?}");
            return ExitCode::from(4);
        }
    };

    let mut ok = true;
    ok &= expect_ok_true("doSingle", svc.doSingle());
    ok &= expect_ok_true("doAllOf", svc.doAllOf());
    ok &= expect_ok_true("doAnyOf", svc.doAnyOf());
    ok &= expect_security_denial(svc.doDenied());

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
