// SPDX-License-Identifier: Apache-2.0
//
// Client harness — drives the server
// (`calling_identity_interop_service`) and the `permission_controller` stub against
// real Android emulator (kernel binder + system_server).
//
// Usage:
//   calling_identity_interop_client                  # both checks
//   calling_identity_interop_client describe         # just the describeCaller round-trip
//   calling_identity_interop_client check_permission # just the IPermissionController probe
//
// Exit codes: 0 on PASS, non-zero on any failure (mismatched SID format,
// unreachable service, etc.). Designed to be driven from
// `example-hello/cpp/run_calling_identity_interop.sh`.

use std::process::ExitCode;

use rsbinder::*;

use example_hello::calling_identity::{ICallingIdentity, SERVICE_NAME};

/// Round-trip: call into the server's `describeCaller`,
/// print the result, and PASS only when the server reported a non-empty
/// SELinux context (`u:r:*:s0...`) and the explicit-identity
/// before/after-clear/after-restore triple matched `false / true /
/// false`.
fn describe_caller_round_trip() -> Result<()> {
    let binder = hub::get_service(SERVICE_NAME).ok_or(StatusCode::NameNotFound)?;
    let svc = <dyn ICallingIdentity>::try_from(binder).map_err(|e| {
        eprintln!("interface_cast failed: {e:?}");
        StatusCode::BadType
    })?;

    let line = svc.describeCaller().map_err(|status| {
        eprintln!("describeCaller failed: {status:?}");
        StatusCode::Unknown
    })?;
    println!("STAGE3_4_1_DESCRIBE: {line}");

    // Parse the line and validate.
    let mut sid: Option<String> = None;
    let mut explicit_pre: Option<bool> = None;
    let mut explicit_after_clear: Option<bool> = None;
    let mut explicit_after_restore: Option<bool> = None;
    for tok in line.split_whitespace() {
        if let Some(v) = tok.strip_prefix("sid=") {
            sid = Some(v.to_owned());
        } else if let Some(v) = tok.strip_prefix("explicit_pre=") {
            explicit_pre = v.parse().ok();
        } else if let Some(v) = tok.strip_prefix("explicit_after_clear=") {
            explicit_after_clear = v.parse().ok();
        } else if let Some(v) = tok.strip_prefix("explicit_after_restore=") {
            explicit_after_restore = v.parse().ok();
        }
    }

    let sid = sid.ok_or_else(|| {
        eprintln!("missing sid= in reply");
        StatusCode::BadValue
    })?;
    let explicit_pre = explicit_pre.ok_or(StatusCode::BadValue)?;
    let explicit_after_clear = explicit_after_clear.ok_or(StatusCode::BadValue)?;
    let explicit_after_restore = explicit_after_restore.ok_or(StatusCode::BadValue)?;

    // Explicit-identity must round-trip
    // false → true → false across clear → restore.
    if explicit_pre || !explicit_after_clear || explicit_after_restore {
        eprintln!(
            "explicit_identity round-trip wrong: pre={explicit_pre} \
             after_clear={explicit_after_clear} \
             after_restore={explicit_after_restore}"
        );
        return Err(StatusCode::BadValue);
    }

    // When the server's binder requested SEC_CTX, the kernel
    // must deliver a SELinux context string (`u:r:<domain>:s0[:...]`).
    if !sid.starts_with("u:r:") || !sid.contains(":s0") {
        eprintln!("SID does not match SELinux context shape: {sid:?}");
        return Err(StatusCode::BadValue);
    }
    println!("STAGE3_4_1_DESCRIBE_PASS sid={sid}");
    Ok(())
}

/// Probe: try to acquire the system-wide `permission` service
/// and call `checkPermission("android.permission.INTERNET", ...)` for
/// the current process. Either result is acceptable PASS (true/false) —
/// what we are proving is that the AIDL stub round-trips wire-correctly
/// with real `system_server`, not the policy outcome.
fn check_permission_round_trip() -> Result<()> {
    let pc = rsbinder::permission_controller::default()?;
    let my_pid = rustix::process::getpid().as_raw_nonzero().get() as i32;
    let my_uid = rustix::process::getuid().as_raw() as i32;
    let granted = pc
        .checkPermission("android.permission.INTERNET", my_pid, my_uid)
        .map_err(|status| {
            eprintln!("checkPermission failed: {status:?}");
            StatusCode::Unknown
        })?;
    println!(
        "STAGE3_4_1_PERMISSION_PASS pid={my_pid} uid={my_uid} \
         android.permission.INTERNET={granted}"
    );
    Ok(())
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if let Err(e) = ProcessState::init_default() {
        eprintln!("init_default failed: {e}");
        return ExitCode::from(2);
    }

    let mode = std::env::args().nth(1).unwrap_or_else(|| "all".to_owned());
    let mut ok = true;

    if mode == "describe" || mode == "all" {
        if let Err(e) = describe_caller_round_trip() {
            eprintln!("describe_caller_round_trip failed: {e:?}");
            ok = false;
        }
    }
    if mode == "check_permission" || mode == "all" {
        if let Err(e) = check_permission_round_trip() {
            eprintln!("check_permission_round_trip failed: {e:?}");
            ok = false;
        }
    }

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
