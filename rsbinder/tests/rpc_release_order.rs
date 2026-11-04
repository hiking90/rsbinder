// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Where a `DEC_STRONG` lands among the frames of a connection whose no-wait send can stop inside
//! a frame: TCP and TLS (`session` module doc "Deferred `DEC_STRONG`", plan 2-27).
//!
//! Over `mem` and `AF_UNIX` the scenario tests in `rpc_e2e` state these orders. Here the same
//! calls run on the transports that keep what the socket did not take, on one connection and on
//! several.

#![cfg(all(feature = "rpc-tcp-debug", feature = "rpc-tls"))]

use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rsbinder::rpc::rustls::pki_types::pem::PemObject;
use rsbinder::rpc::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rsbinder::rpc::rustls::{ClientConfig, RootCertStore, ServerConfig};
use rsbinder::rpc::transport::TcpDebugTransport;
use rsbinder::rpc::{RpcClientConfig, RpcProxy, RpcServer, RpcServerHandle, RpcSession};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, RemoteProxy, Result, SIBinder, StatusCode,
    TransactionCode, FIRST_CALL_TRANSACTION, FLAG_ONEWAY,
};

const DESC: &str = "rsbinder.test.IReleaseOrder";
const TX_CHILD: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION + 1;

const CA: &str = include_str!("tls_fixtures/ca.crt");
const SRV_CRT: &str = include_str!("tls_fixtures/srv.crt");
const SRV_KEY: &str = include_str!("tls_fixtures/srv.key");

/// A node the peer holds, counted while it lives.
struct Child(Arc<AtomicUsize>);

impl Drop for Child {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Interface for Child {}

impl Remotable for Child {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, _: TransactionCode, _: &mut Parcel, _: &mut Parcel) -> Result<()> {
        Ok(())
    }
    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
        Ok(())
    }
}

struct Parent(Arc<AtomicUsize>);

impl Interface for Parent {}

impl Remotable for Parent {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        _reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            TX_CHILD => {
                self.0.fetch_add(1, Ordering::SeqCst);
                let child = Binder::new(Child(self.0.clone())).as_binder();
                reply.write(&child)
            }
            TX_ECHO => Ok(()),
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum Link {
    Tcp,
    Tls,
}

fn tls_server_config() -> Arc<ServerConfig> {
    let certs = CertificateDer::pem_slice_iter(SRV_CRT.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("server certificate");
    let key = PrivateKeyDer::from_pem_slice(SRV_KEY.as_bytes()).expect("server key");
    Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .expect("server config"),
    )
}

fn tls_client_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(CA.as_bytes()) {
        roots.add(ca.expect("CA")).expect("add CA");
    }
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// A served parent, and a client of it over `link` with `connections` outgoing connections.
struct Fixture {
    session: RpcSession,
    root: SIBinder,
    live: Arc<AtomicUsize>,
    _server: Arc<RpcServer>,
    _accept: Option<RpcServerHandle>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.session.close_session();
    }
}

impl Fixture {
    fn new(link: Link, connections: u32) -> Fixture {
        let live = Arc::new(AtomicUsize::new(0));
        let root_obj = Binder::new(Parent(live.clone())).as_binder();
        let (server, accept, port) = match link {
            Link::Tcp => {
                // `serve_connection` needs a server object, whose own listener is never run.
                let dir = std::env::temp_dir().join(format!(
                    "rsb_release_order_{}_{:p}.sock",
                    std::process::id(),
                    Arc::as_ptr(&live)
                ));
                let server = RpcServer::setup_unix_server(&dir).expect("server");
                let _ = std::fs::remove_file(&dir);
                let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
                let port = listener.local_addr().expect("address").port();
                let accepting = server.clone();
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(stream) = stream else { return };
                        let Ok(t) = TcpDebugTransport::from_stream(stream) else {
                            return;
                        };
                        accepting.serve_connection(Box::new(t));
                    }
                });
                (server, None, port)
            }
            Link::Tls => {
                let server = RpcServer::setup_tcp_server_tls(("127.0.0.1", 0), tls_server_config())
                    .expect("server");
                let port = server.tcp_address().expect("address").port();
                (server, None, port)
            }
        };
        server.set_android13plus(2);
        server.set_max_threads(connections).expect("max threads");
        server.set_root(root_obj).expect("set_root");
        let accept = match link {
            Link::Tcp => accept,
            Link::Tls => Some(server.run_background().expect("run_background")),
        };
        let config = match link {
            Link::Tcp => RpcClientConfig::tcp_debug(([127, 0, 0, 1], port).into(), 2),
            Link::Tls => {
                RpcClientConfig::tls("127.0.0.1", port, "localhost", tls_client_config(), 2)
            }
        };
        let session = RpcSession::setup_client_android13plus_with_config(
            config.outgoing_connections(connections),
        )
        .expect("connect");
        let root = session.get_root().expect("root");
        Fixture {
            session,
            root,
            live,
            _server: server,
            _accept: accept,
        }
    }

    fn call(&self, target: &SIBinder, code: TransactionCode, flags: u32) -> Option<Parcel> {
        let proxy = (**target)
            .as_any()
            .downcast_ref::<RpcProxy>()
            .expect("RpcProxy");
        let data = proxy.prepare_request(DESC).expect("request");
        proxy.submit_transact(code, &data, flags).expect("the call")
    }

    fn child(&self) -> SIBinder {
        let mut reply = self.call(&self.root, TX_CHILD, 0).expect("a reply");
        reply.read::<SIBinder>().expect("the child")
    }

    fn echo(&self) {
        self.call(&self.root, TX_ECHO, 0).expect("a reply");
    }

    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }
}

/// `drop; echo`: the peer has released the node by the reply, on one connection or several.
fn a_release_comes_before_the_next_reply(link: Link, connections: u32) {
    let f = Fixture::new(link, connections);
    for round in 0..200 {
        let child = f.child();
        assert_eq!(f.live(), 1, "{link:?}, round {round}: the child lives");
        drop(child);
        f.echo();
        assert_eq!(
            f.live(),
            0,
            "{link:?}, round {round}: released by the reply"
        );
    }
}

#[test]
fn over_tcp_a_release_comes_before_the_next_reply() {
    a_release_comes_before_the_next_reply(Link::Tcp, 1);
    a_release_comes_before_the_next_reply(Link::Tcp, 3);
}

#[test]
fn over_tls_a_release_comes_before_the_next_reply() {
    a_release_comes_before_the_next_reply(Link::Tls, 1);
    a_release_comes_before_the_next_reply(Link::Tls, 3);
}

/// A release held until the peer paid the oneway's target, as the wait for a `REPLY` reads that
/// payment: `oneway; drop; echo; echo`, and the second `echo`'s reply comes after it.
fn a_release_a_payment_lets_go_comes_before_the_next_call(link: Link, connections: u32) {
    let f = Fixture::new(link, connections);
    for round in 0..200 {
        let child = f.child();
        assert!(f.call(&child, TX_ECHO, FLAG_ONEWAY).is_none());
        drop(child);
        f.echo();
        f.echo();
        assert_eq!(
            f.live(),
            0,
            "{link:?}, round {round}: released by the second reply"
        );
    }
}

#[test]
fn over_tcp_a_release_a_payment_lets_go_comes_before_the_next_call() {
    a_release_a_payment_lets_go_comes_before_the_next_call(Link::Tcp, 1);
    a_release_a_payment_lets_go_comes_before_the_next_call(Link::Tcp, 3);
}

#[test]
fn over_tls_a_release_a_payment_lets_go_comes_before_the_next_call() {
    a_release_a_payment_lets_go_comes_before_the_next_call(Link::Tls, 1);
    a_release_a_payment_lets_go_comes_before_the_next_call(Link::Tls, 3);
}
