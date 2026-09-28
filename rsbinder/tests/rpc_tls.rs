// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The TLS backend, exercised with the **core
//! unchanged** — only the transport is swapped. Covers a valid cert
//! handshake + AIDL round-trip + `Certificate` peer-id, an untrusted
//! cert → handshake reject (**zero RPC payload**).
//!
//! Invariant kept by review, not by a test: the RPC public API has no
//! plaintext-network constructor. `tcp_debug` is the only TCP path, it is
//! `rpc-tcp-debug`-gated and hard-wired `Anonymous`, and `tls` is the only
//! real-network transport.
//!
//! Separate test binary; `#![cfg(feature = "rpc-tls")]` so it only
//! builds/runs with the feature (default test runs don't pay rustls).
//!
//! # Shutdown racing a send
//!
//! `tls_shutdown_racing_a_send_is_still_a_clean_close_for_the_peer` checks
//! two promises that a shutdown which simply cut the socket would break.
//! The peer's: `UncleanEndOfStream` is reserved for a stream somebody cut,
//! so a deliberate close owes it `close_notify` — and only the thread
//! holding `wlock` may put that on the wire, so `shutdown` waits for the
//! send in flight rather than strand the alert. The sender's: `Ok` means
//! the frame went out. A send that started after `shutdown` is refused, and
//! the alert is queued only once the lock is held, so no frame can be
//! encrypted behind it, where the peer, having read the alert, would
//! discard it (RFC 8446 §6.1) after the sender was told `Ok`. The wait is
//! bounded, so a peer that has stopped reading still cannot hold teardown.
//!
//! The window is a few instructions wide, so the test races it repeatedly
//! instead of pinning one interleaving, and counts on both ends: every
//! `Ok` the sender saw is a frame the peer received, and the end the peer
//! reads is never an unclean one. A cut *mid-frame* would read as
//! `Truncated`, and the frame it cut was reported failed, so the counts
//! still agree — the assertion tolerates it without naming it.
//!
//! `entry_tls_requires_explicit_config` checks both sides because the
//! server half has no compile-time signal at all
//! (`serve(..).add(..).spawn()` looks complete on its own), which makes it
//! the likelier mistake.
//!
//! # Mutation gates
//!
//! - `tls_concurrent_bidirectional_duplex`: the `RpcTransport` contract
//!   requires a sender and a receiver thread on one transport at once. A
//!   single mutex over all TLS I/O deadlocks here — a blocked `recv` holds
//!   the lock the `send` needs; the split between `Mutex<Connection>` and
//!   the separate `wlock` is what passes.
//! - `tls_large_frame_over_64kib_roundtrips`: a single unchunked
//!   `writer().write_all` of a payload above rustls's ~64 KiB sendable
//!   plaintext buffer fails with `WriteZero`; `send_raw` must chunk the
//!   plaintext and interleave encrypt-drain.
//! - `setup_tcp_server_tls_e2e`: dropping the `tls_config` store in
//!   `setup_tcp_server_tls` (or making `tls_snapshot()` return `None`)
//!   sends the accepted `TcpStream` down the plain branch, which refuses it
//!   ("plain-text TCP server is not exposed"); the worker exits without
//!   serving and the client's TLS handshake fails or times out.

#![cfg(feature = "rpc-tls")]

use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::rpc::rustls::pki_types::pem::PemObject;
use rsbinder::rpc::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rsbinder::rpc::rustls::{ClientConfig, RootCertStore, ServerConfig};
use rsbinder::rpc::transport::TlsTransport;
use rsbinder::rpc::{AddressSpace, PeerIdentity, RpcError, RpcSession, RpcTransport};
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
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<std::result::Result<_, _>>()
        .expect("parse certs")
}
fn key(pem: &str) -> PrivateKeyDer<'static> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes()).expect("parse key")
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

/// Valid cert: handshake + AIDL e2e over TLS; both ends see a `Certificate` peer identity.
#[test]
fn tls_valid_cert_e2e_and_peer_identity() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (tcp, _) = listener.accept().expect("accept");
        let t = TlsTransport::accept(tcp, srv_cfg).expect("server handshake");
        let session =
            RpcSession::new(Box::new(t), AddressSpace::Acceptor).expect("RpcSession::new");
        session
            .set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
                PingSvc,
            )))))
            .expect("set_root");
        let _ = session.serve_blocking();
    });

    let tcp = TcpStream::connect(addr).expect("tcp connect");
    let client_t = TlsTransport::connect(tcp, "localhost", client_config_trusting(CA))
        .expect("client handshake");
    // Peer identity is a leaf-cert fingerprint.
    match client_t.peer_identity() {
        PeerIdentity::Certificate(c) => {
            assert_eq!(c.fingerprint().len(), 32);
            assert_eq!(c.fingerprint_hex().len(), 64);
        }
        other => panic!("expected Certificate peer id, got {other}"),
    }

    let client =
        RpcSession::new(Box::new(client_t), AddressSpace::Initiator).expect("RpcSession::new");
    let root = client.get_root().expect("get_root over TLS");
    assert_eq!(ping_via(&root, "hello").unwrap(), "pong:hello");
    assert_eq!(ping_via(&root, "").unwrap(), "pong:");

    // Plan 10-0: no uid, fds or shared kernel over TCP; CALLBACKS needs incoming connections.
    use rsbinder::TransportCaps;
    assert_eq!(client.caps(), TransportCaps::NONE);
    assert_eq!(
        client
            .caps()
            .require(TransportCaps::CALLBACKS, "streaming sink"),
        Err(StatusCode::InvalidOperation)
    );
    assert_eq!(
        client.caps().require(TransportCaps::FD_PASSING, "a pipe"),
        Err(StatusCode::InvalidOperation)
    );
    // The endpoint agrees, without needing a session at all.
    assert_eq!(
        rsbinder::Endpoint::Tls("localhost".into(), addr.port()).static_caps(),
        TransportCaps::NONE
    );

    drop(root);
    drop(client);
    server.join().unwrap();
}

/// TLS over a `UnixStream`, as AOSP `RpcTransportCtx::newTransport(fd)` is socket-kind-agnostic.
#[test]
fn tls_over_unix_socket_e2e() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");

    let server = thread::spawn(move || {
        let t = TlsTransport::accept_stream(Box::new(s_srv), srv_cfg)
            .expect("server TLS handshake over unix");
        let session =
            RpcSession::new(Box::new(t), AddressSpace::Acceptor).expect("RpcSession::new");
        session
            .set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
                PingSvc,
            )))))
            .expect("set_root");
        let _ = session.serve_blocking();
    });

    let client_t =
        TlsTransport::connect_stream(Box::new(s_cli), "localhost", client_config_trusting(CA))
            .expect("client TLS handshake over unix");
    match client_t.peer_identity() {
        PeerIdentity::Certificate(c) => assert_eq!(c.fingerprint().len(), 32),
        other => panic!("expected Certificate peer id over unix, got {other}"),
    }
    let client =
        RpcSession::new(Box::new(client_t), AddressSpace::Initiator).expect("RpcSession::new");
    let root = client.get_root().expect("get_root over TLS-on-unix");
    assert_eq!(ping_via(&root, "unix-tls").unwrap(), "pong:unix-tls");
    assert_eq!(ping_via(&root, "").unwrap(), "pong:");
    drop(root);
    drop(client);
    server.join().unwrap();
}

/// TCP end without `close_notify` = `UncleanEndOfStream` (truncation), even at a frame boundary.
#[test]
fn tls_eof_without_close_notify_is_unclean() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");
    let server = thread::spawn(move || {
        let t = TlsTransport::accept_stream(Box::new(s_srv), srv_cfg).expect("server handshake");
        // Dropped without `shutdown()`: the fd closes, no close_notify goes out.
        drop(t);
    });
    let client =
        TlsTransport::connect_stream(Box::new(s_cli), "localhost", client_config_trusting(CA))
            .expect("client handshake");
    server.join().unwrap();
    match client.recv_frame() {
        Err(RpcError::UncleanEndOfStream) => {}
        other => panic!("expected UncleanEndOfStream at a frame boundary, got {other:?}"),
    }
}

/// `shutdown()` sends `close_notify` before the socket shutdown: the peer sees a clean end.
#[test]
fn tls_shutdown_is_a_clean_close_for_the_peer() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");
    let server = thread::spawn(move || {
        let t = TlsTransport::accept_stream(Box::new(s_srv), srv_cfg).expect("server handshake");
        t.shutdown().expect("shutdown");
    });
    let client =
        TlsTransport::connect_stream(Box::new(s_cli), "localhost", client_config_trusting(CA))
            .expect("client handshake");
    server.join().unwrap();
    match client.recv_frame() {
        Err(RpcError::EndOfStream) => {}
        other => panic!("expected the clean EndOfStream after close_notify, got {other:?}"),
    }
}

/// Plan 2-21 D-3: our own `shutdown()` ends our reader with a clean `EndOfStream`; twice is `Ok`.
#[test]
fn tls_local_shutdown_is_a_clean_end_for_our_own_reader() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let t = TlsTransport::accept_stream(Box::new(s_srv), srv_cfg).expect("server handshake");
        // Ends handshake I/O before the client's `SHUT_RD`, which would `EPIPE` a pending write.
        t.send_frame(b"hello").expect("send after handshake");
        // Hold our end until the client has looked: the only end it sees is its own.
        let _ = done_rx.recv();
        drop(t);
    });
    let client =
        TlsTransport::connect_stream(Box::new(s_cli), "localhost", client_config_trusting(CA))
            .expect("client handshake");
    assert_eq!(client.recv_frame().expect("server's frame"), b"hello");
    client.shutdown().expect("shutdown");
    client.shutdown().expect("a second shutdown is Ok");
    match client.recv_frame() {
        Err(RpcError::EndOfStream) => {}
        other => panic!("our own shutdown must read as a clean end, got {other:?}"),
    }
    let _ = done_tx.send(());
    server.join().unwrap();
}

/// The peer reads every frame a racing send saw `Ok`, then a clean end; see module doc.
#[test]
fn tls_shutdown_racing_a_send_is_still_a_clean_close_for_the_peer() {
    for round in 0..48u64 {
        let srv_cfg = server_config(SRV_CRT, SRV_KEY);
        let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");
        let (t_tx, t_rx) = std::sync::mpsc::channel::<Arc<TlsTransport>>();
        let sender = thread::spawn(move || {
            let t = Arc::new(
                TlsTransport::accept_stream(Box::new(s_srv), srv_cfg).expect("server handshake"),
            );
            t_tx.send(Arc::clone(&t)).expect("hand the transport over");
            // Small frames: no send parks, and the frame boundary the shutdown must hit recurs.
            let mut sent_ok = 0usize;
            while t.send_frame(b"tick").is_ok() {
                sent_ok += 1;
            }
            sent_ok
        });
        let client =
            TlsTransport::connect_stream(Box::new(s_cli), "localhost", client_config_trusting(CA))
                .expect("client handshake");
        let server_t = t_rx.recv().expect("transport from the sender thread");
        // The delay walks across rounds so the shutdown lands at a different point each time.
        let closer = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_micros(round * 40));
            server_t.shutdown().expect("shutdown");
        });
        let mut received = 0usize;
        let end = loop {
            match client.recv_frame() {
                Ok(f) => {
                    assert_eq!(f, b"tick", "frames must arrive intact until the end");
                    received += 1;
                }
                Err(e) => break e,
            }
        };
        assert!(
            !matches!(end, RpcError::UncleanEndOfStream),
            "round {round}: a shutdown racing a send left the peer an unclean end"
        );
        closer.join().unwrap();
        let sent_ok = sender.join().unwrap();
        assert_eq!(
            received, sent_ok,
            "round {round}: the sender was told {sent_ok} frames went out, the peer read {received}"
        );
    }
}

/// `RpcSession::setup_tcp_client_tls` (TCP + TLS + r34 session) works against a TLS server.
#[test]
fn setup_tcp_client_tls_convenience_e2e() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (tcp, _) = listener.accept().expect("accept");
        let t = TlsTransport::accept(tcp, srv_cfg).expect("server handshake");
        let session =
            RpcSession::new(Box::new(t), AddressSpace::Acceptor).expect("RpcSession::new");
        session
            .set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
                PingSvc,
            )))))
            .expect("set_root");
        let _ = session.serve_blocking();
    });

    let client = RpcSession::setup_tcp_client_tls(addr, "localhost", client_config_trusting(CA))
        .expect("setup_tcp_client_tls");
    let root = client.get_root().expect("get_root");
    assert_eq!(ping_via(&root, "conv").unwrap(), "pong:conv");
    drop(root);
    drop(client);
    server.join().unwrap();
}

/// Plan 2-17 `tls://`: `ServeOptions::tls`, and `open_with` naming `localhost` for `127.0.0.1`.
#[test]
fn entry_tls_serve_and_client() {
    let svc = Interface::as_binder(&Binder::new(BnPing(Box::new(PingSvc))));
    let guard = rsbinder::serve("tls://127.0.0.1:0")
        .expect("serve tls://")
        .with(|o| o.tls = Some(server_config(SRV_CRT, SRV_KEY)))
        .add("ping", svc)
        .expect("add")
        .spawn()
        .expect("spawn");

    let addr = guard
        .server()
        .expect("rpc server")
        .tcp_address()
        .expect("bound TCP address");

    let client = rsbinder::Client::open_with(&format!("tls://{addr}"), |o, _endpoint| {
        o.tls = Some(client_config_trusting(CA));
        // The fixture cert names `localhost`, not the dialed address in the URI.
        o.tls_server_name = Some("localhost".to_string());
    })
    .expect("Client::open_with tls://");

    let root = client.binder("ping").expect("lookup ping");
    assert_eq!(ping_via(&root, "entry").unwrap(), "pong:entry");
    drop(root);
    drop(client);
}

/// `tls://` without a config is refused on both sides before any socket work (URI has no trust).
#[test]
fn entry_tls_requires_explicit_config() {
    let err = rsbinder::Client::open("tls://127.0.0.1:1")
        .expect_err("tls:// without ClientOptions::tls must fail");
    assert_eq!(err, StatusCode::BadValue);

    let err = rsbinder::serve("tls://127.0.0.1:0")
        .expect("parse")
        .spawn()
        .expect_err("tls:// without ServeOptions::tls must fail");
    assert_eq!(err, StatusCode::BadValue);
}

/// Both sides refuse a Unix `fd_modes`/`fd_mode` over `tls://`: TLS cannot carry `SCM_RIGHTS`.
#[test]
fn entry_tls_rejects_unix_fd_mode() {
    use rsbinder::rpc::FileDescriptorTransportMode;
    let err = rsbinder::serve("tls://127.0.0.1:0")
        .expect("parse")
        .with(|o| {
            o.tls = Some(server_config(SRV_CRT, SRV_KEY));
            o.fd_modes = Some(vec![FileDescriptorTransportMode::Unix]);
        })
        .spawn()
        .expect_err("Unix fd passing is not available over TLS");
    assert_eq!(err, StatusCode::BadValue);

    let err = rsbinder::Client::open_with("tls://127.0.0.1:1", |o, _| {
        o.tls = Some(client_config_trusting(CA));
        o.fd_mode = Some(FileDescriptorTransportMode::Unix);
    })
    .expect_err("Unix fd passing is not available over TLS");
    assert_eq!(err, StatusCode::BadValue);
}

/// Full duplex on both ends of one `TlsTransport`, FIFO kept; see module doc "Mutation gates".
#[test]
fn tls_concurrent_bidirectional_duplex() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");
    let h = thread::spawn(move || {
        TlsTransport::accept_stream(Box::new(s_srv), srv_cfg).expect("server handshake")
    });
    let cli =
        TlsTransport::connect_stream(Box::new(s_cli), "localhost", client_config_trusting(CA))
            .expect("client handshake");
    let srv = Arc::new(h.join().unwrap());
    let cli = Arc::new(cli);

    let n = 300usize;

    // Sender + receiver on the SAME transport; one sender per direction keeps FIFO checkable.
    let srv_s = Arc::clone(&srv);
    let srv_send = thread::spawn(move || {
        for i in 0..n {
            srv_s
                .send_frame(format!("s{i}").as_bytes())
                .expect("srv send");
        }
    });
    let srv_r = Arc::clone(&srv);
    let srv_recv = thread::spawn(move || {
        for i in 0..n {
            assert_eq!(
                srv_r.recv_frame().expect("srv recv"),
                format!("c{i}").into_bytes()
            );
        }
    });
    let cli_s = Arc::clone(&cli);
    let cli_send = thread::spawn(move || {
        for i in 0..n {
            cli_s
                .send_frame(format!("c{i}").as_bytes())
                .expect("cli send");
        }
    });
    let cli_r = Arc::clone(&cli);
    for i in 0..n {
        assert_eq!(
            cli_r.recv_frame().expect("cli recv"),
            format!("s{i}").into_bytes()
        );
    }
    srv_send.join().unwrap();
    srv_recv.join().unwrap();
    cli_send.join().unwrap();
}

/// A frame above rustls's ~64 KiB sendable buffer round-trips; see module doc "Mutation gates".
#[test]
fn tls_large_frame_over_64kib_roundtrips() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");
    let h = thread::spawn(move || {
        TlsTransport::accept_stream(Box::new(s_srv), srv_cfg).expect("server handshake")
    });
    let cli =
        TlsTransport::connect_stream(Box::new(s_cli), "localhost", client_config_trusting(CA))
            .expect("client handshake");
    let srv = Arc::new(h.join().unwrap());
    let cli = Arc::new(cli);

    // 1 MiB payload — well past the 64 KiB rustls sendable-buffer limit.
    let payload: Vec<u8> = (0..(1usize << 20)).map(|i| (i % 251) as u8).collect();
    let srv_s = Arc::clone(&srv);
    let sent = payload.clone();
    let sender = thread::spawn(move || {
        srv_s.send_frame(&sent).expect("srv send large frame");
    });
    let got = cli.recv_frame().expect("cli recv large frame");
    sender.join().unwrap();
    assert_eq!(got, payload, "1 MiB frame must round-trip over TLS");
}

/// Queued frames all arrive although one pump decrypts past rustls's 16 KiB plaintext cap.
#[test]
fn tls_queued_frames_past_the_plaintext_limit_all_arrive() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");
    let h = thread::spawn(move || {
        TlsTransport::accept_stream(Box::new(s_srv), srv_cfg).expect("server handshake")
    });
    let cli =
        TlsTransport::connect_stream(Box::new(s_cli), "localhost", client_config_trusting(CA))
            .expect("client handshake");
    let srv = h.join().unwrap();

    let sizes = [16_400usize, 4_096, 16_384, 1, 40_000, 8_192, 16_383];
    let frames: Vec<Vec<u8>> = sizes
        .iter()
        .enumerate()
        .map(|(i, &n)| vec![i as u8 + 1; n])
        .collect();
    // Queued before the first read; a thread, so a full socket buffer cannot hang the test.
    let sent = frames.clone();
    let sender = thread::spawn(move || {
        for f in &sent {
            srv.send_frame(f).expect("send");
        }
        srv
    });
    thread::sleep(Duration::from_millis(100));
    for (i, f) in frames.iter().enumerate() {
        let got = cli
            .recv_frame()
            .unwrap_or_else(|e| panic!("frame {i}: {e:?}"));
        assert_eq!(&got, f, "frame {i}");
    }
    drop(sender.join().unwrap());
}

/// `TlsTransport` keeps the fd-rejecting trait default (as AOSP: `Unix` fd mode excludes TLS).
#[test]
fn tls_rejects_fd_passing_by_type() {
    use std::os::fd::AsFd;

    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");
    let server = thread::spawn(move || {
        let _ = TlsTransport::accept_stream(Box::new(s_srv), srv_cfg);
        std::thread::sleep(std::time::Duration::from_millis(200));
    });
    let t = TlsTransport::connect_stream(Box::new(s_cli), "localhost", client_config_trusting(CA))
        .expect("client handshake");

    let stdin = std::io::stdin();
    let fd = stdin.as_fd();
    assert!(
        matches!(
            t.send_frame_with_fds(b"x", &[fd]),
            Err(RpcError::Protocol(_))
        ),
        "TLS must reject framed fd-passing by type"
    );
    assert!(
        matches!(t.send_raw_with_fds(b"x", &[fd]), Err(RpcError::Protocol(_))),
        "TLS must reject raw fd-passing by type"
    );
    drop(t);
    let _ = server.join();
}

/// A server cert not signed by the client's CA fails the handshake: no session, zero RPC bytes.
#[test]
fn tls_untrusted_cert_rejected_at_handshake() {
    // Server presents a self-signed rogue cert; client only trusts CA.
    let rogue_cfg = server_config(ROGUE_CRT, ROGUE_KEY);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        if let Ok((tcp, _)) = listener.accept() {
            // May fail too (the client aborts); either way no RPC layer is built.
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

/// `setup_tcp_server_tls` e2e: accept loop + worker-side handshake; module doc "Mutation gates".
#[test]
fn setup_tcp_server_tls_e2e() {
    use rsbinder::rpc::RpcServer;

    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let server =
        RpcServer::setup_tcp_server_tls("127.0.0.1:0", srv_cfg).expect("setup_tcp_server_tls");
    server
        .set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))))
        .expect("set_root");
    let addr = server.tcp_address().expect("tcp_address");
    // Accessor gates: tcp_address Some, path None.
    assert!(server.path().is_none(), "TCP server has no fs path");
    let bg = server.run_background();

    // One-call client: TCP connect → TLS handshake → R34 session.
    let client = RpcSession::setup_tcp_client_tls(addr, "localhost", client_config_trusting(CA))
        .expect("setup_tcp_client_tls");
    let root = client.get_root().expect("get_root over TCP+TLS server");
    assert_eq!(ping_via(&root, "e1-tcp").unwrap(), "pong:e1-tcp");
    assert_eq!(ping_via(&root, "").unwrap(), "pong:");

    drop(root);
    drop(client);
    server.stop_accepting();
    let _ = bg.join();
}

/// vsock × TLS (AVF/Microdroid) via `setup_vsock_server_tls`; ignored: CI loads no kernel module.
#[cfg(all(feature = "rpc-vsock", target_os = "linux"))]
#[test]
#[ignore = "needs Linux vsock loopback (modprobe vsock_loopback) or a peer VM"]
fn vsock_tls_loopback_e2e() {
    use rsbinder::rpc::transport::VsockTransport;
    use rsbinder::rpc::RpcServer;
    use vsock::VMADDR_CID_LOCAL;

    // Outside `tests/rpc_vsock.rs`'s 0x5242..=0x5245, so running both cannot `EADDRINUSE`.
    const TLS_TEST_PORT: u32 = 0x52_52;

    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let server = RpcServer::setup_vsock_server_tls(VMADDR_CID_LOCAL, TLS_TEST_PORT, srv_cfg)
        .expect("setup_vsock_server_tls");
    server
        .set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))))
        .expect("set_root");
    assert_eq!(
        server.vsock_address(),
        Some((VMADDR_CID_LOCAL, TLS_TEST_PORT))
    );
    assert!(server.path().is_none(), "vsock+TLS server has no fs path");
    let bg = server.run_background();

    // Raw `VsockStream`, not `VsockTransport`: TLS runs over it (AOSP `newTransport(fd)`).
    let vsock_stream =
        vsock::VsockStream::connect(&vsock::VsockAddr::new(VMADDR_CID_LOCAL, TLS_TEST_PORT))
            .expect("client vsock connect");
    let client_t = TlsTransport::connect_stream(
        Box::new(vsock_stream),
        "localhost",
        client_config_trusting(CA),
    )
    .expect("client TLS handshake over vsock");
    // The TLS layer overrides the plain `Vsock { cid }` identity with the leaf-cert fingerprint.
    match client_t.peer_identity() {
        PeerIdentity::Certificate(c) => assert_eq!(c.fingerprint().len(), 32),
        other => panic!("expected Certificate peer id over vsock+TLS, got {other}"),
    }
    let client =
        RpcSession::new(Box::new(client_t), AddressSpace::Initiator).expect("RpcSession::new");
    let root = client.get_root().expect("get_root over vsock+TLS");
    assert_eq!(ping_via(&root, "vsock-tls").unwrap(), "pong:vsock-tls");
    assert_eq!(ping_via(&root, "").unwrap(), "pong:");

    // Type witness only: the client above builds the stream by hand to keep TLS explicit.
    let _: fn(u32, u32) -> _ = VsockTransport::connect;

    drop(root);
    drop(client);
    server.stop_accepting();
    let _ = bg.join();
}

/// `RpcServer::setup_unix_server_tls` e2e: the TLS handshake runs over a UDS listener.
#[test]
fn setup_unix_server_tls_e2e() {
    use rsbinder::rpc::RpcServer;

    let path = {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "rsb_rpc_unix_tls_{}_{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    };
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let server = RpcServer::setup_unix_server_tls(&path, srv_cfg).expect("setup_unix_server_tls");
    server
        .set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))))
        .expect("set_root");
    assert_eq!(
        server.path(),
        Some(path.as_path()),
        "UDS+TLS server still exposes its fs path"
    );
    let bg = server.run_background();
    // Wait for the socket file to appear (bounded).
    for _ in 0..400 {
        if path.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(path.exists(), "UDS server socket must appear");

    // Client: TLS over a hand-opened UDS via the socket-kind-orthogonal `connect_stream`.
    let unix_client = UnixStream::connect(&path).expect("unix connect");
    let client_t = TlsTransport::connect_stream(
        Box::new(unix_client),
        "localhost",
        client_config_trusting(CA),
    )
    .expect("client TLS handshake over UDS+TLS server");
    let client =
        RpcSession::new(Box::new(client_t), AddressSpace::Initiator).expect("RpcSession::new");
    let root = client.get_root().expect("get_root over UDS+TLS");
    assert_eq!(ping_via(&root, "e1-uds").unwrap(), "pong:e1-uds");

    drop(root);
    drop(client);
    server.stop_accepting();
    let _ = bg.join();
}

// ---- incoming (callback) connections over TLS ----------------------

const HOLDER_DESC: &str = "rsbinder.test.IHolder";
const TX_HOLD: TransactionCode = FIRST_CALL_TRANSACTION;

/// Keeps the last binder a client hands it, for the test to call from outside any handler.
struct Holder(Arc<Mutex<Option<SIBinder>>>);
impl Interface for Holder {}
impl Remotable for Holder {
    fn descriptor() -> &'static str {
        HOLDER_DESC
    }
    fn on_transact(&self, code: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        match code {
            TX_HOLD => {
                *self.0.lock().unwrap() = Some(r.read()?);
                reply.write(&Status::from(StatusCode::Ok))
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn hold_via(holder: &SIBinder, cb: &SIBinder) -> Result<()> {
    let rp = (**holder)
        .as_any()
        .downcast_ref::<rsbinder::rpc::RpcProxy>()
        .expect("RpcProxy");
    let mut d = rp.build_request(HOLDER_DESC)?;
    d.write(cb)?;
    let mut r = rp
        .transact(TX_HOLD, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    let st: Status = r.read()?;
    if st.is_ok() {
        Ok(())
    } else {
        Err(StatusCode::from(st))
    }
}

/// Counts its calls, so a oneway one can be seen landing.
struct CountingPing(Arc<AtomicUsize>);
impl Interface for CountingPing {}
impl IPing for CountingPing {
    fn ping(&self, s: &str) -> Result<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(format!("pong:{s}"))
    }
}

fn counting_callback() -> (SIBinder, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let cb = Interface::as_binder(&Binder::new(BnPing(Box::new(CountingPing(Arc::clone(
        &calls,
    ))))));
    (cb, calls)
}

/// Call the held callback from a fresh thread, outside any handler: once twoway, once oneway.
fn call_back_from_outside(held: &Mutex<Option<SIBinder>>) -> (Result<String>, Result<()>) {
    let cb = held.lock().unwrap().take().expect("a held callback");
    thread::spawn(move || {
        let twoway = ping_via(&cb, "outside");
        let oneway = (|| {
            let rp = (*cb)
                .as_any()
                .downcast_ref::<rsbinder::rpc::RpcProxy>()
                .expect("RpcProxy");
            let mut d = rp.build_request(DESC)?;
            d.write(&"oneway")?;
            rp.transact(TX_PING, &d, rsbinder::FLAG_ONEWAY).map(|_| ())
        })();
        (twoway, oneway)
    })
    .join()
    .expect("caller thread")
}

fn poll_until(mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    f()
}

/// Off-thread under a deadline: an incoming thread `close_session` fails to wake would hang.
fn close_within(session: &RpcSession, what: &str) {
    let s = session.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        s.close_session();
        let _ = tx.send(s.__incoming_thread_live_count());
    });
    let live = rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| panic!("{what}: close_session did not return"));
    assert_eq!(live, 0, "{what}: incoming threads still running");
}

/// `incoming_connections` on `tls://` carry out-of-handler callbacks; without one, `WouldBlock`.
#[test]
fn entry_tls_incoming_connections_carry_callbacks() {
    use rsbinder::TransportCaps;

    let held = Arc::new(Mutex::new(None));
    let guard = rsbinder::serve("tls://127.0.0.1:0?profile=android13plus")
        .expect("serve tls://")
        .with(|o| {
            o.tls = Some(server_config(SRV_CRT, SRV_KEY));
            o.threads = Some(2);
        })
        .add(
            "holder",
            Interface::as_binder(&Binder::new(Holder(Arc::clone(&held)))),
        )
        .expect("add")
        .spawn()
        .expect("spawn");
    let addr = guard
        .server()
        .expect("rpc server")
        .tcp_address()
        .expect("bound TCP address");
    let open = |incoming: Option<u32>, outgoing: Option<u32>| {
        rsbinder::Client::open_with(&format!("tls://{addr}?profile=android13plus"), |o, _| {
            o.tls = Some(client_config_trusting(CA));
            o.tls_server_name = Some("localhost".to_string());
            o.incoming_connections = incoming;
            o.outgoing_connections = outgoing;
            o.handshake_timeout = Some(Duration::from_secs(5));
        })
    };

    // Control: no incoming connection, so nothing for the server to send on.
    {
        let client = open(None, None).expect("open tls://");
        assert!(!client.caps().contains(TransportCaps::CALLBACKS));
        let (cb, calls) = counting_callback();
        hold_via(&client.binder("holder").expect("holder"), &cb).expect("hold");
        let (twoway, oneway) = call_back_from_outside(&held);
        assert_eq!(twoway, Err(StatusCode::WouldBlock));
        assert_eq!(oneway, Err(StatusCode::WouldBlock));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    let client = open(Some(1), Some(2)).expect("open tls:// with incoming connections");
    let session = client.session().expect("rpc session").clone();
    // Founding + one fan-out (the server allows two) + one incoming.
    let slots = session.__slot_count();
    let threads = session.__incoming_thread_count();
    let caps = client.caps();
    if slots != 3 || threads != 1 || !caps.contains(TransportCaps::CALLBACKS) {
        close_within(&session, "shape check");
        panic!("slots={slots} incoming threads={threads} caps={caps}");
    }

    let (cb, calls) = counting_callback();
    hold_via(&client.binder("holder").expect("holder"), &cb).expect("hold");
    let (twoway, oneway) = call_back_from_outside(&held);
    let landed = poll_until(|| calls.load(Ordering::SeqCst) == 2);
    close_within(&session, "tls://");
    assert_eq!(twoway.as_deref(), Ok("pong:outside"));
    assert_eq!(oneway, Ok(()));
    assert!(landed, "the oneway callback never reached the client");
}

/// `RpcClientConfig` over a transport the entry does not name (TLS on Unix), one `connect` each.
#[test]
fn client_config_opens_incoming_connections_over_tls_on_unix() {
    let path = std::env::temp_dir().join(format!("rsb_tls_in_{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let server =
        rsbinder::rpc::RpcServer::setup_unix_server_tls(&path, server_config(SRV_CRT, SRV_KEY))
            .expect("setup_unix_server_tls");
    server.set_android13plus(2);
    let held = Arc::new(Mutex::new(None));
    server
        .set_root(Interface::as_binder(&Binder::new(Holder(Arc::clone(
            &held,
        )))))
        .expect("set_root");
    let bg = server.run_background();

    let connects = AtomicUsize::new(0);
    let session = RpcSession::setup_client_android13plus_with_config(
        rsbinder::rpc::RpcClientConfig::new(2, || {
            connects.fetch_add(1, Ordering::SeqCst);
            let unix = UnixStream::connect(&path)?;
            let t = TlsTransport::connect_stream(
                Box::new(unix),
                "localhost",
                client_config_trusting(CA),
            )?;
            Ok(Box::new(t) as Box<dyn RpcTransport>)
        })
        .incoming_connections(1)
        .handshake_timeout(Duration::from_secs(5)),
    )
    .expect("setup over TLS-on-unix with an incoming connection");

    let (cb, calls) = counting_callback();
    let root = session.get_root().expect("root");
    hold_via(&root, &cb).expect("hold");
    let (twoway, oneway) = call_back_from_outside(&held);
    let landed = poll_until(|| calls.load(Ordering::SeqCst) == 2);
    close_within(&session, "tls over unix");
    drop(root);
    server.stop_accepting();
    let _ = bg.join();
    let _ = std::fs::remove_file(&path);

    assert_eq!(connects.load(Ordering::SeqCst), 2, "founding + incoming");
    assert_eq!(twoway.as_deref(), Ok("pong:outside"));
    assert_eq!(oneway, Ok(()));
    assert!(landed, "the oneway callback never reached the client");
}

/// For the two `RpcClientConfig::tls` cases below: android-13+, two threads, a `PingSvc` root.
fn tls_ping_server() -> (Arc<rsbinder::rpc::RpcServer>, std::net::SocketAddr) {
    let server = rsbinder::rpc::RpcServer::setup_tcp_server_tls(
        "127.0.0.1:0",
        server_config(SRV_CRT, SRV_KEY),
    )
    .expect("setup_tcp_server_tls");
    server.set_android13plus(2);
    server.set_max_threads(2);
    server
        .set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))))
        .expect("set_root");
    let addr = server.tcp_address().expect("tcp_address");
    (server, addr)
}

/// Plan 10-7 Phase 0b: the `tls` constructor drives the entry layer's setup, fan-out included.
#[test]
fn client_config_tls_constructor_opens_a_fan_out_session() {
    use rsbinder::rpc::RpcClientConfig;

    let (server, addr) = tls_ping_server();
    let bg = server.run_background();
    let host = addr.ip().to_string();
    let session = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::tls(
            &host,
            addr.port(),
            "localhost",
            client_config_trusting(CA),
            2,
        )
        .outgoing_connections(2)
        .handshake_timeout(Duration::from_secs(5)),
    )
    .expect("tls constructor");
    assert_eq!(session.negotiated_max_threads(), 2);
    assert_eq!(session.__slot_count(), 2, "founding + one fan-out");
    let root = session.get_root().expect("get_root");
    assert_eq!(ping_via(&root, "cfg-tls").unwrap(), "pong:cfg-tls");

    drop(root);
    close_within(&session, "tls constructor");
    server.stop_accepting();
    let _ = bg.join();
}

/// Unix fd mode needs a Unix socket; `RpcClientConfig::tls` refuses it on the founding connection.
#[test]
fn fd_mode_unix_over_tls_is_refused_at_setup() {
    use rsbinder::rpc::{FileDescriptorTransportMode, RpcClientConfig};

    let (server, addr) = tls_ping_server();
    server.set_supported_fd_modes(&[FileDescriptorTransportMode::Unix]);
    let bg = server.run_background();
    let host = addr.ip().to_string();
    let refused = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::tls(
            &host,
            addr.port(),
            "localhost",
            client_config_trusting(CA),
            2,
        )
        .fd_mode(FileDescriptorTransportMode::Unix)
        .handshake_timeout(Duration::from_secs(5)),
    );
    assert_eq!(refused.err(), Some(StatusCode::BadValue));

    server.stop_accepting();
    let _ = bg.join();
}
