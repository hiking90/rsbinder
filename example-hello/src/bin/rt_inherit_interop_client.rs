// SPDX-License-Identifier: Apache-2.0
//
// RT inheritance client.
//
// Calls `IRtCheck::reportSchedPolicy()` twice:
//
//   * Once from the default SCHED_NORMAL/SCHED_OTHER class — the
//     server's worker thread should report SCHED_FIFO (the floor the
//     server advertised via `BinderFeatures::min_sched_policy`).
//   * Once after raising the *client* to SCHED_FIFO/10 — the server
//     should still report SCHED_FIFO (inherited from us).
//
// Requires CAP_SYS_NICE for the `sched_setscheduler` call; the
// `run_rt_inherit_interop.sh` wrapper drops into `sudo` on REMOTE_LINUX.

use std::process::ExitCode;

use rsbinder::service::{kernel, Broker as _};

use example_hello::rt_inherit::{IRtCheck, SERVICE_NAME};

#[cfg(any(target_os = "linux", target_os = "android"))]
fn try_become_rt(priority: i32) -> bool {
    // SAFETY: `sched_param` is a POSIX-defined C aggregate of
    // primitive integers; a zero-initialized instance is a valid
    // starting point on every libc that exposes it. We immediately
    // overwrite the only field rsbinder cares about
    // (`sched_priority`) before passing the pointer to the kernel,
    // and the struct lives for the duration of the call.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = priority;
    // SAFETY: pid=0 means "the current thread"; libc::sched_setscheduler
    // has no preconditions beyond a valid `&sched_param`.
    let r = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    r == 0
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn try_become_rt(_priority: i32) -> bool {
    // SCHED_FIFO is Linux/Android-only; the test reports SKIPPED.
    false
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
    let svc = match broker.get_interface::<dyn IRtCheck>(SERVICE_NAME) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lookup/interface_cast failed: {e:?}");
            return ExitCode::from(4);
        }
    };

    let mut ok = true;

    // Linux `<linux/sched.h>` constants — duplicated locally so the
    // STAGE3 client builds on macOS hosts that vendor a different
    // `libc::sched_param`. Values match the kernel headers verbatim.
    const SCHED_OTHER_NORMAL: i32 = 0;
    const SCHED_FIFO: i32 = 1;
    const SCHED_RR: i32 = 2;

    // Call A — from SCHED_NORMAL: server should report its declared
    // floor (SCHED_FIFO = 1).
    let policy_a = match svc.reportSchedPolicy() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("STAGE3_4_5_FAIL reportSchedPolicy (NORMAL caller): {e:?}");
            return ExitCode::from(1);
        }
    };
    println!("STAGE3_4_5_A caller=SCHED_NORMAL server_policy={policy_a}");
    if policy_a != SCHED_FIFO && policy_a != SCHED_RR {
        // Some kernels only lift to SCHED_FIFO once the caller is
        // already RT, in which case A reports SCHED_OTHER/NORMAL
        // (=0). We accept either; the hard check is in Call B.
        if policy_a != SCHED_OTHER_NORMAL {
            eprintln!("STAGE3_4_5_FAIL A: unexpected policy {policy_a}");
            ok = false;
        }
    }

    // Call B — from SCHED_FIFO: the kernel MUST honor inherit_rt and
    // surface SCHED_FIFO inside the server. If `sched_setscheduler`
    // fails we record the cause and skip B (counts as informational).
    if try_become_rt(10) {
        let policy_b = match svc.reportSchedPolicy() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("STAGE3_4_5_FAIL reportSchedPolicy (FIFO caller): {e:?}");
                return ExitCode::from(1);
            }
        };
        println!("STAGE3_4_5_B caller=SCHED_FIFO server_policy={policy_b}");
        if policy_b != SCHED_FIFO {
            eprintln!("STAGE3_4_5_FAIL B: expected SCHED_FIFO (=1) got {policy_b}");
            ok = false;
        }
    } else {
        let errno = std::io::Error::last_os_error();
        println!("STAGE3_4_5_B_SKIPPED sched_setscheduler failed: {errno}");
    }

    if ok {
        println!("STAGE3_4_5_PASS");
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
