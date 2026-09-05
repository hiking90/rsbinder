// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! vsock backend e2e — **Linux-
//! only, `#[ignore]` by default** (environment gate: needs a
//! peer VM or the `vsock_loopback` kernel module + `VMADDR_CID_LOCAL`).
//!
//! Run manually on a suitable Linux host with:
//! ```text
//! sudo modprobe vsock_loopback   # if not built-in
//! cargo test -p rsbinder --features rpc-vsock --test rpc_vsock -- --ignored
//! ```
//!
//! Demonstrates that the core runs unmodified with the
//! transport swapped to vsock, host↔guest value round-trip, and
//! `PeerIdentity::Vsock{cid}` never mis-reported as `Local`.
//!
//! The server is built with the same `RpcServer::setup_vsock_server`
//! factory + `run_background` pattern as the UDS e2e suite. The
//! underlying `VsockTransport::from_stream` / `VsockTransport::connect`
//! are exercised (one through the server's accept loop, the other
//! through the test's client construction).

#![cfg(all(feature = "rpc-vsock", target_os = "linux"))]

use rsbinder::rpc::transport::VsockTransport;
use rsbinder::rpc::{PeerIdentity, RpcServer, RpcSession, RpcTransport};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, Result, SIBinder, Status, StatusCode, TransactionCode,
    FIRST_CALL_TRANSACTION,
};

const DESC: &str = "rsbinder.test.IVsockPing";
const TX_PING: TransactionCode = FIRST_CALL_TRANSACTION;
const TEST_PORT: u32 = 0x52_42; // arbitrary

trait IPing: Interface {
    fn ping(&self, s: &str) -> Result<String>;
}
struct PingSvc;
impl Interface for PingSvc {}
impl IPing for PingSvc {
    fn ping(&self, s: &str) -> Result<String> {
        Ok(format!("pong:{s}"))
    }
}
struct BnPing(Box<dyn IPing + Send + Sync>);
impl Remotable for BnPing {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            TX_PING => {
                let a: String = reader.read()?;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&self.0.ping(&a)?)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn ping_via(root: &SIBinder, msg: &str) -> Result<String> {
    let rp = (**root)
        .as_any()
        .downcast_ref::<rsbinder::rpc::RpcProxy>()
        .expect("RpcProxy");
    let mut d = rp.build_request(DESC)?;
    d.write(&msg)?;
    let mut r = rp
        .transact(TX_PING, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    let st: Status = r.read()?;
    if !st.is_ok() {
        return Err(StatusCode::from(st));
    }
    r.read::<String>()
}

/// Core round-trip over loopback vsock (`VMADDR_CID_LOCAL`) — server
/// built with `RpcServer::setup_vsock_server`.
///
/// Server-side: the same factory + `run_background` shape used by the
/// UDS e2e suite — backend swap is the only difference. Client-side:
/// `VsockTransport::connect` so the `PeerIdentity::Vsock`
/// assertion keeps its original wire-level reach.
#[test]
#[ignore = "needs Linux vsock loopback (modprobe vsock_loopback) or a peer VM"]
fn vsock_loopback_e2e() {
    use vsock::VMADDR_CID_LOCAL;

    // Same `RpcServer` API as UDS, vsock-backed listener.
    let server =
        RpcServer::setup_vsock_server(VMADDR_CID_LOCAL, TEST_PORT).expect("setup_vsock_server");
    server
        .set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))))
        .expect("set_root");
    // Accessor gates: vsock_address `Some`, fs path `None`.
    assert_eq!(server.vsock_address(), Some((VMADDR_CID_LOCAL, TEST_PORT)));
    assert_eq!(server.path(), None, "vsock server has no filesystem entry");
    let bg = server.run_background();

    let client_t = VsockTransport::connect(VMADDR_CID_LOCAL, TEST_PORT).expect("client connect");
    // Identity is Vsock{cid}, never Local.
    match client_t.peer_identity() {
        PeerIdentity::Vsock { cid } => assert_eq!(cid, VMADDR_CID_LOCAL),
        other => panic!("expected Vsock peer id, got {other}"),
    }
    let client = RpcSession::new(Box::new(client_t), rsbinder::rpc::AddressSpace::Initiator)
        .expect("RpcSession::new");
    let root = client.get_root().expect("get_root over vsock");
    assert_eq!(ping_via(&root, "hi").unwrap(), "pong:hi");

    // Teardown — explicit shutdown + bg.join (same shape as UDS tests).
    drop(root);
    drop(client);
    server.stop_accepting();
    let _ = bg.join();
}

/// Plan 2-20 (`RpcTransport::shutdown`): a thread blocked in `recv_frame`
/// on a vsock connection returns once `shutdown()` is called on the same
/// transport — the primitive that ends a client's incoming-connection
/// threads and any user `serve_blocking` on session death.
#[test]
#[ignore = "needs Linux vsock loopback (modprobe vsock_loopback) or a peer VM"]
fn vsock_shutdown_wakes_blocked_recv() {
    use std::sync::Arc;
    use std::time::Duration;
    use vsock::VMADDR_CID_LOCAL;

    let port = TEST_PORT + 1;
    let server = RpcServer::setup_vsock_server(VMADDR_CID_LOCAL, port).expect("setup_vsock_server");
    server
        .set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))))
        .expect("set_root");
    let bg = server.run_background();

    let t: Arc<dyn RpcTransport> =
        Arc::new(VsockTransport::connect(VMADDR_CID_LOCAL, port).expect("client connect"));
    // The reader reports through a channel, so "shutdown must wake it"
    // is a `recv_timeout` that *fails* on regression. Asserting on
    // `elapsed()` after `reader.join()` cannot: if `shutdown` stops
    // waking the reader, the join never returns and the test hangs
    // until the CI timeout instead of failing here.
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let reader = {
        let t = Arc::clone(&t);
        std::thread::spawn(move || {
            let _ = tx.send(t.recv_frame());
        })
    };
    // Give the reader time to park in `recv`.
    std::thread::sleep(Duration::from_millis(200));
    t.shutdown().expect("shutdown");
    let got = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("shutdown must wake the blocked reader");
    assert!(
        got.is_err(),
        "recv must not return a frame after shutdown: {got:?}"
    );
    reader.join().expect("reader thread");
    server.stop_accepting();
    // Join the workers before the accept loop, as the sibling test does:
    // a worker still serving this client would otherwise outlive the test.
    server.join_workers();
    if let Err(p) = bg.join() {
        eprintln!("WARNING: vsock accept loop panicked: {p:?}");
    }
}

/// Plan 2-20 (`RpcSession::close_session` on a vsock session): a user
/// `serve_blocking` thread ends, the death recipient fires, and the
/// server sees the connection go — the whole teardown path over vsock.
#[test]
#[ignore = "needs Linux vsock loopback (modprobe vsock_loopback) or a peer VM"]
fn vsock_session_shutdown_ends_serve_thread() {
    use std::sync::{mpsc, Arc};
    use std::time::Duration;
    use vsock::VMADDR_CID_LOCAL;

    struct Flag(mpsc::SyncSender<()>);
    impl rsbinder::DeathRecipient for Flag {
        fn binder_died(&self, _who: &rsbinder::WIBinder) {
            let _ = self.0.try_send(());
        }
    }

    let port = TEST_PORT + 2;
    let server = RpcServer::setup_vsock_server(VMADDR_CID_LOCAL, port).expect("setup_vsock_server");
    server
        .set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))))
        .expect("set_root");
    let bg = server.run_background();

    let client_t = VsockTransport::connect(VMADDR_CID_LOCAL, port).expect("client connect");
    let client = RpcSession::new(Box::new(client_t), rsbinder::rpc::AddressSpace::Initiator)
        .expect("RpcSession::new");
    let root = client.get_root().expect("get_root over vsock");
    assert_eq!(ping_via(&root, "pre").unwrap(), "pong:pre");
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    let flag: Arc<Flag> = Arc::new(Flag(tx));
    root.link_to_death(Arc::downgrade(&flag) as _)
        .expect("link_to_death");
    let serving = client.clone();
    let serve = std::thread::spawn(move || serving.serve_blocking());

    std::thread::sleep(Duration::from_millis(200));
    client.close_session();
    assert!(
        rx.recv_timeout(Duration::from_secs(3)).is_ok(),
        "shutdown must fire the linked recipient"
    );
    let _ = serve.join().expect("serve thread joins after shutdown");
    assert!(
        ping_via(&root, "post").is_err(),
        "the session is dead after shutdown"
    );
    drop(root);
    drop(client);
    server.stop_accepting();
    server.join_workers();
    // Surface an accept-loop panic instead of discarding it — a future
    // regression there would otherwise leave every test green.
    if let Err(p) = bg.join() {
        eprintln!("WARNING: vsock accept loop panicked: {p:?}");
    }
}
