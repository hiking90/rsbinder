// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `wait_for_interface_async` against a live service manager
//! (plan 11-1 AC-11-1.1, AC-11-1.3, AC-11-1.6).
//!
//! The wait is a service-manager feature, so none of this is hermetic: it
//! needs a kernel binder device and a running service manager (`rsb_hub` on
//! Linux, `servicemanager` on Android). Hence `#[ignore]` — run it explicitly
//! on a binder-capable host:
//!
//! ```text
//! sudo target/debug/rsb_device binder --group "$(id -gn)" --mode 0660
//! target/debug/rsb_hub --config tests/policy/permissive.toml &
//! cargo test --package tests --test async_wait -- --ignored --nocapture
//! ```
//!
//! What each test pins:
//!
//!   * the **event path**, end to end: a callback registered with the service
//!     manager receives `onRegistration`, and a wait started before the name
//!     existed resolves to a working handle;
//!   * the **re-poll** path, in a child process with no thread pool, where
//!     `onRegistration` has no looper to arrive on;
//!   * **cancellation**, by giving a runtime exactly one blocking thread: the
//!     wait occupies it, and dropping the future has to give it back;
//!   * that a dropped wait leaves **no callback registered**, using the
//!     per-name callback cap as the instrument;
//!   * that both async lookups are `Send + 'static` (compile-time).
//!
//! None of them asserts a wall-clock duration. Which of the two mechanisms
//! answered a given wait is not observable from here, and timing it is not a
//! substitute — see plan 11-1 §8.5.

#![cfg(any(target_os = "linux", target_os = "android"))]
#![allow(non_snake_case)]

use std::time::{Duration, Instant};

use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/async_rt.rs"));

use asyncrt::IAsyncRt::{BnAsyncRt, IAsyncRt};

/// Unique per process, so parallel test binaries never collide on a name.
fn service_name(tag: &str) -> String {
    format!("rsb.test.asyncwait.{tag}.{}", std::process::id())
}

struct Svc;
impl Interface for Svc {}

impl IAsyncRt for Svc {
    fn r#echo(&self, msg: &str) -> rsbinder::BinderResult<String> {
        Ok(format!("echo:{msg}"))
    }
}

/// AC-11-1.1(a) — the wait wakes on `onRegistration`, not on the next re-poll
/// tick.
///
/// Ten rounds, each registering 300ms into a wait that is already blocked.
/// The notification arrives within milliseconds; a wait that only re-polled
/// would answer at its next second boundary, so the delay between
/// `add_service` returning and the future completing separates the two
/// AC-11-1.1(a) — registration notifications reach this process, and a wait
/// started before the service existed resolves to a working handle.
///
/// Two claims, neither of them timed:
///
///   1. the event path is wired end to end — a callback this test registers
///      with the service manager receives `onRegistration` once the name is
///      added, which is the same wire the wait's internal waiter sits on;
///   2. a wait that missed on its fast path resolves after the registration
///      and yields a handle that answers a call.
///
/// What it deliberately does **not** claim is that the wait's own answer came
/// from the notification rather than from its per-second re-poll. That
/// distinction was asserted by comparing wall-clock stamps, and the clock is
/// not a usable discriminator: on an emulator the same code completes one
/// round in 1.5ms and another in 2.3s, because thread wake-ups there are late
/// by tens to hundreds of milliseconds (a plain 200ms sleep measures
/// 225-375ms). Asserting it properly needs the wait to report which branch
/// answered, which is a counter behind `test-util` that the library does not
/// have yet; see plan 11-1 §8.5.
///
/// Mutant: a `register_for_notifications` that does not reach the service
/// manager, or a service manager that stops fanning out `onRegistration`,
/// leaves the callback below unfired. A broken wait leaves claim 2 unmet.
#[test]
#[ignore = "needs a kernel binder device and a running service manager"]
fn registration_notifies_this_client_and_the_wait_resolves() {
    use rsbinder::hub::android_16::android::os::IServiceCallback::{
        BnServiceCallback, IServiceCallback,
    };

    /// Reports the name it was told about, on the binder thread it arrives on.
    struct Watcher(std::sync::mpsc::SyncSender<String>);
    impl Interface for Watcher {}
    impl IServiceCallback for Watcher {
        fn onRegistration(&self, name: &str, _binder: &SIBinder) -> rsbinder::BinderResult<()> {
            let _ = self.0.try_send(name.to_owned());
            Ok(())
        }
    }

    ProcessState::init_default().expect("ProcessState::init_default");
    ProcessState::start_thread_pool();

    let sm = hub::default().expect("service manager");
    let name = service_name("notify");

    // Registered before the name exists, so the callback can only fire off the
    // registration below.
    let (tx_fired, rx_fired) = std::sync::mpsc::sync_channel::<String>(1);
    let watcher = BnServiceCallback::new_binder(Watcher(tx_fired));
    sm.register_for_notifications(&name, &watcher)
        .expect("register_for_notifications");

    // The service stays alive for the whole test: the service manager holds a
    // reference, and dropping the local object underneath it would only add a
    // way for the assertions below to fail.
    let service = BnAsyncRt::new_binder(Svc);

    // Registered after the wait has started. There is no window to hit any
    // more — nothing here depends on whether the wait had reached its blocking
    // point first — so this only has to happen while the wait is outstanding.
    let (tx_added, rx_added) = std::sync::mpsc::sync_channel::<std::result::Result<(), String>>(1);
    let reg_name = name.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let added = hub::add_service(&reg_name, &service).map_err(|e| format!("{e:?}"));
        let _ = tx_added.try_send(added);
        // The thread ends here; the result travels by channel rather than by
        // `join`, so a service manager that never answers cannot wedge the
        // test binary on an unbounded join.
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    // Bounded only so a registration that never lands fails instead of parking
    // this `block_on` forever. It is not a measurement.
    let waited = rt.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(20),
            wait_for_interface_async::<dyn IAsyncRt>(&name),
        )
        .await
    });

    // `Runtime::drop` waits for blocking tasks, so a wait that ignored its
    // cancellation would hold the test binary open through the unwind of any
    // assertion below. Shut down first, bounded.
    rt.shutdown_timeout(Duration::from_secs(2));

    // The registration is checked first, so a denied `add_service` is reported
    // as itself rather than as the wait timeout it causes.
    rx_added
        .recv_timeout(Duration::from_secs(10))
        .expect("the registrar never reported back")
        .expect("add_service");

    let svc = waited
        .expect("the wait never resolved, though the registration succeeded")
        .expect("wait_for_interface_async");
    assert_eq!(svc.r#echo("hi").expect("echo"), "echo:hi");

    // Claim 1. Generous: what is being pinned is that it arrives at all.
    let fired = rx_fired
        .recv_timeout(Duration::from_secs(10))
        .expect("onRegistration never reached this process");
    assert_eq!(fired, name, "the notification named a different service");
}

/// AC-11-1.1(c) — a client with no binder thread pool still finds the service.
///
/// Runs in a child process: the thread pool is process-global and the other
/// tests here start it. Without a pool `onRegistration` has no looper to
/// arrive on, so the per-second re-poll inside the wait is the only thing that
/// can resolve it — the same degradation the synchronous path has.
///
/// Mutant: replacing the re-poll in `wait_for_service_cancellable`'s loop with
/// a bare condvar wait makes the child time out.
#[test]
#[ignore = "needs a kernel binder device and a running service manager"]
fn a_client_without_a_thread_pool_still_finds_the_service() {
    const CHILD_ENV: &str = "RSB_ASYNC_WAIT_NOPOOL";

    if let Ok(name) = std::env::var(CHILD_ENV) {
        ProcessState::init_default().expect("ProcessState::init_default");
        // Deliberately no `start_thread_pool()`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let found = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(4),
                wait_for_interface_async::<dyn IAsyncRt>(&name),
            )
            .await
        });
        assert!(
            matches!(found, Ok(Ok(_))),
            "a client without a thread pool must still resolve the wait: {found:?}"
        );
        std::process::exit(0);
    }

    ProcessState::init_default().expect("ProcessState::init_default");
    ProcessState::start_thread_pool();

    let name = service_name("nopool");
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = std::process::Command::new(exe)
        .args([
            "--exact",
            "a_client_without_a_thread_pool_still_finds_the_service",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD_ENV, &name)
        .spawn()
        .expect("spawn child");

    // Register after the child is waiting.
    std::thread::sleep(Duration::from_millis(500));
    let service = BnAsyncRt::new_binder(Svc);
    hub::add_service(&name, &service).expect("add_service");

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the child never resolved the wait");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    assert!(status.success(), "child failed: {status:?}");
}

/// AC-11-1.3 — dropping the future returns the blocking thread.
///
/// A runtime with a single blocking thread makes the occupancy observable
/// without `tokio_unstable` metrics: while the wait is running, a second
/// blocking task cannot start; once the future is dropped, it can. The first
/// half is also what the API documents as the cost of a wait.
///
/// Mutant: dropping the `cancel()` from `CancelWaitOnDrop`, or checking the
/// flag only once per tick instead of waking the condvar, leaves the slot
/// occupied and the second assertion times out.
#[test]
#[ignore = "needs a kernel binder device and a running service manager"]
fn dropping_the_wait_returns_the_blocking_thread() {
    ProcessState::init_default().expect("ProcessState::init_default");
    ProcessState::start_thread_pool();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("runtime");

    // `rsbinder::Result` is a one-parameter alias, so this spells out std's.
    let outcome: std::result::Result<(), String> = rt.block_on(async {
        // A name nobody ever registers: the wait blocks until cancelled.
        let name = service_name("never");
        let waiting = tokio::spawn(async move {
            let _ = wait_for_interface_async::<dyn IAsyncRt>(&name).await;
        });

        // Let the spawned task reach its `spawn_blocking`. Without this the
        // probe below can be queued first, take the free thread, and report
        // occupancy that never happened — the blocking pool is first-come.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let occupied = tokio::time::timeout(
            Duration::from_millis(500),
            tokio::task::spawn_blocking(|| ()),
        )
        .await;
        if occupied.is_ok() {
            return Err("the wait should hold the runtime's only blocking thread".into());
        }

        waiting.abort();
        let _ = waiting.await;

        match tokio::time::timeout(Duration::from_secs(3), tokio::task::spawn_blocking(|| ())).await
        {
            Ok(joined) => joined.map_err(|e| format!("blocking task: {e:?}")),
            Err(_) => Err("the blocking thread must come back once the wait is dropped".into()),
        }
    });

    // A wait that ignored the cancellation keeps its blocking thread forever,
    // and `Runtime::drop` waits for blocking tasks — so a regression here
    // would wedge the whole test binary instead of failing. Bound it.
    rt.shutdown_timeout(Duration::from_secs(2));
    outcome.expect("blocking thread was not reclaimed");
}

/// AC-11-1.1(b) — a dropped wait leaves no callback registered.
///
/// The service manager's callback list is not readable from here, so the
/// observation goes through `rsb_hub`'s per-name cap: it refuses a
/// registration once a name already holds `MAX_CALLBACKS_PER_NAME` (256) of
/// them. Filling the list to one below the cap makes the next single slot the
/// instrument — the wait takes it, and a wait that unregisters on drop gives it
/// back.
///
/// Linux/`rsb_hub` only in substance: Android's `servicemanager` has no such
/// cap (AOSP gates callers by uid/SELinux instead), so there the instrument
/// cannot be armed and the test says so and returns rather than reporting a
/// pass it did not measure. It is listed for the Linux gate.
///
/// Mutant: removing `UnregisterOnDrop` from the cancelled return path (or
/// returning without dropping it) keeps the list at the cap, and the final
/// registration stays rejected for the whole retry window.
#[test]
#[ignore = "needs a kernel binder device and a running rsb_hub"]
fn a_dropped_wait_leaves_no_callback_registered() {
    use rsbinder::hub::android_16::android::os::IServiceCallback::{
        BnServiceCallback, IServiceCallback,
    };

    /// A callback that exists only to occupy a slot in the list.
    struct Occupant;
    impl Interface for Occupant {}
    impl IServiceCallback for Occupant {
        fn onRegistration(&self, _name: &str, _binder: &SIBinder) -> rsbinder::BinderResult<()> {
            Ok(())
        }
    }

    /// `rsb_hub`'s `MAX_CALLBACKS_PER_NAME`.
    const CAP: usize = 256;

    ProcessState::init_default().expect("ProcessState::init_default");
    ProcessState::start_thread_pool();

    let sm = hub::default().expect("service manager");
    let name = service_name("leak");

    // Hold the callbacks: the list in rsb_hub keeps its own references, but
    // dropping ours would only invite a death-driven prune mid-test.
    let mut occupants = Vec::new();
    for i in 0..(CAP - 1) {
        let callback = BnServiceCallback::new_binder(Occupant);
        sm.register_for_notifications(&name, &callback)
            .unwrap_or_else(|e| panic!("occupant {i} could not register: {e:?}"));
        occupants.push(callback);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    // Registered only to read the cap, never to occupy a slot for long: a
    // refusal leaves the list untouched, and an acceptance ends the test.
    let arming_probe = BnServiceCallback::new_binder(Occupant);

    let armed = rt.block_on(async {
        let waiting_name = name.clone();
        let waiting = tokio::spawn(async move {
            let _ = wait_for_interface_async::<dyn IAsyncRt>(&waiting_name).await;
        });
        // Long enough to have registered the 256th callback — the list is at
        // the cap while this wait is alive.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Read the instrument before using it. Without the wait's own callback
        // the list is one short of the cap, and every registration below is
        // accepted whether or not anything unregistered.
        let armed = sm.register_for_notifications(&name, &arming_probe).is_err();
        waiting.abort();
        let _ = waiting.await;
        armed
    });
    // Same reason as in the test above: do not let a wait that ignored its
    // cancellation hold the runtime's shutdown (and the test binary) open.
    rt.shutdown_timeout(Duration::from_secs(2));

    if !armed {
        // Android's `servicemanager` has no per-name cap, so the probe is
        // accepted there no matter what the wait did — vacuous rather than
        // failing. On `rsb_hub` the cap exists, so an unarmed instrument means
        // the wait never registered and there is nothing to measure.
        if cfg!(target_os = "android") {
            println!(
                "skipped: no per-name callback cap on this service manager, so the unregister \
                 cannot be observed from here"
            );
            return;
        }
        panic!(
            "the wait registered no callback: the list never reached the cap ({CAP}), \
             so nothing below observes the unregister"
        );
    }

    // Aborting the task drops the future, which cancels the wait; the
    // unregister is a wire call the blocking thread makes on its way out, so
    // retry rather than assume it has already landed. On a leak every attempt
    // is refused and the loop runs out.
    let extra = BnServiceCallback::new_binder(Occupant);
    let mut last = None;
    for _ in 0..10 {
        match sm.register_for_notifications(&name, &extra) {
            Ok(()) => return,
            Err(err) => last = Some(err),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("the dropped wait kept its callback registered: the list stayed at the cap ({last:?})");
}

/// AC-11-1.6 — both futures are `Send + 'static`, so they can be spawned and
/// used as `select!` arms. A compile-time check; the assertion is that this
/// builds.
#[test]
fn the_async_lookups_are_send_and_static() {
    fn assert_send_static<T: Send + 'static>(_: T) {}

    assert_send_static(async {
        let _ = wait_for_interface_async::<dyn IAsyncRt>("unused").await;
    });
    assert_send_static(async {
        let _ = check_interface_async::<dyn IAsyncRt>("unused").await;
    });
}
