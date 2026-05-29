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
    server.set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
        PingSvc,
    )))));
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
    server.shutdown();
    let _ = bg.join();
}
