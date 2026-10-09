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
//! to issue; Android's SELinux allows it to system_server only, so there these
//! tests run as root with SELinux permissive.
//! `unprivileged_domain_observes_app_freeze` covers the enforcing `shell`
//! domain instead. A driver without `features/freeze_notification` is not a
//! failure: the tests then check the refusal (`InvalidOperation`) and stop,
//! unless `/dev/binderfs/features/freeze_notification` reads `1`, which makes
//! the refusal a failed support check.
//!
//! `Recorder` keeps the thread that delivered each state, which tells a fresh
//! kernel registration from a cached state: the kernel's initial
//! `BR_FROZEN_BINDER` arrives on a looper, while a state the registry already
//! held is handed over on the thread calling `add`. The adding thread can
//! also win the race to deliver a fresh registration's state (about 2% of
//! tight add/remove cycles on a 16-core host), so `add_registered` tries
//! three times: a slot left registered hands its state over on every try.
//! `dropping_the_proxy_clears_its_registration` relies on it: each round drops
//! its proxy for good (its `WIBinder` no longer upgrades) and the next looks
//! the service up again on the same handle, while the drop's clear may still
//! be in flight.
//!
//! `unprivileged_domain_observes_app_freeze` runs as `shell` with SELinux
//! enforcing (`adb unroot`), not root: every domain may read
//! `binderfs_features` and use the `BINDER_WRITE_READ` commands, while
//! `BINDER_FREEZE` / `BINDER_GET_FROZEN_INFO` are left out of the
//! `unpriv_binder_ioctls` allowlist and granted to system_server alone
//! (`system/sepolicy` `private/domain.te`, `private/system_server.te`).
//! `RSB_FREEZE_APP_SERVICE` / `RSB_FREEZE_APP_PROCESS` name the observed
//! service and its app process (default `phone` / `com.android.phone`).
//!
//! The protocol is between the kernel and this process only, so no libbinder
//! peer is needed; what matters is the driver (Rust binder on REMOTE_LINUX,
//! the C driver on an emulator). On a device, push `reconnect_service` next
//! to the test binary and point `RSB_RECONNECT_SERVICE` at it.

#![cfg(any(target_os = "linux", target_os = "android"))]
#![allow(non_snake_case)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Condvar, Mutex};
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

/// Records each state with its delivering thread: a looper means a kernel registration.
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
            let reported = std::fs::read_to_string("/dev/binderfs/features/freeze_notification")
                .is_ok_and(|v| v.trim() == "1");
            assert!(
                !reported,
                "the driver reports freeze notifications, but the add was refused"
            );
            eprintln!("driver lacks features/freeze_notification: checked the refusal only");
            false
        }
        Err(err) => panic!("add_frozen_state_change_callback: {err:?}"),
    }
}

/// Adds a recorder whose initial state came from a kernel registration; see module doc.
fn add_registered(binder: &SIBinder, initial: FrozenState, what: &str) -> Arc<Recorder> {
    for _ in 0..3 {
        let recorder = Recorder::for_binder(binder);
        binder
            .add_frozen_state_change_callback_arc(&recorder)
            .expect("add");
        assert_eq!(recorder.wait_for(1), [initial], "{what}: initial state");
        if recorder.first_from_kernel() {
            return recorder;
        }
        binder
            .remove_frozen_state_change_callback_arc(&recorder)
            .expect("remove");
        thread::sleep(Duration::from_millis(50));
    }
    panic!("{what}: three adds in a row got a cached state, not a new kernel registration");
}

struct Died(Mutex<mpsc::Sender<()>>);
impl DeathRecipient for Died {
    fn binder_died(&self, _: &WIBinder) {
        let _ = self.0.lock().unwrap().send(());
    }
}

/// Kills the service and waits for the obituary, which removes the proxy's cache entry.
fn kill_and_await_obituary(svc: &mut Service, binder: &SIBinder) {
    let (tx, rx) = mpsc::channel();
    let died = Arc::new(Died(Mutex::new(tx)));
    binder.link_to_death_arc(&died).expect("link_to_death");
    svc.0.kill().expect("kill the service");
    let _ = svc.0.wait();
    rx.recv_timeout(WAIT).expect("obituary");
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

    let first = add_registered(&binder, FrozenState::Unfrozen, "first add");

    set_frozen(pid, true);
    assert_eq!(
        first.wait_for(2),
        [FrozenState::Unfrozen, FrozenState::Frozen]
    );

    // Frozen: a sync call fails (`BR_FROZEN_REPLY`), a oneway one is queued (AC-4.3.11).
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

    // The removal cleared the registration, so a re-add requests a new one.
    let again = add_registered(&binder, FrozenState::Unfrozen, "re-add");
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

/// After the obituary a late callback still gets the cached state (AOSP `initialStateReceived`).
#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn a_callback_added_after_the_obituary_gets_the_cached_state() {
    init();
    let name = service_name("obituary");
    let mut svc = Service::start(&name);
    let binder = hub::check_service(&name).expect("registered");
    if !supported_or_refused(&binder) {
        return;
    }
    let first = add_registered(&binder, FrozenState::Unfrozen, "first add");
    set_frozen(svc.pid(), true);
    assert_eq!(
        first.wait_for(2),
        [FrozenState::Unfrozen, FrozenState::Frozen]
    );

    kill_and_await_obituary(&mut svc, &binder);
    let late = Recorder::for_binder(&binder);
    binder
        .add_frozen_state_change_callback_arc(&late)
        .expect("add after the obituary");
    assert_eq!(late.wait_for(1), [FrozenState::Frozen], "cached state");
    for recorder in [&first, &late] {
        binder
            .remove_frozen_state_change_callback_arc(recorder)
            .expect("remove");
        assert_eq!(
            *recorder.wrong_who.lock().unwrap(),
            0,
            "`who` names the binder"
        );
    }
}

/// AC-4.3.5/4.3.9: each round's initial state comes from a new kernel registration.
#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn dropping_the_proxy_clears_its_registration() {
    init();
    let name = service_name("drop");
    let _svc = Service::start(&name);
    for round in 0..3 {
        let binder = hub::check_service(&name).expect("registered");
        if !supported_or_refused(&binder) {
            return;
        }
        // The previous round's drop cleared its registration.
        let _recorder = add_registered(&binder, FrozenState::Unfrozen, &format!("round {round}"));
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
    }
}

/// `cmd activity freeze|unfreeze PROCESS`: system_server issues `BINDER_FREEZE`.
#[cfg(target_os = "android")]
fn am_freeze(process: &str, frozen: bool) {
    let mut cmd = Command::new("cmd");
    cmd.arg("activity");
    if frozen {
        cmd.args(["freeze", "--sticky"]);
    } else {
        cmd.arg("unfreeze");
    }
    let status = cmd.arg(process).status().expect("run cmd activity");
    assert!(status.success(), "cmd activity freeze={frozen} {process}");
}

/// Thaws the app if the test stops while it is frozen.
#[cfg(target_os = "android")]
struct Thaw<'a>(&'a str);
#[cfg(target_os = "android")]
impl Drop for Thaw<'_> {
    fn drop(&mut self) {
        // Asserting while unwinding would abort the test binary.
        if thread::panicking() {
            let _ = Command::new("cmd")
                .args(["activity", "unfreeze", self.0])
                .status();
        } else {
            am_freeze(self.0, false);
        }
    }
}

/// Run as `shell` with SELinux enforcing; see module doc.
#[cfg(target_os = "android")]
#[test]
#[ignore = "needs an Android device; run as shell with SELinux enforcing"]
fn unprivileged_domain_observes_app_freeze() {
    init();
    let service = std::env::var("RSB_FREEZE_APP_SERVICE").unwrap_or_else(|_| "phone".to_owned());
    let process =
        std::env::var("RSB_FREEZE_APP_PROCESS").unwrap_or_else(|_| "com.android.phone".to_owned());

    let domain = std::fs::read_to_string("/proc/self/attr/current").unwrap_or_default();
    let enforcing =
        std::fs::read_to_string("/sys/fs/selinux/enforce").is_ok_and(|v| v.trim() == "1");
    eprintln!(
        "domain {}, enforcing {enforcing}",
        domain.trim_end_matches(['\0', '\n'])
    );
    // Only `shell` is checked: userdebug's `su` is a permissive domain (`su.te`).
    if enforcing && domain.contains(":shell:") {
        // Our own pid, thawing: harmless where the ioctl is allowed.
        let me = std::process::id() as i32;
        let eacces = StatusCode::Errno(-13);
        assert_eq!(
            ProcessState::as_self().freeze_process(me, false, Duration::ZERO),
            Err(eacces),
            "SELinux refuses BINDER_FREEZE"
        );
        assert_eq!(
            ProcessState::as_self().process_freeze_info(me).err(),
            Some(eacces),
            "SELinux refuses BINDER_GET_FROZEN_INFO"
        );
    } else {
        eprintln!("not the enforcing shell domain: ioctl refusal not checked");
    }

    let binder = hub::check_service(&service).expect("the observed service");
    if !supported_or_refused(&binder) {
        return;
    }
    let recorder = add_registered(&binder, FrozenState::Unfrozen, "app service");

    let thaw = Thaw(&process);
    am_freeze(&process, true);
    assert_eq!(
        recorder.wait_for(2),
        [FrozenState::Unfrozen, FrozenState::Frozen]
    );
    drop(thaw);
    assert_eq!(
        recorder.wait_for(3),
        [
            FrozenState::Unfrozen,
            FrozenState::Frozen,
            FrozenState::Unfrozen
        ]
    );
    binder
        .remove_frozen_state_change_callback_arc(&recorder)
        .expect("remove");
    assert_eq!(
        *recorder.wrong_who.lock().unwrap(),
        0,
        "`who` names the binder"
    );
}
