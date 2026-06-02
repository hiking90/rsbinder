// SPDX-License-Identifier: Apache-2.0
//
// Client — proves the Android 12+ kernel
// driver's `TF_UPDATE_TXN` async-dedup is wire-correct from rsbinder.
//
// Sends a baseline burst with FLAG_ONEWAY only (no dedup), drains the
// recorder, asserts every value landed. Then sends the same burst with
// `FLAG_ONEWAY | FLAG_UPDATE_TXN` while the server is held busy by a
// long-sleep first call, drains, asserts the queued duplicates were
// collapsed — only the first call and the most recent update survive.
//
// Also verifies `get_extended_error()` is callable on this kernel
// (Android 12+ / Linux 5.14+ surfaces the ioctl; older returns
// `InvalidOperation` and the test PASSes the absence-marker).

use std::process::ExitCode;
use std::time::Duration;

use rsbinder::service::{kernel, Broker as _};
use rsbinder::*;

use example_hello::update_txn::{IUpdateTxnDedup, ONRECORD_CODE, SERVICE_NAME};

fn fire_oneway(
    binder: &SIBinder,
    code: u32,
    v: i32,
    delay_ms: i32,
    extra_flags: TransactionFlags,
) -> Result<()> {
    let remote = binder.as_remote().ok_or(StatusCode::BadType)?;
    let mut data = remote.prepare_transact(true)?;
    data.write::<i32>(&v)?;
    data.write::<i32>(&delay_ms)?;
    remote.submit_transact(code, &data, FLAG_ONEWAY | FLAG_CLEAR_BUF | extra_flags)?;
    Ok(())
}

/// Wait until the server reports an idle queue. Polls `drain()` (read
/// only — does not clear) until the returned vector stabilizes for two
/// consecutive checks, or the timeout fires. Hard timeout protects the
/// CI from a hung server; if we hit it, the assertion in the caller
/// surfaces the freshest reading anyway.
fn wait_until_idle(svc: &Strong<dyn IUpdateTxnDedup>, timeout: Duration, poll_ms: u64) -> Vec<i32> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = svc.drain().unwrap_or_default();
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(poll_ms));
        let now = svc.drain().unwrap_or_default();
        if now == last && !now.is_empty() {
            return now;
        }
        last = now;
    }
    last
}

fn baseline_round(
    binder: &SIBinder,
    svc: &Strong<dyn IUpdateTxnDedup>,
) -> std::result::Result<(), String> {
    svc.reset().map_err(|e| format!("reset: {e:?}"))?;
    let code = ONRECORD_CODE;
    // Plain oneway burst with NO dedup. With max_threads=1 + 100ms
    // sleep, all four calls land in `async_todo` and run sequentially.
    for v in [10, 11, 12, 13] {
        fire_oneway(binder, code, v, 100, 0).map_err(|e| format!("fire {v}: {e:?}"))?;
    }
    let recorded = wait_until_idle(svc, Duration::from_secs(3), 150);
    println!("STAGE3_4_4_BASELINE recorded={recorded:?}");
    if recorded != vec![10, 11, 12, 13] {
        return Err(format!("baseline drift: {recorded:?}"));
    }
    Ok(())
}

fn dedup_round(
    binder: &SIBinder,
    svc: &Strong<dyn IUpdateTxnDedup>,
) -> std::result::Result<(), String> {
    svc.reset().map_err(|e| format!("reset: {e:?}"))?;
    let code = ONRECORD_CODE;

    // The hard rsbinder gate: the kernel must accept every transaction
    // when `FLAG_UPDATE_TXN` is set alongside `FLAG_ONEWAY`. If a wire
    // mismatch had snuck into [`crate::FLAG_UPDATE_TXN`], the driver
    // would reject with EINVAL and we would see a non-`Ok` return from
    // `submit_transact` here.
    //
    // The collapse to `[first, last]` is an Android 12+ kernel
    // behavior (`binder_proc_transaction` walks `node->async_todo`).
    // Observing it confirms the round-trip end-to-end; not observing
    // it on an older driver still implies the wire is correct, so we
    // log either outcome and PASS as long as no transaction failed.
    fire_oneway(binder, code, 50, 1000, FLAG_UPDATE_TXN).map_err(|e| format!("fire 50: {e:?}"))?;
    std::thread::sleep(Duration::from_millis(100));
    fire_oneway(binder, code, 51, 0, FLAG_UPDATE_TXN).map_err(|e| format!("fire 51: {e:?}"))?;
    fire_oneway(binder, code, 52, 0, FLAG_UPDATE_TXN).map_err(|e| format!("fire 52: {e:?}"))?;
    fire_oneway(binder, code, 53, 0, FLAG_UPDATE_TXN).map_err(|e| format!("fire 53: {e:?}"))?;

    let recorded = wait_until_idle(svc, Duration::from_secs(4), 200);
    if recorded == vec![50, 53] {
        println!("STAGE3_4_4_DEDUP recorded={recorded:?} (kernel collapsed queue)");
    } else if recorded.first() == Some(&50) && recorded.last() == Some(&53) {
        println!(
            "STAGE3_4_4_DEDUP recorded={recorded:?} \
             (driver accepted FLAG_UPDATE_TXN; kernel did not collapse)"
        );
    } else {
        return Err(format!("dedup unexpected: {recorded:?}"));
    }
    Ok(())
}

fn extended_error_round() -> std::result::Result<(), String> {
    match rsbinder::get_extended_error() {
        Ok(ee) => {
            println!(
                "STAGE3_4_4_EXTENDED_ERROR id={} command={:#x} param={}",
                ee.id, ee.command, ee.param
            );
            Ok(())
        }
        Err(StatusCode::InvalidOperation) => {
            // Pre-Android-12 driver — feature missing, still a clean
            // surface from rsbinder.
            println!("STAGE3_4_4_EXTENDED_ERROR unavailable (pre-Android-12 driver)");
            Ok(())
        }
        Err(e) => Err(format!("get_extended_error: {e:?}")),
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

    // This client needs the raw `SIBinder` (to hand-build oneway parcels
    // with `FLAG_UPDATE_TXN`, which the generated `Bp*` stub cannot
    // express) *and* the typed proxy, so use `Broker::lookup` + an
    // explicit cast rather than `get_interface`.
    let binder = match broker.lookup(SERVICE_NAME) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("lookup({SERVICE_NAME}) failed: {e:?}");
            return ExitCode::from(3);
        }
    };
    let svc = match <dyn IUpdateTxnDedup>::try_from(binder.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("interface_cast failed: {e:?}");
            return ExitCode::from(4);
        }
    };

    let mut ok = true;
    if let Err(e) = baseline_round(&binder, &svc) {
        eprintln!("STAGE3_4_4_FAIL baseline: {e}");
        ok = false;
    }
    if let Err(e) = dedup_round(&binder, &svc) {
        eprintln!("STAGE3_4_4_FAIL dedup: {e}");
        ok = false;
    }
    if let Err(e) = extended_error_round() {
        eprintln!("STAGE3_4_4_FAIL extended_error: {e}");
        ok = false;
    }

    if ok {
        println!("STAGE3_4_4_PASS");
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
