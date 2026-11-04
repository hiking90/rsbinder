// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Where a `DEC_STRONG` lands among the frames of a connection whose no-wait send can stop inside
//! a frame (`session` module doc "Deferred `DEC_STRONG`", plan 2-27).
//!
//! Over `mem` and `AF_UNIX` the scenario tests in `rpc_e2e` state these orders. Here the same
//! calls run on TCP and TLS, on one connection and on several, and on `Link::HoldingPins`: TCP
//! under a transport that says nothing of a frame's being taken whole, as vsock does, so a pin
//! holds every release. On several, one thread's frames all take the first free connection, which
//! is what gives them an order: nothing orders frames between two connections.
//!
//! # Mutation gates
//!
//! - `a_release_in_a_handler_comes_before_its_nested_call`, on `HoldingPins`: drop the writes of
//!   what the slot holds from `client_transact`, and the nested call overtakes the release.
//! - `a_handler_can_wait_for_what_it_released`: hold on every pin, as `HoldingPins` does, and the
//!   handler's wait passes with the release still on its slot. So it runs on Linux and Android
//!   only: elsewhere every pin holds (`session` module doc "Deferred `DEC_STRONG`").
//!
//! `ConnGuard::drop`'s writes have their gates in the `session` tests, where a raw peer can see
//! that nothing else writes them: here a next request writes what the slot holds as well.

#![cfg(all(feature = "rpc-tcp-debug", feature = "rpc-tls"))]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use rsbinder::rpc::rustls::pki_types::pem::PemObject;
use rsbinder::rpc::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rsbinder::rpc::rustls::{ClientConfig, RootCertStore, ServerConfig};
use rsbinder::rpc::transport::TcpDebugTransport;
use rsbinder::rpc::{
    PeerIdentity, RpcClientConfig, RpcError, RpcProxy, RpcServer, RpcServerHandle, RpcSession,
    RpcTransport,
};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, RemoteProxy, Result, SIBinder, StatusCode,
    TransactionCode, FIRST_CALL_TRANSACTION, FLAG_ONEWAY,
};

const DESC: &str = "rsbinder.test.IReleaseOrder";
const TX_CHILD: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION + 1;
/// Keep the binder in the request.
const TX_PARK: TransactionCode = FIRST_CALL_TRANSACTION + 2;
/// Drop what is kept, then call the binder in the request.
const TX_RELEASE_AND_CALL: TransactionCode = FIRST_CALL_TRANSACTION + 3;
/// Drop what is kept, then answer whether the client's object went within the milliseconds asked.
const TX_RELEASE_AND_WAIT: TransactionCode = FIRST_CALL_TRANSACTION + 4;
/// Call the binder in the request, then answer how many children live.
const TX_CALL_AND_COUNT: TransactionCode = FIRST_CALL_TRANSACTION + 5;

const CA: &str = include_str!("tls_fixtures/ca.crt");
const SRV_CRT: &str = include_str!("tls_fixtures/srv.crt");
const SRV_KEY: &str = include_str!("tls_fixtures/srv.key");

fn call_binder(
    target: &SIBinder,
    code: TransactionCode,
    flags: u32,
    write: impl FnOnce(&mut Parcel) -> Result<()>,
) -> Result<Option<Parcel>> {
    let proxy = (**target)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .expect("RpcProxy");
    let mut data = proxy.prepare_request(DESC)?;
    write(&mut data)?;
    proxy.submit_transact(code, &data, flags)
}

/// An object the other end holds, counted while it lives; its `Drop` can tell a waiting handler.
struct Counted {
    live: Arc<AtomicUsize>,
    gone: Option<mpsc::Sender<()>>,
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
        if let Some(gone) = &self.gone {
            let _ = gone.send(());
        }
    }
}

impl Interface for Counted {}

impl Remotable for Counted {
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

/// On a call: note how many of the client's objects live, and drop what it was given to drop.
struct Callee {
    live: Arc<AtomicUsize>,
    seen: Arc<AtomicUsize>,
    to_drop: Mutex<Option<SIBinder>>,
}

impl Interface for Callee {}

impl Remotable for Callee {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, _: TransactionCode, _: &mut Parcel, _: &mut Parcel) -> Result<()> {
        self.seen
            .store(self.live.load(Ordering::SeqCst), Ordering::SeqCst);
        drop(self.to_drop.lock().expect("to_drop").take());
        Ok(())
    }
    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
        Ok(())
    }
}

struct Parent {
    live: Arc<AtomicUsize>,
    parked: Mutex<Vec<SIBinder>>,
    gone: Mutex<mpsc::Receiver<()>>,
}

impl Interface for Parent {}

impl Remotable for Parent {
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
            TX_CHILD => {
                self.live.fetch_add(1, Ordering::SeqCst);
                let child = Binder::new(Counted {
                    live: self.live.clone(),
                    gone: None,
                })
                .as_binder();
                reply.write(&child)
            }
            TX_ECHO => Ok(()),
            TX_PARK => {
                let binder: SIBinder = reader.read()?;
                self.parked.lock().expect("parked").push(binder);
                Ok(())
            }
            TX_RELEASE_AND_CALL => {
                let callee: SIBinder = reader.read()?;
                self.parked.lock().expect("parked").clear();
                call_binder(&callee, TX_ECHO, 0, |_| Ok(())).map(drop)
            }
            TX_RELEASE_AND_WAIT => {
                let ms: i32 = reader.read()?;
                self.parked.lock().expect("parked").clear();
                let wait = Duration::from_millis(ms as u64);
                let gone = self.gone.lock().expect("gone").recv_timeout(wait).is_ok();
                reply.write(&gone)
            }
            TX_CALL_AND_COUNT => {
                let callee: SIBinder = reader.read()?;
                call_binder(&callee, TX_ECHO, 0, |_| Ok(()))?;
                reply.write(&(self.live.load(Ordering::SeqCst) as i32))
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
        Ok(())
    }
}

/// TCP that says nothing of a frame's being taken whole, so every pin holds what it releases.
struct HoldsOnPins(TcpDebugTransport);

impl RpcTransport for HoldsOnPins {
    fn send_raw(&self, buf: &[u8]) -> std::result::Result<(), RpcError> {
        self.0.send_raw(buf)
    }
    fn recv_raw(&self, buf: &mut [u8]) -> std::result::Result<usize, RpcError> {
        self.0.recv_raw(buf)
    }
    fn send_raw_draining(
        &self,
        buf: &[u8],
        fds: &[std::os::fd::BorrowedFd<'_>],
        drain: &mut dyn FnMut() -> std::result::Result<(), RpcError>,
    ) -> std::result::Result<(), RpcError> {
        self.0.send_raw_draining(buf, fds, drain)
    }
    fn __send_no_wait(
        &self,
        buf: &[u8],
        fds: &[std::os::fd::BorrowedFd<'_>],
    ) -> Option<std::result::Result<bool, RpcError>> {
        self.0.__send_no_wait(buf, fds)
    }
    fn __carrying(&self) -> bool {
        self.0.__carrying()
    }
    fn __flush_carried(
        &self,
        drain: Option<&mut dyn FnMut() -> std::result::Result<(), RpcError>>,
    ) -> std::result::Result<(), RpcError> {
        self.0.__flush_carried(drain)
    }
    fn __wait_for_room(&self) -> std::result::Result<bool, RpcError> {
        self.0.__wait_for_room()
    }
    fn peer_identity(&self) -> PeerIdentity {
        self.0.peer_identity()
    }
    fn describe(&self) -> &str {
        self.0.describe()
    }
    fn set_read_timeout(&self, t: Option<Duration>) -> std::result::Result<(), RpcError> {
        self.0.set_read_timeout(t)
    }
    fn set_write_timeout(&self, t: Option<Duration>) -> std::result::Result<(), RpcError> {
        self.0.set_write_timeout(t)
    }
    fn peer_closed(&self) -> Option<bool> {
        self.0.peer_closed()
    }
    fn shutdown_handle(&self) -> Option<Box<dyn FnOnce() + Send>> {
        self.0.shutdown_handle()
    }
    fn peer_close_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.0.peer_close_fd()
    }
    fn shutdown(&self) -> std::result::Result<(), RpcError> {
        self.0.shutdown()
    }
}

#[derive(Clone, Copy, Debug)]
enum Link {
    Tcp,
    Tls,
    HoldingPins,
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

fn holding_pins(stream: TcpStream) -> Result<Box<dyn RpcTransport>> {
    Ok(Box::new(HoldsOnPins(TcpDebugTransport::from_stream(
        stream,
    )?)))
}

/// A served parent, and a client of it over `link` with `connections` outgoing connections.
struct Fixture {
    session: RpcSession,
    root: SIBinder,
    /// The server's children.
    live: Arc<AtomicUsize>,
    /// Tells `TX_RELEASE_AND_WAIT` that the client's object went.
    gone: mpsc::Sender<()>,
    _server: Arc<RpcServer>,
    _accept: Option<RpcServerHandle>,
    _tcp_accept: Option<TcpAccept>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.session.close_session();
    }
}

/// `tcp_server`'s accept thread, which its drop wakes with a connection of its own and joins.
struct TcpAccept {
    stop: Arc<AtomicBool>,
    addr: SocketAddr,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TcpAccept {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unwoken, the thread stays in `accept`: a join would turn a failing test into a hang.
        if TcpStream::connect(self.addr).is_ok() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

/// A server whose own listener is never run, fed plain TCP connections through `wrap`.
fn tcp_server(
    root_tag: *const AtomicUsize,
    wrap: fn(TcpStream) -> Result<Box<dyn RpcTransport>>,
) -> (Arc<RpcServer>, SocketAddr, Option<TcpAccept>) {
    let path = std::env::temp_dir().join(format!(
        "rsb_release_order_{}_{root_tag:p}.sock",
        std::process::id()
    ));
    let server = RpcServer::setup_unix_server(&path).expect("server");
    let _ = std::fs::remove_file(&path);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("address");
    let accepting = server.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let thread = std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stopping.load(Ordering::SeqCst) {
                return;
            }
            let Ok(stream) = stream else { return };
            let Ok(t) = wrap(stream) else { return };
            accepting.serve_connection(t);
        }
    });
    let accept = TcpAccept {
        stop,
        addr,
        thread: Some(thread),
    };
    (server, addr, Some(accept))
}

impl Fixture {
    fn new(link: Link, connections: u32) -> Fixture {
        let live = Arc::new(AtomicUsize::new(0));
        let (gone, gone_rx) = mpsc::channel();
        let root_obj = Binder::new(Parent {
            live: live.clone(),
            parked: Mutex::new(Vec::new()),
            gone: Mutex::new(gone_rx),
        })
        .as_binder();
        let tag = Arc::as_ptr(&live);
        let (server, addr, tcp_accept) = match link {
            Link::Tcp => tcp_server(tag, |s| Ok(Box::new(TcpDebugTransport::from_stream(s)?))),
            Link::HoldingPins => tcp_server(tag, holding_pins),
            Link::Tls => {
                let server = RpcServer::setup_tcp_server_tls(("127.0.0.1", 0), tls_server_config())
                    .expect("server");
                let addr = server.tcp_address().expect("address");
                (server, addr, None)
            }
        };
        server.set_android13plus(2);
        server.set_max_threads(connections).expect("max threads");
        server.set_root(root_obj).expect("set_root");
        let accept =
            matches!(link, Link::Tls).then(|| server.run_background().expect("run_background"));
        let config = match link {
            Link::Tcp => RpcClientConfig::tcp_debug(addr, 2),
            Link::HoldingPins => RpcClientConfig::new(2, move || {
                holding_pins(TcpStream::connect(addr).map_err(StatusCode::from)?)
            }),
            Link::Tls => RpcClientConfig::tls(
                "127.0.0.1",
                addr.port(),
                "localhost",
                tls_client_config(),
                2,
            ),
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
            gone,
            _server: server,
            _accept: accept,
            _tcp_accept: tcp_accept,
        }
    }

    fn child(&self) -> SIBinder {
        let reply = call_binder(&self.root, TX_CHILD, 0, |_| Ok(())).expect("the call");
        reply.expect("a reply").read().expect("the child")
    }

    fn echo(&self) {
        call_binder(&self.root, TX_ECHO, 0, |_| Ok(()))
            .expect("the call")
            .expect("a reply");
    }

    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Hand the server an object of this end that only the server keeps; its count, and it gone.
    fn park_a_counted_object(&self, tell: bool) -> Arc<AtomicUsize> {
        let live = Arc::new(AtomicUsize::new(1));
        let object = Binder::new(Counted {
            live: live.clone(),
            gone: tell.then(|| self.gone.clone()),
        })
        .as_binder();
        call_binder(&self.root, TX_PARK, 0, |d| d.write(&object)).expect("park");
        live
    }
}

const LINKS: [Link; 3] = [Link::Tcp, Link::Tls, Link::HoldingPins];

/// `drop; echo`: the peer has released the node by the reply, on one connection or several.
#[test]
fn a_release_comes_before_the_next_reply() {
    for link in LINKS {
        for connections in [1, 3] {
            let f = Fixture::new(link, connections);
            for round in 0..100 {
                let child = f.child();
                assert_eq!(f.live(), 1, "{link:?}, round {round}: the child lives");
                drop(child);
                f.echo();
                assert_eq!(
                    f.live(),
                    0,
                    "{link:?}, {connections}, round {round}: released by the reply"
                );
            }
        }
    }
}

/// `oneway; drop; echo; echo`: a release held for the oneway's payment precedes the second reply.
#[test]
fn a_release_a_payment_lets_go_comes_before_the_next_call() {
    for link in LINKS {
        for connections in [1, 3] {
            let f = Fixture::new(link, connections);
            for round in 0..100 {
                let child = f.child();
                let oneway = call_binder(&child, TX_ECHO, FLAG_ONEWAY, |_| Ok(()));
                assert!(oneway.expect("the oneway").is_none());
                drop(child);
                f.echo();
                f.echo();
                assert_eq!(
                    f.live(),
                    0,
                    "{link:?}, {connections}, round {round}: released by the second reply"
                );
            }
        }
    }
}

/// A handler's `drop` on its `Incoming` pin, then a nested call: the callee sees the release.
#[test]
fn a_release_in_a_handler_comes_before_its_nested_call() {
    for link in LINKS {
        let f = Fixture::new(link, 1);
        for round in 0..50 {
            let live = f.park_a_counted_object(false);
            let seen = Arc::new(AtomicUsize::new(usize::MAX));
            let callee = Binder::new(Callee {
                live: live.clone(),
                seen: seen.clone(),
                to_drop: Mutex::new(None),
            })
            .as_binder();
            call_binder(&f.root, TX_RELEASE_AND_CALL, 0, |d| d.write(&callee)).expect("the call");
            assert_eq!(
                seen.load(Ordering::SeqCst),
                0,
                "{link:?}, round {round}: released before the nested call"
            );
        }
    }
}

/// A nested handler's `drop` on the `Outgoing` pin of its reply wait comes before its `REPLY`.
#[test]
fn a_release_in_a_nested_handler_comes_before_its_reply() {
    for link in LINKS {
        let f = Fixture::new(link, 1);
        for round in 0..50 {
            let callee = Binder::new(Callee {
                live: Arc::new(AtomicUsize::new(0)),
                seen: Arc::new(AtomicUsize::new(0)),
                to_drop: Mutex::new(Some(f.child())),
            })
            .as_binder();
            let reply = call_binder(&f.root, TX_CALL_AND_COUNT, 0, |d| d.write(&callee));
            let children: i32 = reply
                .expect("the call")
                .expect("a reply")
                .read()
                .expect("the count");
            assert_eq!(
                children, 0,
                "{link:?}, round {round}: released by its REPLY"
            );
        }
    }
}

/// `drop; wait for the peer's Drop` in a handler: over TCP and TLS the release goes at once.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_handler_can_wait_for_what_it_released() {
    for link in [Link::Tcp, Link::Tls] {
        let f = Fixture::new(link, 1);
        let live = f.park_a_counted_object(true);
        let reply = call_binder(&f.root, TX_RELEASE_AND_WAIT, 0, |d| d.write(&5000i32));
        let gone: bool = reply
            .expect("the call")
            .expect("a reply")
            .read()
            .expect("the answer");
        assert!(
            gone,
            "{link:?}: the release reached the peer in the handler"
        );
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }
}

/// Where a pin cannot write, the handler's wait passes and the reply brings the release.
#[test]
fn a_pin_that_holds_releases_with_the_reply() {
    let f = Fixture::new(Link::HoldingPins, 1);
    let live = f.park_a_counted_object(true);
    let reply = call_binder(&f.root, TX_RELEASE_AND_WAIT, 0, |d| d.write(&200i32));
    let gone: bool = reply
        .expect("the call")
        .expect("a reply")
        .read()
        .expect("the answer");
    assert!(!gone, "the release waits for the reply");
    assert_eq!(live.load(Ordering::SeqCst), 0, "and comes before it");
}
