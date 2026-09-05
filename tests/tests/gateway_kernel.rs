// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-22 Phase E — the gateway across the two *real* stacks:
//! `A (rpc) → B (gateway) → C (kernel binder)`.
//!
//! The hermetic Phase B tests (`gateway_rpc.rs`) put RPC on both hops,
//! which proves the delegating `impl` but not the thing the gateway
//! exists for: reaching a **kernel** service from a socket client. Here
//! C is registered with the service manager, B holds a kernel proxy to
//! it and re-publishes that proxy on a Unix socket, and A speaks only
//! RPC.
//!
//! C runs in a **child process**, and it has to: a process that looks up
//! its own service gets its local node back from the driver
//! (`BINDER_TYPE_BINDER`, not a handle), so an in-process C would leave B
//! holding a local binder and prove nothing. The child is this same test
//! binary re-executed with `RSB_GW_KERNEL_SERVICE` set — the pattern
//! `rsbinder/tests/rpc_server.rs` already uses.
//!
//! Requires a real binder device (`/dev/binder`, Linux+binderfs or
//! Android) and a service manager permissive enough to accept
//! `addService` for an arbitrary name (`rsb_hub --insecure-allow-all`).
//! Hence `#[ignore]`:
//!
//! ```text
//! cargo test -p tests --features rpc --test gateway_kernel -- --ignored --nocapture
//! ```

#![cfg(all(feature = "rpc", any(target_os = "linux", target_os = "android")))]
#![allow(non_snake_case)]

use rsbinder::{Interface, Strong};

include!(concat!(env!("OUT_DIR"), "/rpc_smoke.rs"));

use rpcsmoke::IRpcSmoke::{BnRpcSmoke, IRpcSmoke};

/// The child blocks in the kernel thread pool forever; reap it however
/// the test ends, including on a panic.
struct KillOnDrop<'a>(&'a mut std::process::Child);
impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct KernelSvc;
impl Interface for KernelSvc {}
impl IRpcSmoke for KernelSvc {
    fn r#echo(&self, s: &str) -> rsbinder::status::Result<String> {
        Ok(format!("kernel:{s}"))
    }
    fn r#add(&self, a: i32, b: i32) -> rsbinder::status::Result<i32> {
        Ok(a + b)
    }
    fn r#ping(&self) -> rsbinder::status::Result<()> {
        Ok(())
    }
}

#[test]
#[ignore = "requires kernel binder (/dev/binder) + a permissive service manager; run on REMOTE_LINUX/emulator"]
fn rpc_client_reaches_a_kernel_service_through_a_gateway() {
    // Child role (C): publish on kernel binder and block.
    if let Ok(name) = std::env::var("RSB_GW_KERNEL_SERVICE") {
        rsbinder::serve("binder://")
            .expect("serve kernel")
            .add(&name, BnRpcSmoke::new_binder(KernelSvc))
            .expect("addService (needs a permissive service manager)")
            .run()
            .expect("kernel thread pool");
        std::process::exit(0);
    }

    let name = format!("rsbinder.test.gw.{}", std::process::id());
    let mut sock = std::env::temp_dir();
    sock.push(format!("rsb_gw_kernel.{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let uri = format!("unix://{}", sock.display());

    let exe = std::env::current_exe().expect("current_exe");
    let mut c = std::process::Command::new(exe)
        .args([
            "--ignored",
            "--exact",
            "rpc_client_reaches_a_kernel_service_through_a_gateway",
            "--nocapture",
        ])
        .env("RSB_GW_KERNEL_SERVICE", &name)
        .spawn()
        .expect("spawn kernel service child");
    let _kill = KillOnDrop(&mut c);

    // B — the gateway: a kernel proxy of C, re-published on a socket.
    // `connect("binder://…")` waits for the registration (AOSP
    // `waitForService`), so no sleep is needed here.
    let upstream: Strong<dyn IRpcSmoke> =
        rsbinder::connect(&format!("binder://{name}")).expect("kernel lookup");
    assert!(
        (*upstream.as_binder()).is_remote(),
        "the gateway must be fronting a real kernel proxy, not a local node"
    );

    // Handing that kernel proxy straight to an RPC server is the mistake
    // the gateway exists to replace — refused here, against a real
    // `/dev/binder` proxy rather than a hand-built stand-in.
    let refused = rsbinder::serve(&uri)
        .expect("serve rpc")
        .add("smoke", upstream.as_binder())
        .err();
    assert_eq!(
        refused,
        Some(rsbinder::StatusCode::InvalidOperation),
        "AC-22.4: a kernel proxy cannot be published on an RPC server"
    );

    let _b = rsbinder::serve(&uri)
        .expect("serve rpc")
        .add("smoke", BnRpcSmoke::new_binder(upstream))
        .expect("AC-22.12: a kernel proxy wrapped in a Bn* is publishable over RPC")
        .spawn()
        .expect("spawn rpc");

    // A — pure RPC; it never learns the kernel exists.
    let a: Strong<dyn IRpcSmoke> = rsbinder::connect(&format!("{uri}#smoke")).expect("A→B");
    assert_eq!(
        a.r#echo("hi").expect("echo through the gateway"),
        "kernel:hi",
        "AC-22.12: the answer comes from the kernel service"
    );
    assert_eq!(a.r#add(40, 2).expect("add through the gateway"), 42);

    // A oneway call has no reply to wait on; pair it with a twoway call
    // so the assertion below cannot pass on an unflushed queue.
    a.r#ping().expect("oneway through the gateway");
    assert_eq!(a.r#echo("barrier").unwrap(), "kernel:barrier");

    let _ = std::fs::remove_file(&sock);
}
