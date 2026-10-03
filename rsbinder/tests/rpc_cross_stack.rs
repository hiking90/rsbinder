// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The boundary between the two IPC stacks — kernel binder
//! (`/dev/binder`) and socket RPC — is refused at **write time**, not
//! discovered later by the receiver.
//!
//! libbinder refuses the same three writes with `INVALID_OPERATION`
//! (`Parcel.cpp` `flattenBinder`, `RpcState.cpp` `onBinderLeaving`), and
//! rsbinder refuses all three at the same points — so no such binder ever
//! reaches a receiver that could only answer it with `UnknownTransaction`.
//! See `plans/2-22-*`.
//!
//! Hermetic: `mem`/`unix` transports only, and deliberately **never**
//! initializes `ProcessState` — the kernel-parcel checks must fire
//! before `FlatBinderObject::from` calls `ProcessState::as_self()`,
//! so a pure-RPC process (macOS, or any Linux process that never opened
//! `/dev/binder`) gets the rejection rather than a panic.
//!
//! # Mutation gates
//!
//! - `kernel_parcel_refuses_rpc_proxy`: `From<&SIBinder> for FlatBinderObject` calls
//!   `ProcessState::as_self()`, which panics in a process that never initialized it. Moving
//!   the check after that conversion turns this test red on macOS.
//! - `rpc_parcel_refuses_kernel_proxy`: without the check the kernel proxy is registered as a
//!   *local* node (the `local_node_count` assertion) and the peer's first call to it dies with
//!   `UnknownTransaction`.
//! - `rpc_parcel_refuses_another_sessions_proxy`: removing the `ptr::eq` session guard makes
//!   the foreign-proxy write `Ok`.

#![cfg(feature = "rpc")]

use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::thread;

use rsbinder::rpc::transport::MemTransport;
use rsbinder::rpc::{AddressSpace, RpcServer, RpcSession};
use rsbinder::{
    Binder, DeathRecipient, IBinder, Interface, Parcel, Remotable, Result, SIBinder, StatusCode,
    Transactable, TransactionCode, WIBinder,
};

// ---- fixtures -------------------------------------------------------

const DESC: &str = "rsbinder.test.ICrossStack";

struct BnSvc;
impl Remotable for BnSvc {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, _: TransactionCode, _: &mut Parcel, _: &mut Parcel) -> Result<()> {
        Err(StatusCode::UnknownTransaction)
    }
    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
        Ok(())
    }
}

fn local_root() -> SIBinder {
    Interface::as_binder(&Binder::new(BnSvc))
}

/// Served RPC session pair: the server runs `serve_blocking` on its own thread; drop tears down.
struct Pair {
    client: RpcSession,
    server: Option<RpcSession>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Pair {
    fn new() -> Self {
        let (a, b) = MemTransport::pair();
        let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("server session");
        server.set_root(local_root()).expect("set_root");
        let serving = server.clone();
        let thread = thread::spawn(move || {
            let _ = serving.serve_blocking();
        });
        let client = RpcSession::new(Box::new(b), AddressSpace::Initiator).expect("client session");
        Self {
            client,
            server: Some(server),
            thread: Some(thread),
        }
    }

    /// The peer's root, as an `RpcProxy`-backed `SIBinder`.
    fn remote_root(&self) -> SIBinder {
        self.client.get_root().expect("get_root")
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        // Not a bare drop: `self.client` keeps the transport open; `serve_blocking` never sees EOF.
        if let Some(s) = self.server.take() {
            s.close_session();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Remote but not RPC-backed: a kernel `ProxyHandle` stand-in that needs no `/dev/binder`.
struct KernelishProxy;

impl IBinder for KernelishProxy {
    fn link_to_death(&self, _: std::sync::Weak<dyn DeathRecipient>) -> Result<()> {
        Err(StatusCode::InvalidOperation)
    }
    fn unlink_to_death(&self, _: std::sync::Weak<dyn DeathRecipient>) -> Result<()> {
        Err(StatusCode::InvalidOperation)
    }
    fn ping_binder(&self) -> Result<()> {
        Ok(())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_transactable(&self) -> Option<&dyn Transactable> {
        None
    }
    fn descriptor(&self) -> &str {
        "rsbinder.test.IKernelish"
    }
    fn is_remote(&self) -> bool {
        true
    }
    fn inc_strong(&self, _: &SIBinder) -> Result<()> {
        Ok(())
    }
    fn attempt_inc_strong(&self) -> bool {
        true
    }
    fn dec_strong(&self, _: Option<ManuallyDrop<SIBinder>>) -> Result<()> {
        Ok(())
    }
    fn inc_weak(&self, _: &WIBinder) -> Result<()> {
        Ok(())
    }
    fn dec_weak(&self) -> Result<()> {
        Ok(())
    }
}

fn kernelish() -> SIBinder {
    SIBinder::new(Arc::new(KernelishProxy)).expect("SIBinder::new")
}

// ---- AC-22.1: kernel parcel ← RPC proxy -----------------------------

/// AC-22.1: a kernel parcel refuses an RPC proxy in every funnel shape, before `ProcessState`.
#[test]
fn kernel_parcel_refuses_rpc_proxy() {
    let pair = Pair::new();
    let proxy = pair.remote_root();

    // Bare `SIBinder`.
    let mut p = Parcel::new();
    assert_eq!(
        p.write(&proxy).unwrap_err(),
        StatusCode::InvalidOperation,
        "AC-22.1: an RPC proxy cannot be written into a kernel parcel"
    );

    // `Option<SIBinder>` — same funnel via `SerializeOption`.
    let mut p = Parcel::new();
    assert_eq!(
        p.write(&Some(proxy.clone())).unwrap_err(),
        StatusCode::InvalidOperation,
        "AC-22.1: Option<SIBinder> takes the same funnel"
    );

    // `Vec<SIBinder>` — the array path, one element deep.
    let mut p = Parcel::new();
    assert_eq!(
        p.write(&vec![proxy.clone()]).unwrap_err(),
        StatusCode::InvalidOperation,
        "AC-22.1: Vec<SIBinder> takes the same funnel"
    );

    // A null `Option` still writes: the rejection is about the object, not the parcel mode.
    let mut p = Parcel::new();
    p.write(&None::<SIBinder>)
        .expect("a null binder is still writable into a kernel parcel");
}

/// AC-22.5: the refusal is type-based, not mode-based; a local `Binder<T>` writes into RPC.
#[test]
fn local_binder_still_writes_into_an_rpc_parcel() {
    let pair = Pair::new();
    let remote = pair.remote_root();
    let rp = (*remote).as_remote().expect("as_remote");
    let mut data = rp.prepare_transact(false).expect("prepare_transact");

    data.write(&local_root())
        .expect("AC-22.5: a local binder must still cross into an RPC parcel");
}

// ---- AC-22.2: RPC parcel ← kernel proxy -----------------------------

/// AC-22.2: an RPC parcel refuses a remote binder that is not RPC-backed (a kernel proxy).
#[test]
fn rpc_parcel_refuses_kernel_proxy() {
    let pair = Pair::new();
    let remote = pair.remote_root();
    let rp = (*remote).as_remote().expect("as_remote");
    let mut data = rp.prepare_transact(false).expect("prepare_transact");

    assert_eq!(
        data.write(&kernelish()).unwrap_err(),
        StatusCode::InvalidOperation,
        "AC-22.2: a kernel proxy cannot be written into an RPC parcel"
    );

    // Nor registered as a local node in the *client* session, which owns the parcel.
    assert_eq!(
        pair.client.local_node_count(),
        0,
        "AC-22.2: the refused binder must not have been registered as a local node"
    );
}

// ---- AC-22.3: RPC parcel ← another session's proxy ------------------

/// AC-22.3: another session's `RpcProxy` (address foreign here) is refused, as `onBinderLeaving`.
#[test]
fn rpc_parcel_refuses_another_sessions_proxy() {
    let one = Pair::new();
    let two = Pair::new();

    let foreign = one.remote_root();
    let remote_two = two.remote_root();
    let rp = (*remote_two).as_remote().expect("as_remote");
    let mut data = rp.prepare_transact(false).expect("prepare_transact");

    assert_eq!(
        data.write(&foreign).unwrap_err(),
        StatusCode::InvalidOperation,
        "AC-22.3: a proxy from an unrelated RPC session is refused"
    );

    // Same session's own proxy is fine: the guard is about identity, not proxies.
    let mut data = rp.prepare_transact(false).expect("prepare_transact");
    data.write(&remote_two)
        .expect("AC-22.3: this session's own proxy may travel back home");
}

// ---- AC-22.4: registration-time refusal -----------------------------

/// AC-22.4: server/session `set_root` and `add_service` refuse a remote binder up front.
#[test]
fn registration_refuses_a_remote_binder() {
    let pair = Pair::new();
    let proxy = pair.remote_root();

    // Bound but never run: registration guards need no accept loop.
    let path = SockPath::new("reg");
    let server = RpcServer::setup_unix_server(&path.0).expect("bind");
    assert_eq!(
        server.set_root(proxy.clone()).unwrap_err(),
        StatusCode::InvalidOperation,
        "AC-22.4: RpcServer::set_root refuses a remote binder"
    );
    assert_eq!(
        server.add_service("gw", proxy.clone()).unwrap_err(),
        StatusCode::InvalidOperation,
        "AC-22.4: RpcServer::add_service refuses a remote binder"
    );
    // A local binder still registers.
    server.set_root(local_root()).expect("local root accepted");
    server
        .add_service("gw", local_root())
        .expect("local service accepted");

    // A kernel proxy is refused the same way: the rule is `is_remote`, not "is an RpcProxy".
    assert_eq!(
        server.set_root(kernelish()).unwrap_err(),
        StatusCode::InvalidOperation,
        "AC-22.4: a kernel proxy is refused at registration too"
    );

    let (c, _d) = MemTransport::pair();
    let session = RpcSession::new(Box::new(c), AddressSpace::Acceptor).expect("session");
    assert_eq!(
        session.set_root(proxy).unwrap_err(),
        StatusCode::InvalidOperation,
        "AC-22.4: RpcSession::set_root refuses a remote binder"
    );
    session.set_root(local_root()).expect("local root accepted");
}

/// Socket path that unlinks itself on drop, even after a failed assertion.
struct SockPath(std::path::PathBuf);

impl SockPath {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "rsb_xstack_{}_{}_{}.sock",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        SockPath(p)
    }
}

impl Drop for SockPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// AC-22.5: the fixture root is local, so the refusals above are not vacuous.
#[test]
fn fixture_root_is_local() {
    assert!(
        !(*local_root()).is_remote(),
        "the fixture root must be local, or every rejection above is vacuous"
    );
}
