// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Subplan 2-4 track V + **Plan 2-15 E2**: vsock backend e2e — **Linux-
//! only, `#[ignore]` by default** (plan V6 environment gate: needs a
//! peer VM or the `vsock_loopback` kernel module + `VMADDR_CID_LOCAL`).
//!
//! Run manually on a suitable Linux host with:
//! ```text
//! sudo modprobe vsock_loopback   # if not built-in
//! cargo test -p rsbinder --features rpc-vsock --test rpc_vsock -- --ignored
//! ```
//!
//! Demonstrates AC-4.1 (the 2-2/2-3 core runs unmodified with the
//! transport swapped to vsock), AC-4.2 (host↔guest value round-trip),
//! AC-4.3 (`PeerIdentity::Vsock{cid}`, never mis-reported as `Local`).
//!
//! **E2 cleanup**: the original e2e built a raw `VsockListener` +
//! thread-spawned `RpcSession::new(..)` server **outside** `RpcServer`
//! to work around the (pre-E0) `RpcServer.listener: UnixListener` lock-
//! in. With E0+E2 (`RpcServer::setup_vsock_server`) that workaround is
//! gone — the server is built with the same factory + `run_background`
//! pattern as the UDS e2e suite. The raw `VsockListener` path is no
//! longer exercised here, but the underlying `VsockTransport::from_stream`
//! / `VsockTransport::connect` still are (one through the server's
//! accept loop, the other through the test's client construction).

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

/// AC-4.1/4.2/4.3 over loopback vsock (`VMADDR_CID_LOCAL`) — server
/// built with **Plan 2-15 E2** `RpcServer::setup_vsock_server`.
///
/// Server-side: the same factory + `run_background` shape used by the
/// UDS e2e suite — backend swap is the only difference. Client-side:
/// `VsockTransport::connect` (unchanged) so the `PeerIdentity::Vsock`
/// assertion (AC-4.3) keeps its original wire-level reach.
#[test]
#[ignore = "needs Linux vsock loopback (modprobe vsock_loopback) or a peer VM"]
fn vsock_loopback_e2e() {
    use vsock::VMADDR_CID_LOCAL;

    // E2: same `RpcServer` API as UDS, vsock-backed listener.
    let server =
        RpcServer::setup_vsock_server(VMADDR_CID_LOCAL, TEST_PORT).expect("setup_vsock_server");
    server.set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
        PingSvc,
    )))));
    // E0 accessor gates: vsock_address `Some`, fs path `None`.
    assert_eq!(server.vsock_address(), Some((VMADDR_CID_LOCAL, TEST_PORT)));
    assert_eq!(server.path(), None, "vsock server has no filesystem entry");
    let bg = server.run_background();

    let client_t = VsockTransport::connect(VMADDR_CID_LOCAL, TEST_PORT).expect("client connect");
    // AC-4.3: identity is Vsock{cid}, never Local.
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
