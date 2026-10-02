// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-10b — `Reconnecting` over kernel binder, against a live service
//! manager, so `#[ignore]` (run with `--ignored` on a binder-capable host):
//!
//! ```text
//! target/debug/rsb_hub --config tests/policy/permissive.toml &
//! cargo test -p tests --features rpc --test reconnect_kernel -- --ignored --test-threads=1
//! ```
//!
//! The service each test kills and restarts is `reconnect_service`, under a
//! name of its own, so the shared `test_service` is never touched. On a
//! device, push it next to the test binary and point `RSB_RECONNECT_SERVICE`
//! at it.

#![cfg(any(target_os = "linux", target_os = "android"))]
#![allow(non_snake_case)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rsbinder::reconnect::Reconnecting;
use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/rpc_smoke.rs"));

use rpcsmoke::IRpcSmoke::{BnRpcSmoke, IRpcSmoke};

/// Unique per process, so parallel test binaries never collide on a name.
fn service_name(tag: &str) -> String {
    format!("rsb.test.reconnect.{tag}.{}", std::process::id())
}

/// The service binary; `RSB_RECONNECT_SERVICE` overrides the build path (on a device).
fn service_bin() -> std::ffi::OsString {
    std::env::var_os("RSB_RECONNECT_SERVICE")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_reconnect_service").into())
}

/// `reconnect_service NAME TAG`, returned once it printed `ready` (registered).
struct Service(Child);
impl Service {
    fn start(name: &str, tag: &str) -> Self {
        let mut child = Command::new(service_bin())
            .args([name, tag])
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
}
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn eventually(mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    f()
}

fn kernel(name: &str) -> String {
    format!("binder://{name}")
}

/// AC-10b.3: a killed and restarted service is picked up again by its death
/// notification alone, `on_connect` runs once per connection.
#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn a_restarted_service_is_reconnected_without_a_call() {
    let name = service_name("restart");
    let svc = Service::start(&name, "a");
    let connects = Arc::new(AtomicU32::new(0));
    let counted = connects.clone();
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&kernel(&name))
        .on_connect(move |c| {
            counted.fetch_add(1, Ordering::SeqCst);
            c.proxy().r#ping()?;
            Ok(())
        })
        .build()
        .expect("build");
    assert_eq!(h.with(|p| p.r#echo("x")).unwrap(), "a:x");

    drop(svc);
    let _svc = Service::start(&name, "b");
    assert!(
        eventually(|| h.generation() == 2),
        "reconnected without a call"
    );
    assert_eq!(h.with(|p| p.r#echo("x")).unwrap(), "b:x");
    assert_eq!(connects.load(Ordering::SeqCst), 2);
}

/// AC-10b.7b: built before the service registers, then connected by the
/// registration notification.
#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn a_helper_built_before_its_service_connects_on_registration() {
    let name = service_name("late");
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&kernel(&name))
        .build()
        .expect("an unregistered name is not a build error");
    assert_eq!(h.generation(), 0);
    let _svc = Service::start(&name, "a");
    let p = h
        .wait_connected(Some(Duration::from_secs(10)))
        .expect("connected");
    assert_eq!(p.r#echo("x").unwrap(), "a:x");
}

/// AC-10b.5: a name the service manager refuses to watch is a build error.
#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn an_invalid_name_is_a_build_error() {
    let r = Reconnecting::<dyn IRpcSmoke>::builder("binder://not a valid name")
        .build()
        .err();
    assert_eq!(r, Some(StatusCode::BadValue));
}

/// AC-10b.7c: a service in this process comes back as a local binder, which
/// cannot die on its own, so nothing is watched.
#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn a_local_service_is_used_without_watching() {
    struct Local;
    impl Interface for Local {}
    impl IRpcSmoke for Local {
        fn r#echo(&self, s: &str) -> rsbinder::BinderResult<String> {
            Ok(format!("local:{s}"))
        }
        fn r#add(&self, a: i32, b: i32) -> rsbinder::BinderResult<i32> {
            Ok(a + b)
        }
        fn r#ping(&self) -> rsbinder::BinderResult<()> {
            Ok(())
        }
        fn r#fail(&self, code: i32) -> rsbinder::BinderResult<()> {
            Err(Status::new_service_specific_error(code, None))
        }
    }
    let name = service_name("local");
    // Initializes ProcessState as the helper would; the service must exist first.
    drop(Client::open("binder://").expect("open"));
    hub::add_service(&name, BnRpcSmoke::new_binder(Local).as_binder()).expect("add_service");
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&kernel(&name))
        .build()
        .expect("build");
    assert_eq!(h.with(|p| p.r#echo("x")).unwrap(), "local:x");
    assert_eq!(h.generation(), 1);
}

/// AC-10b.9 (kernel): dropping a helper that waits for a registration leaves
/// no callback with the service manager. rsb_hub caps callbacks per name at
/// 256, so filling the cap after the drop shows that none was left behind.
#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn dropping_a_waiting_helper_unregisters_its_callback() {
    struct Cb;
    impl Interface for Cb {}
    impl hub::IServiceCallback for Cb {
        fn onRegistration(&self, _: &str, _: &SIBinder) -> rsbinder::BinderResult<()> {
            Ok(())
        }
    }
    let name = service_name("dropwait");
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&kernel(&name))
        .build()
        .expect("build");
    // The first background attempt finds nothing and waits for the name.
    std::thread::sleep(Duration::from_millis(200));
    let t0 = Instant::now();
    drop(h);
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "{:?}",
        t0.elapsed()
    );

    let filled = eventually(|| {
        let cbs: Vec<_> = (0..256)
            .map(|_| hub::BnServiceCallback::new_binder(Cb))
            .collect();
        let all = cbs
            .iter()
            .all(|cb| hub::register_for_notifications(&name, cb).is_ok());
        for cb in &cbs {
            let _ = hub::unregister_for_notifications(&name, cb);
        }
        all
    });
    assert!(
        filled,
        "a callback of the dropped helper is still registered"
    );
}
