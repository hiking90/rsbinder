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
//!   * the **notification** path, by measuring how long after registration the
//!     future completes — the per-second re-poll would show up as a wake
//!     hundreds of milliseconds late;
//!   * the **re-poll** path, in a child process with no thread pool, where
//!     `onRegistration` has no looper to arrive on;
//!   * **cancellation**, by giving a runtime exactly one blocking thread: the
//!     wait occupies it, and dropping the future has to give it back.

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
/// mechanisms. The per-round bound is what makes this a measurement rather
/// than a coin flip: a re-poll lands uniformly in the following second, so
/// ten rounds under 100ms each is not something the polling path produces.
///
/// Mutant: deleting the `register_for_notifications` call (so every wait falls
/// back to `poll_for_service`) pushes the delay to ~700ms and fails here.
#[test]
#[ignore = "needs a kernel binder device and a running service manager"]
fn the_wait_wakes_on_the_registration_notification() {
    ProcessState::init_default().expect("ProcessState::init_default");
    ProcessState::start_thread_pool();

    // Registered services stay alive for the whole test: the service manager
    // holds a reference, and dropping the local object underneath it would
    // only add a way for a later round to fail.
    let mut registered = Vec::new();

    for round in 0..10 {
        let name = service_name(&format!("notify{round}"));
        let service = BnAsyncRt::new_binder(Svc);
        registered.push(service.clone());

        let (tx, rx) = std::sync::mpsc::sync_channel::<Instant>(1);
        let reg_name = name.clone();
        let registrar = std::thread::spawn(move || {
            // Long enough that the wait is past its fast path and blocked on
            // the condvar, short enough to stay under the 1s re-poll tick.
            std::thread::sleep(Duration::from_millis(300));
            let added = hub::add_service(&reg_name, &service);
            if added.is_ok() {
                let _ = tx.try_send(Instant::now());
            }
            added
        });

        // One runtime per round, so the shutdown below is reached before this
        // round's assertions can panic.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");

        // The wait is unbounded, so a registration that never happens (an
        // `add_service` the service manager denies, say) would park this
        // `block_on` forever and wedge the whole test binary instead of
        // failing. The bound is two orders of magnitude above the 100ms the
        // measurement below asserts, so it never decides that question.
        let waited = rt.block_on(async {
            let res = tokio::time::timeout(
                Duration::from_secs(10),
                wait_for_interface_async::<dyn IAsyncRt>(&name),
            )
            .await;
            (res, Instant::now())
        });

        // Same reason as the two tests below: `Runtime::drop` waits for
        // blocking tasks, so a wait that ignored its cancellation would hold
        // the test binary open — here through the unwind of any assertion
        // below, which is why the shutdown comes first and is bounded.
        rt.shutdown_timeout(Duration::from_secs(2));

        // Joined before the wait's own result is unwrapped, so a failed
        // registration is reported as itself rather than as the timeout it
        // causes.
        registrar
            .join()
            .expect("registrar thread")
            .expect("add_service");

        let (waited, found_at) = waited;
        let svc = waited
            .expect("the wait never resolved, though the registration succeeded")
            .expect("wait_for_interface_async");
        // The lookup really did produce a working handle.
        assert_eq!(svc.r#echo("hi").expect("echo"), "echo:hi");

        let registered_at = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("registration timestamp");
        let delay = found_at.saturating_duration_since(registered_at);
        assert!(
            delay < Duration::from_millis(100),
            "round {round}: the wait completed {delay:?} after registration — \
             that is the re-poll tick, not the notification"
        );
    }
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
