// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-0 §3.4 — a kernel option the process cannot honor is
//! [`StatusCode::BadValue`], not a warning.
//!
//! The kernel thread pool is fixed by whoever initializes the
//! process-wide `ProcessState` first and cannot be changed afterwards.
//! A second `serve` naming a *different* size used to be warned about and
//! then ignored, which left the caller believing a setting it never got.
//! Every other inapplicable option in this layer is refused, and this one
//! now is too.
//!
//! Order matters and the process is the fixture: the first `serve` here
//! decides the pool for the whole binary, so every case — the value
//! already in force, a different one, the option omitted, and the same
//! rule through `ServeOptions::threads` and `ClientOptions::driver` — must
//! run in one test, in this order. A second test touching `ProcessState`
//! would see whatever this one left behind.
//!
//! Needs a real binder device (`/dev/binder`, Linux + binderfs or
//! Android) — but no service manager, since nothing is registered. Hence
//! `#[ignore]`:
//!
//! ```text
//! cargo test -p tests --test entry_kernel_options -- --ignored --nocapture
//! ```

#![cfg(any(target_os = "linux", target_os = "android"))]

use rsbinder::StatusCode;

#[test]
#[ignore = "requires kernel binder (/dev/binder); run on REMOTE_LINUX/emulator"]
fn a_kernel_option_the_process_cannot_honor_is_refused() {
    // First in the process: this one decides the pool size.
    rsbinder::serve("binder://?threads=4").expect("the first serve initializes ProcessState");

    // Asking again for what is already true is not an error — the caller
    // gets exactly what it asked for.
    rsbinder::serve("binder://?threads=4").expect("the same value must be accepted");

    // A different value cannot be applied, and saying so is the point:
    // the pool stays at 4 either way, but now the caller is told.
    assert_eq!(
        rsbinder::serve("binder://?threads=8").err(),
        Some(StatusCode::BadValue),
        "a different thread count must be refused, not warned about"
    );

    // Omitting the option asks for nothing, so it always succeeds — this
    // is what keeps a second server in the same process working.
    rsbinder::serve("binder://").expect("no `?threads=` asks for nothing");

    // The same rule through `ServeOptions`, which is applied later (at
    // `spawn`) than the URI form (at `serve`).
    let err = rsbinder::serve("binder://")
        .expect("parse")
        .with(|o| o.threads = Some(8))
        .spawn()
        .err();
    assert_eq!(
        err,
        Some(StatusCode::BadValue),
        "ServeOptions::threads must follow the same rule as `?threads=`"
    );

    // And the client half shares the one check.
    assert_eq!(
        rsbinder::Client::open_with("binder://", |o, _| {
            o.driver = Some("/dev/definitely-not-the-binder-in-use".into())
        })
        .err(),
        Some(StatusCode::BadValue),
        "a different driver must be refused on the client path too"
    );
}
