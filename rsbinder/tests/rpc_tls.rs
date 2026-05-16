// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Subplan 2-4 track T: the TLS backend, exercised with the **2-2/2-3
//! core unchanged** — only the transport is swapped (additive
//! invariant AC-4.1). Covers AC-4.4 (valid cert handshake + AIDL
//! round-trip + `Certificate` peer-id), AC-4.5 (untrusted cert →
//! handshake reject, **zero RPC payload**), AC-4.6 (no plaintext
//! network backend — enforced by type/absence, noted here).
//!
//! Separate test binary; `#![cfg(feature = "rpc-tls")]` so it only
//! builds/runs with the feature (default test runs don't pay rustls).

#![cfg(feature = "rpc-tls")]

use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use rsbinder::rpc::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rsbinder::rpc::rustls::{ClientConfig, RootCertStore, ServerConfig};
use rsbinder::rpc::transport::TlsTransport;
use rsbinder::rpc::{AddressSpace, PeerIdentity, RpcSession, RpcTransport};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, Result, SIBinder, Status, StatusCode, TransactionCode,
    FIRST_CALL_TRANSACTION,
};

const DESC: &str = "rsbinder.test.IPing";
const TX_PING: TransactionCode = FIRST_CALL_TRANSACTION;

const CA: &str = include_str!("tls_fixtures/ca.crt");
const SRV_CRT: &str = include_str!("tls_fixtures/srv.crt");
const SRV_KEY: &str = include_str!("tls_fixtures/srv.key");
const ROGUE_CRT: &str = include_str!("tls_fixtures/rogue.crt");
const ROGUE_KEY: &str = include_str!("tls_fixtures/rogue.key");

fn certs(pem: &str) -> Vec<CertificateDer<'static>> {
    rustls_pemfile::certs(&mut BufReader::new(pem.as_bytes()))
        .collect::<std::result::Result<_, _>>()
        .expect("parse certs")
}
fn key(pem: &str) -> PrivateKeyDer<'static> {
    rustls_pemfile::private_key(&mut BufReader::new(pem.as_bytes()))
        .expect("parse key")
        .expect("a key")
}

fn server_config(cert_pem: &str, key_pem: &str) -> Arc<ServerConfig> {
    Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs(cert_pem), key(key_pem))
            .expect("server config"),
    )
}
fn client_config_trusting(ca_pem: &str) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for c in certs(ca_pem) {
        roots.add(c).expect("add ca");
    }
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

// ---- minimal echo fixture (server stub reused unmodified) ----------

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
fn ping_on_transact(
    s: &dyn IPing,
    code: TransactionCode,
    reader: &mut Parcel,
    reply: &mut Parcel,
) -> Result<()> {
    match code {
        TX_PING => {
            let a: String = reader.read()?;
            match s.ping(&a) {
                Ok(v) => {
                    reply.write(&Status::from(StatusCode::Ok))?;
                    reply.write(&v)
                }
                Err(e) => reply.write(&Status::from(e)),
            }
        }
        _ => Err(StatusCode::UnknownTransaction),
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
        ping_on_transact(&*self.0, code, reader, reply)
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

/// AC-4.1 / AC-4.4: valid cert → handshake + the unchanged 2-2/2-3
/// AIDL e2e over TLS; both ends see a `Certificate` peer identity.
#[test]
fn tls_valid_cert_e2e_and_peer_identity() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (tcp, _) = listener.accept().expect("accept");
        let t = TlsTransport::accept(tcp, srv_cfg).expect("server handshake");
        let session = RpcSession::new(Box::new(t), AddressSpace::Acceptor);
        session.set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))));
        let _ = session.serve_blocking();
    });

    let tcp = TcpStream::connect(addr).expect("tcp connect");
    let client_t = TlsTransport::connect(tcp, "localhost", client_config_trusting(CA))
        .expect("client handshake");
    // AC-4.4: peer identity is a leaf-cert fingerprint.
    match client_t.peer_identity() {
        PeerIdentity::Certificate(c) => {
            assert_eq!(c.fingerprint().len(), 32);
            assert_eq!(c.fingerprint_hex().len(), 64);
        }
        other => panic!("expected Certificate peer id, got {other}"),
    }

    let client = RpcSession::new(Box::new(client_t), AddressSpace::Initiator);
    let root = client.get_root().expect("get_root over TLS");
    assert_eq!(ping_via(&root, "hello").unwrap(), "pong:hello");
    assert_eq!(ping_via(&root, "").unwrap(), "pong:");
    drop(root);
    drop(client);
    server.join().unwrap();
}

/// AC-4.5 (security, 1st class): a server cert NOT signed by the
/// client's trusted CA must be rejected **at the handshake** — the
/// client never obtains a session, so zero RPC payload is exchanged.
#[test]
fn tls_untrusted_cert_rejected_at_handshake() {
    // Server presents a self-signed rogue cert; client only trusts CA.
    let rogue_cfg = server_config(ROGUE_CRT, ROGUE_KEY);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        if let Ok((tcp, _)) = listener.accept() {
            // Server-side handshake may fail too (client aborts);
            // either way no RPC layer is constructed.
            let _ = TlsTransport::accept(tcp, rogue_cfg);
        }
    });

    let tcp = TcpStream::connect(addr).expect("tcp connect");
    let res = TlsTransport::connect(tcp, "localhost", client_config_trusting(CA));
    assert!(
        res.is_err(),
        "AC-4.5: untrusted server cert must fail the handshake (no session, no payload)"
    );
    let _ = server.join();
}

/// AC-4.6 documentation gate: there is **no** plaintext-network
/// constructor in the RPC public API — `tcp_debug` is the only TCP
/// path, it is `rpc-tcp-debug`-gated and hard-wired `Anonymous`, and
/// `tls` is the only real-network transport. This is enforced by
/// *absence/type*, not a runtime check; this test documents the
/// invariant next to the TLS tests so a regression that adds a
/// `setup_tcp_*`/plaintext-net constructor is caught in review.
#[test]
fn no_plaintext_network_backend_in_api() {
    // If a plaintext network transport were ever added, this file
    // would be the natural place to construct it — its continued
    // absence is the gate. (Compile-time: nothing to call.)
}
