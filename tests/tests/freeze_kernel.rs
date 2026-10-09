// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 4-3 Phase D — freeze notifications against a real binder driver, so
//! `#[ignore]` (run with `--ignored` on a binder-capable host):
//!
//! ```text
//! target/debug/rsb_hub --config tests/policy/permissive.toml &
//! cargo test -p tests --features rpc --test freeze_kernel -- --ignored --test-threads=1
//! ```
//!
//! The observed process is a `reconnect_service` of this test's own name;
//! this process registers the callbacks and freezes it with
//! `ProcessState::freeze_process`, which the driver allows any binder process
//! to issue. A driver without `features/freeze_notification` is not a
//! failure: the tests then check the refusal (`InvalidOperation`) and stop.
//!
//! The protocol is between the kernel and this process only, so no libbinder
//! peer is needed; what matters is the driver (Rust binder on REMOTE_LINUX,
//! the C driver on an emulator). On a device, push `reconnect_service` next
//! to the test binary and point `RSB_RECONNECT_SERVICE` at it.

#![cfg(any(target_os = "linux", target_os = "android"))]
#![allow(non_snake_case)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/rpc_smoke.rs"));

use rpcsmoke::IRpcSmoke::IRpcSmoke;

const WAIT: Duration = Duration::from_secs(10);

fn service_name(tag: &str) -> String {
    format!("rsb.test.freeze.{tag}.{}", std::process::id())
}

fn service_bin() -> std::ffi::OsString {
    std::env::var_os("RSB_RECONNECT_SERVICE")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_reconnect_service").into())
}

/// `reconnect_service NAME TAG`, returned once registered; killed on drop.
struct Service(Child);
impl Service {
    fn start(name: &str) -> Self {
        let mut child = Command::new(service_bin())
            .args([name, "freeze"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn reconnect_service");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .expect("read ready");
        assert_eq!(line.trim(), "ready");
        Service(child)
    }

    fn pid(&self) -> i32 {
        self.0.id() as i32
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        // Thaw first: the driver keeps no freeze state past the process, but be tidy.
        let _ = ProcessState::as_self().freeze_process(self.pid(), false, Duration::ZERO);
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The observer needs a looper thread: freeze notifications go to the process's todo list.
fn init() {
    ProcessState::init_default().expect("ProcessState");
    ProcessState::start_thread_pool();
}

/// Records every state it is given, the thread that gave it, and whether `who`
/// named the expected binder.
///
/// The thread tells a fresh kernel registration from a cached state: the
/// kernel's initial `BR_FROZEN_BINDER` arrives on a looper, while a state the
/// registry already held is handed over on the thread calling `add`.
#[derive(Default)]
struct Recorder {
    states: Mutex<Vec<(FrozenState, ThreadId)>>,
    changed: Condvar,
    /// Weak, so the recorder never keeps the proxy alive.
    expected: Option<WIBinder>,
    wrong_who: Mutex<u32>,
}

impl Recorder {
    fn for_binder(binder: &SIBinder) -> Arc<Self> {
        Arc::new(Self {
            expected: Some(SIBinder::downgrade(binder)),
            ..Self::default()
        })
    }

    /// Waits until at least `n` states arrived, then returns them all.
    fn wait_for(&self, n: usize) -> Vec<FrozenState> {
        let states = self.states.lock().unwrap();
        let (states, _) = self
            .changed
            .wait_timeout_while(states, WAIT, |s| s.len() < n)
            .unwrap();
        states.iter().map(|(state, _)| *state).collect()
    }

    fn snapshot(&self) -> Vec<FrozenState> {
        self.states
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| *s)
            .collect()
    }

    /// Whether the first state came from a looper, i.e. from a kernel registration.
    fn first_from_kernel(&self) -> bool {
        let states = self.states.lock().unwrap();
        states.first().expect("a state").1 != thread::current().id()
    }
}

impl FrozenStateChangeCallback for Recorder {
    fn on_state_changed(&self, who: &WIBinder, state: FrozenState) {
        if self
            .expected
            .as_ref()
            .is_some_and(|expected| who != expected)
        {
            *self.wrong_who.lock().unwrap() += 1;
        }
        self.states
            .lock()
            .unwrap()
            .push((state, thread::current().id()));
        self.changed.notify_all();
    }
}

/// `freeze_process`, retried while the driver reports transactions still draining.
fn set_frozen(pid: i32, frozen: bool) {
    let end = Instant::now() + WAIT;
    loop {
        match ProcessState::as_self().freeze_process(pid, frozen, Duration::from_millis(100)) {
            Ok(()) => return,
            Err(StatusCode::WouldBlock) if Instant::now() < end => continue,
            Err(err) => panic!("freeze_process(pid {pid}, {frozen}): {err:?}"),
        }
    }
}

/// `true` if the driver supports freeze notifications; otherwise checks the refusal.
fn supported_or_refused(binder: &SIBinder) -> bool {
    let probe = Recorder::for_binder(binder);
    match binder.add_frozen_state_change_callback_arc(&probe) {
        Ok(()) => {
            binder
                .remove_frozen_state_change_callback_arc(&probe)
                .expect("remove the probe");
            true
        }
        Err(StatusCode::InvalidOperation) => {
            eprintln!("driver lacks features/freeze_notification: checked the refusal only");
            false
        }
        Err(err) => panic!("add_frozen_state_change_callback: {err:?}"),
    }
}

/// AC-4.3.9: initial state, freeze, thaw, late add, remove, re-add.
#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn callbacks_follow_freeze_and_thaw() {
    init();
    let name = service_name("follow");
    let svc = Service::start(&name);
    let binder = hub::check_service(&name).expect("registered");
    if !supported_or_refused(&binder) {
        return;
    }
    let pid = svc.pid();

    let first = Recorder::for_binder(&binder);
    binder
        .add_frozen_state_change_callback_arc(&first)
        .expect("add");
    assert_eq!(first.wait_for(1), [FrozenState::Unfrozen], "initial state");
    assert!(
        first.first_from_kernel(),
        "the kernel sent the initial state"
    );

    set_frozen(pid, true);
    assert_eq!(
        first.wait_for(2),
        [FrozenState::Unfrozen, FrozenState::Frozen]
    );

    // A synchronous call into a frozen process fails (`BR_FROZEN_REPLY`); a oneway one
    // is queued and completes (`BR_TRANSACTION_PENDING_FROZEN`, AC-4.3.11). Both are recorded.
    let smoke: Strong<dyn IRpcSmoke> = binder.clone().into_interface().expect("IRpcSmoke");
    assert!(
        smoke.r#echo("x").is_err(),
        "a synchronous call into a frozen process fails"
    );
    smoke
        .r#ping()
        .expect("a oneway call to a frozen process is queued");
    let info = ProcessState::as_self()
        .process_freeze_info(pid)
        .expect("process_freeze_info");
    assert!(
        info.sync_received && info.async_received,
        "the driver recorded both calls: {info:?}"
    );

    // A callback added now gets the cached state at once.
    let late = Recorder::for_binder(&binder);
    binder
        .add_frozen_state_change_callback_arc(&late)
        .expect("late add");
    assert_eq!(late.wait_for(1), [FrozenState::Frozen], "cached state");

    set_frozen(pid, false);
    assert_eq!(
        first.wait_for(3),
        [
            FrozenState::Unfrozen,
            FrozenState::Frozen,
            FrozenState::Unfrozen
        ]
    );
    assert_eq!(
        late.wait_for(2),
        [FrozenState::Frozen, FrozenState::Unfrozen]
    );
    assert_eq!(
        smoke.r#echo("x").expect("a thawed process answers"),
        "freeze:x"
    );

    binder
        .remove_frozen_state_change_callback_arc(&first)
        .expect("remove");
    assert_eq!(
        binder.remove_frozen_state_change_callback_arc(&first),
        Err(StatusCode::NameNotFound)
    );
    // The last removal clears the kernel registration; no more states arrive.
    binder
        .remove_frozen_state_change_callback_arc(&late)
        .expect("remove the last");
    set_frozen(pid, true);
    set_frozen(pid, false);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(first.snapshot().len(), 3, "removed callbacks stay silent");
    assert_eq!(late.snapshot().len(), 2, "removed callbacks stay silent");

    // Re-adding right after the clear (possibly before its done) registers again: the
    // state comes from a new kernel registration, not from a slot the clear left behind.
    let again = Recorder::for_binder(&binder);
    binder
        .add_frozen_state_change_callback_arc(&again)
        .expect("re-add");
    assert_eq!(again.wait_for(1), [FrozenState::Unfrozen]);
    assert!(
        again.first_from_kernel(),
        "the removal cleared the registration, so the re-add requested a new one"
    );
    binder
        .remove_frozen_state_change_callback_arc(&again)
        .expect("remove");

    for recorder in [&first, &late, &again] {
        assert_eq!(
            *recorder.wrong_who.lock().unwrap(),
            0,
            "`who` names the binder"
        );
    }
}

/// AC-4.3.7: a dropped callback is `BadValue`, before the driver is consulted.
#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn a_dropped_callback_is_refused() {
    init();
    let name = service_name("dropped");
    let _svc = Service::start(&name);
    let binder = hub::check_service(&name).expect("registered");
    let gone: Arc<dyn FrozenStateChangeCallback> = Arc::new(Recorder::default());
    let weak = Arc::downgrade(&gone);
    drop(gone);
    assert_eq!(
        binder.add_frozen_state_change_callback(weak),
        Err(StatusCode::BadValue)
    );
}

/// AC-4.3.5/4.3.9: dropping the proxy clears its registration.
///
/// Each round's proxy is dropped for good (its `WIBinder` no longer upgrades)
/// before the next round looks the service up again, on the same handle while
/// the clear may still be in flight. Every round's initial state must then come
/// from a new kernel registration (a looper), not from a slot the drop left
/// `Registered` (the adding thread). The recorders stay alive, so a freeze at the
/// end shows that no dropped proxy's callback is still registered.
#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn dropping_the_proxy_clears_its_registration() {
    init();
    let name = service_name("drop");
    let svc = Service::start(&name);
    let mut recorders = Vec::new();
    for round in 0..3 {
        let binder = hub::check_service(&name).expect("registered");
        if !supported_or_refused(&binder) {
            return;
        }
        let recorder = Recorder::for_binder(&binder);
        binder
            .add_frozen_state_change_callback_arc(&recorder)
            .expect("add");
        assert_eq!(
            recorder.wait_for(1),
            [FrozenState::Unfrozen],
            "round {round}"
        );
        assert!(
            recorder.first_from_kernel(),
            "round {round}: the previous proxy's drop cleared its registration"
        );
        let weak = SIBinder::downgrade(&binder);
        drop(binder);
        // A looper may still hold the proxy while finishing its delivery.
        let end = Instant::now() + WAIT;
        while weak.upgrade().is_ok() {
            assert!(
                Instant::now() < end,
                "round {round}: the proxy never dropped"
            );
            thread::sleep(Duration::from_millis(10));
        }
        recorders.push(recorder);
    }
    set_frozen(svc.pid(), true);
    set_frozen(svc.pid(), false);
    thread::sleep(Duration::from_millis(300));
    for (round, recorder) in recorders.iter().enumerate() {
        assert_eq!(
            recorder.snapshot(),
            [FrozenState::Unfrozen],
            "round {round}: a dropped proxy's callback stays silent"
        );
    }
}
