// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A death link made while the previous proxy's clear is still queued must
//! survive that clear (plan 4-3 §4.2, §10-4), so `#[ignore]`:
//!
//! ```text
//! target/debug/rsb_hub --config tests/policy/permissive.toml &
//! cargo test -p tests --features rpc --test death_reorder_kernel -- --ignored --test-threads=1
//! ```
//!
//! The death cookie is the handle, and both drivers ignore a second
//! `BC_REQUEST_DEATH_NOTIFICATION` on a ref that has one but honor a
//! `BC_CLEAR_DEATH_NOTIFICATION` whose cookie matches. So if a dropped proxy's
//! clear reaches the kernel after a new proxy of the same handle linked, it
//! removes the new link, and the new recipient never hears of the death.
//!
//! The order is forced: the old proxy is dropped inside another service's
//! `binder_died`, which runs on a looper thread, where the drop's clear stays
//! queued until the callback returns. The main thread re-resolves and links
//! before letting it return.

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rsbinder::*;

const WAIT: Duration = Duration::from_secs(10);

fn service_name(tag: &str) -> String {
    format!("rsb.test.deathorder.{tag}.{}", std::process::id())
}

fn service_bin() -> std::ffi::OsString {
    std::env::var_os("RSB_RECONNECT_SERVICE")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_reconnect_service").into())
}

struct Service(Child);
impl Service {
    fn start(name: &str) -> Self {
        let mut child = Command::new(service_bin())
            .args([name, "death"])
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

    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Signals once on death.
struct Notify(Mutex<Sender<()>>);
impl DeathRecipient for Notify {
    fn binder_died(&self, _: &WIBinder) {
        let _ = self.0.lock().unwrap().send(());
    }
}

/// On a looper, drops the proxy it holds, then waits for the main thread's go.
struct DropOnLooper {
    victim: Mutex<Option<SIBinder>>,
    dropped: Mutex<Sender<()>>,
    go: Mutex<Receiver<()>>,
}
impl DeathRecipient for DropOnLooper {
    fn binder_died(&self, _: &WIBinder) {
        drop(self.victim.lock().unwrap().take());
        let _ = self.dropped.lock().unwrap().send(());
        let _ = self.go.lock().unwrap().recv_timeout(WAIT);
    }
}

fn handle_of(binder: &SIBinder) -> u32 {
    binder.as_proxy().expect("a kernel proxy").handle()
}

/// The forced order (`link_while_queued`) and its control: whether the new
/// proxy's recipient heard of the subject's death.
fn new_link_hears_the_death(tag: &str, link_while_queued: bool) -> bool {
    ProcessState::init_default().expect("ProcessState");
    ProcessState::start_thread_pool();

    let (b_name, c_name) = (
        service_name(&format!("{tag}.subject")),
        service_name(&format!("{tag}.hook")),
    );
    let mut subject = Service::start(&b_name);
    let mut hook_service = Service::start(&c_name);

    // The subject's first proxy, linked, so its drop queues a clear.
    let old = hub::check_service(&b_name).expect("subject registered");
    let old_handle = handle_of(&old);
    let (old_tx, _old_rx) = channel();
    let old_recipient = Arc::new(Notify(Mutex::new(old_tx)));
    old.link_to_death_arc(&old_recipient).expect("link old");

    let (dropped_tx, dropped_rx) = channel();
    let (go_tx, go_rx) = channel();
    let hook = Arc::new(DropOnLooper {
        victim: Mutex::new(Some(old)),
        dropped: Mutex::new(dropped_tx),
        go: Mutex::new(go_rx),
    });
    let hook_binder = hub::check_service(&c_name).expect("hook registered");
    hook_binder.link_to_death_arc(&hook).expect("link hook");

    // The hook's death runs `DropOnLooper` on a looper: the old proxy drops there.
    hook_service.kill();
    dropped_rx
        .recv_timeout(WAIT)
        .expect("the old proxy dropped on a looper");

    if !link_while_queued {
        // Control: the looper returns first, so its clear is in the kernel before the link.
        go_tx.send(()).expect("go");
        std::thread::sleep(Duration::from_millis(300));
    }

    // In the forced order the old proxy's clear is still queued on that looper.
    let new = hub::check_service(&b_name).expect("subject re-resolved");
    if link_while_queued {
        assert_eq!(
            handle_of(&new),
            old_handle,
            "the kernel ref outlived the old proxy, so the handle is the same"
        );
    }
    let (new_tx, new_rx) = channel();
    let new_recipient = Arc::new(Notify(Mutex::new(new_tx)));
    new.link_to_death_arc(&new_recipient).expect("link new");

    if link_while_queued {
        // Let the looper return: its queued clear goes out now.
        go_tx.send(()).expect("go");
        std::thread::sleep(Duration::from_millis(300));
    }

    subject.kill();
    let heard = new_rx.recv_timeout(WAIT).is_ok();
    drop(new);
    heard
}

#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn a_link_made_while_the_old_clear_is_queued_survives_it() {
    assert!(
        new_link_hears_the_death("queued", true),
        "the new proxy's death link was removed by the old proxy's late clear"
    );
}

/// The same steps with the old clear already in the kernel: the link must hold.
#[test]
#[ignore = "needs /dev/binderfs/binder and a running rsb_hub"]
fn control_a_link_made_after_the_old_clear_survives() {
    assert!(new_link_hears_the_death("control", false));
}
