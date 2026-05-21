// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Subplan 2-4 track V: vsock backend e2e — **Linux-only, `#[ignore]`
//! by default** (plan V6 environment gate: needs a peer VM or the
//! `vsock_loopback` kernel module + `VMADDR_CID_LOCAL`).
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

#![cfg(all(feature = "rpc-vsock", target_os = "linux"))]

use std::thread;

use rsbinder::rpc::transport::VsockTransport;
use rsbinder::rpc::{AddressSpace, PeerIdentity, RpcSession, RpcTransport};
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

/// AC-4.1/4.2/4.3 over loopback vsock (`VMADDR_CID_LOCAL`).
#[test]
#[ignore = "needs Linux vsock loopback (modprobe vsock_loopback) or a peer VM"]
fn vsock_loopback_e2e() {
    use vsock::{VsockListener, VMADDR_CID_LOCAL};

    let listener =
        VsockListener::bind_with_cid_port(VMADDR_CID_LOCAL, TEST_PORT).expect("vsock bind");
    let server = thread::spawn(move || {
        let (stream, _addr) = listener.accept().expect("vsock accept");
        let t = VsockTransport::from_stream(stream).expect("server transport");
        let session = RpcSession::new(Box::new(t), AddressSpace::Acceptor).expect("RpcSession::new");
        session.set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))));
        let _ = session.serve_blocking();
    });

    let client_t = VsockTransport::connect(VMADDR_CID_LOCAL, TEST_PORT).expect("client connect");
    // AC-4.3: identity is Vsock{cid}, never Local.
    match client_t.peer_identity() {
        PeerIdentity::Vsock { cid } => assert_eq!(cid, VMADDR_CID_LOCAL),
        other => panic!("expected Vsock peer id, got {other}"),
    }
    let client = RpcSession::new(Box::new(client_t), AddressSpace::Initiator).expect("RpcSession::new");
    let root = client.get_root().expect("get_root over vsock");
    assert_eq!(ping_via(&root, "hi").unwrap(), "pong:hi");
    drop(root);
    drop(client);
    server.join().unwrap();
}
