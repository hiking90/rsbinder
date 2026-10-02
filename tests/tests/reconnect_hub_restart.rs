// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-10b AC-10b.4 — `Reconnecting` across a restart of the service
//! manager itself. This test starts and kills its own `rsb_hub`, so it runs
//! with no other one up: last in the kernel tier, after the shared hub is
//! stopped. `#[ignore]`; run it explicitly:
//!
//! ```text
//! cargo build --workspace --bins
//! cargo test -p tests --features rpc --test reconnect_hub_restart -- --ignored
//! ```
//!
//! `RSB_HUB_BIN` overrides the `rsb_hub` path (default: this workspace's
//! `target/debug/rsb_hub`).

#![cfg(any(target_os = "linux", target_os = "android"))]
#![allow(non_snake_case)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rsbinder::reconnect::Reconnecting;
use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/rpc_smoke.rs"));

use rpcsmoke::IRpcSmoke::IRpcSmoke;

struct Killed(Child);
impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn hub() -> Killed {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bin = std::env::var_os("RSB_HUB_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../target/debug/rsb_hub"));
    let child = Command::new(&bin)
        .arg("--config")
        .arg(manifest.join("policy/permissive.toml"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
    let hub = Killed(child);
    // Up once it answers a lookup as the context manager.
    assert!(
        eventually(|| hub::default()
            .and_then(|sm| sm.try_get_service("none"))
            .is_ok()),
        "rsb_hub did not come up"
    );
    hub
}

fn service(name: &str, tag: &str) -> Killed {
    let mut child = Command::new(env!("CARGO_BIN_EXE_reconnect_service"))
        .args([name, tag])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn reconnect_service");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .expect("read ready");
    assert_eq!(line.trim(), "ready");
    Killed(child)
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

#[test]
#[ignore = "starts its own rsb_hub: run last, with no other rsb_hub up"]
fn reconnects_across_a_service_manager_restart() {
    let running = Command::new("pgrep").args(["-x", "rsb_hub"]).output();
    assert!(
        !running.is_ok_and(|o| o.status.success()),
        "another rsb_hub is running; stop it first (this test owns the context manager)"
    );
    ProcessState::init_default().expect("ProcessState");
    ProcessState::start_thread_pool();

    let name = format!("rsb.test.reconnect.hub.{}", std::process::id());
    let mut sm = hub();
    let mut svc = service(&name, "a");
    let h = Reconnecting::<dyn IRpcSmoke>::builder(&format!("binder://{name}"))
        .build()
        .expect("build");
    assert_eq!(h.with(|p| p.r#echo("x")).unwrap(), "a:x");

    // (a) The service manager restarts under a live connection: nothing to do.
    drop(sm);
    sm = hub();
    assert_eq!(h.with(|p| p.r#echo("x")).unwrap(), "a:x");
    assert_eq!(h.generation(), 1);

    // (b) The service restarts and registers with the new service manager.
    drop(svc);
    svc = service(&name, "b");
    assert!(
        eventually(|| h.generation() == 2),
        "reconnected via the new hub"
    );
    assert_eq!(h.with(|p| p.r#echo("x")).unwrap(), "b:x");

    // (c) The service dies, then the service manager restarts while the helper waits.
    drop(svc);
    std::thread::sleep(Duration::from_millis(100));
    drop(sm);
    std::thread::sleep(Duration::from_millis(100));
    let _sm = hub();
    let _svc = service(&name, "c");
    assert!(
        eventually(|| h.generation() == 3),
        "reconnected after both restarted"
    );
    assert_eq!(h.with(|p| p.r#echo("x")).unwrap(), "c:x");
}
