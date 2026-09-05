// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The TLS backend, exercised with the **core
//! unchanged** — only the transport is swapped. Covers a valid cert
//! handshake + AIDL round-trip + `Certificate` peer-id, an untrusted
//! cert → handshake reject (**zero RPC payload**), and the absence of
//! any plaintext network backend (enforced by type/absence, noted
//! here).
//!
//! Separate test binary; `#![cfg(feature = "rpc-tls")]` so it only
//! builds/runs with the feature (default test runs don't pay rustls).

#![cfg(feature = "rpc-tls")]

use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;

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

/// Valid cert → handshake + the unchanged
/// AIDL e2e over TLS; both ends see a `Certificate` peer identity.
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
        session.set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))));
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
    drop(root);
    drop(client);
    server.join().unwrap();
}

/// TLS is decoupled from TCP — the same handshake +
/// unchanged AIDL e2e runs over a **`UnixStream`** via
/// [`TlsTransport::connect_stream`]/[`TlsTransport::accept_stream`]
/// (not just `TcpStream`). Mirrors AOSP's socket-kind-orthogonal
/// `RpcTransportCtx::newTransport(fd)`.
#[test]
fn tls_over_unix_socket_e2e() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");

    let server = thread::spawn(move || {
        let t = TlsTransport::accept_stream(Box::new(s_srv), srv_cfg)
            .expect("server TLS handshake over unix");
        let session =
            RpcSession::new(Box::new(t), AddressSpace::Acceptor).expect("RpcSession::new");
        session.set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))));
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

/// A TCP end without a TLS `close_notify` is not a clean close. On the
/// one backend built for untrusted networks that is what a truncation
/// attack looks like, so the transport reports it as
/// [`RpcError::UncleanEndOfStream`] — not the `EndOfStream` a
/// `close_notify` yields — even at a frame boundary, where the plain
/// framing reader would otherwise see a clean end of stream.
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

/// The transport's own `shutdown()` sends `close_notify` before the
/// socket shutdown, so a deliberate local close is the clean end on the
/// peer — the unclean report above is reserved for a stream that was cut.
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

/// Plan 2-21 D-3 — our own `shutdown()` wakes our own reader with an end
/// of stream that carries no `close_notify` from the peer. That is the
/// shape of a cut, but it is ours: the transport reports the clean
/// `EndOfStream`, not `UncleanEndOfStream`, so a session this end shut
/// down ends its serve loop cleanly. A second `shutdown()` is `Ok`.
#[test]
fn tls_local_shutdown_is_a_clean_end_for_our_own_reader() {
    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let t = TlsTransport::accept_stream(Box::new(s_srv), srv_cfg).expect("server handshake");
        // One frame proves the server's handshake I/O is over before the
        // client shuts down — a client `SHUT_RD` while the server still has
        // a post-handshake write pending fails that write with `EPIPE` on
        // Linux. Then the server holds its end until the client has looked,
        // so the only end the client sees is the one it made itself.
        t.send_frame(b"hello").expect("send after handshake");
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

/// A `shutdown()` racing this end's own in-flight send: the peer reads
/// every frame that send was told went out, then a clean end.
///
/// Two promises, both of which a shutdown that simply cut the socket
/// broke. The peer's: `UncleanEndOfStream` is reserved for a stream
/// somebody cut, so a deliberate close owes it `close_notify` — and only
/// the thread holding `wlock` may put that on the wire, so `shutdown` has
/// to wait for the send in flight rather than strand the alert. The
/// sender's: `Ok` means the frame went out. A send that started after
/// `shutdown` is refused, and the alert is queued only once the lock is
/// held, so no frame can be encrypted behind it — where the peer, having
/// read the alert, would discard it (RFC 8446 §6.1) after the sender was
/// told `Ok`. The wait is bounded, so a peer that has stopped reading
/// still cannot hold teardown.
///
/// The window is a few instructions wide, so this races it repeatedly
/// rather than pinning one interleaving, and counts on both ends: every
/// `Ok` the sender saw is a frame the peer received, and the end the peer
/// reads is never an unclean one. A cut *mid-frame* would read as
/// `Truncated`, and the frame it cut was reported failed, so the counts
/// still agree — the assertion tolerates it without naming it.
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
            // Small frames so a send never parks in the socket write and
            // the boundary between two of them comes round often — that
            // boundary is what the shutdown has to hit. Count what this
            // end was told went out.
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
        // Walk the delay across the rounds so the shutdown lands at a
        // different point of the send loop each time.
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

/// The one-call TCP+TLS client
/// constructor `RpcSession::setup_tcp_client_tls` — TCP-connect + TLS
/// handshake + R34 session — interoperates with a TLS server end to end.
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
        session.set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
            PingSvc,
        )))));
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

/// Plan 2-17: the entry layer over `tls://` — `serve` binds the TLS
/// listener with `ServeOptions::tls`, `Client::open_with` supplies the
/// client config plus a `tls_server_name` that differs from the URI host
/// (cert is for `localhost`, the socket is `127.0.0.1`). The `:0` port is
/// resolved through the guard's `server()` escape hatch.
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
        // The fixture cert is issued for `localhost`; the URI host is the
        // dialed address. `tls_server_name` decouples the two.
        o.tls_server_name = Some("localhost".to_string());
    })
    .expect("Client::open_with tls://");

    let root = client.binder("ping").expect("lookup ping");
    assert_eq!(ping_via(&root, "entry").unwrap(), "pong:entry");
    drop(root);
    drop(client);
}

/// `tls://` without a config is refused on **both** sides before any
/// socket work — the URI cannot carry trust anchors or a server identity,
/// so the option is mandatory. The server half has no compile-time signal
/// at all (`serve(..).add(..).spawn()` looks complete on its own), which
/// makes it the likelier mistake.
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

/// `fd_modes` / `fd_mode` may only advertise Unix fd passing where the
/// transport can actually carry `SCM_RIGHTS`; TLS cannot, so both sides
/// reject it instead of agreeing a mode that fails later on the wire.
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

/// Concurrency gate: a single `TlsTransport` must
/// support a sender thread and a receiver thread **concurrently**
/// ([`RpcTransport`] contract). The decomposed `Mutex<Connection>` +
/// `try_lock` write-path makes this lock-free-duplex
/// safe — a single-`Mutex<TlsIo>` would deadlock here (a
/// blocked `recv` held the lock the `send` needed). Drives full
/// bidirectional traffic on both ends and checks FIFO integrity.
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

    // Each end runs a sender thread AND a receiver thread on the SAME
    // transport object at once — the concurrent send+recv the contract
    // requires. One sender per direction ⇒ FIFO, so content is checkable.
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

/// A frame larger than rustls's ~64 KiB plaintext sendable buffer must still
/// round-trip: `send_raw` chunks the plaintext and interleaves encrypt-drain
/// (a single unchunked `writer().write_all` of such a payload fails with
/// `WriteZero`).
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

/// TLS rejects out-of-band file descriptors *by type*
/// — `SCM_RIGHTS` cannot ride an encrypted byte stream, so `TlsTransport`
/// keeps the trait's rejecting default for `send_*_with_fds` (no
/// override). Matches AOSP, where `FileDescriptorTransportMode::Unix` is
/// incompatible with TLS.
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

/// Security: a server cert NOT signed by the
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

/// Documentation gate: there is **no** plaintext-network
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

/// TCP+TLS server e2e via `RpcServer::setup_tcp_server_tls`.
/// The server is a real `RpcServer`:
/// accept loop, worker-thread TLS handshake (so a slow-handshake peer
/// never stalls accept), authorizer hook (post-handshake peer-id), and
/// the rest of the `RpcServer` knob set all work unchanged.
///
/// **Mutant gate**: dropping the `*server.tls_config.lock() =
/// Some(config)` line in `setup_tcp_server_tls` (or returning `None`
/// from `tls_snapshot()`) leaves the worker wrapping the accepted
/// TcpStream with the plain branch — `RawAccepted::Tcp(_)` then hits
/// the "plain-text TCP server is not exposed" error, the worker exits
/// without serving, the client's TLS handshake times out / errors.
#[test]
fn setup_tcp_server_tls_e2e() {
    use rsbinder::rpc::RpcServer;

    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let server =
        RpcServer::setup_tcp_server_tls("127.0.0.1:0", srv_cfg).expect("setup_tcp_server_tls");
    server.set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
        PingSvc,
    )))));
    let addr = server.tcp_address().expect("tcp_address");
    // Accessor gates: tcp_address Some, path None.
    assert!(server.path().is_none(), "TCP server has no fs path");
    let bg = server.run_background();

    // Use the matching one-call client convenience constructor — the
    // helper itself goes through `setup_tcp_client_tls`'s normal
    // TCP-connect → TLS-handshake → R34 session path.
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

/// vsock × TLS hermetic e2e, server built
/// via `RpcServer::setup_vsock_server_tls`, client TLS-wraps a raw
/// vsock stream with `TlsTransport::connect_stream`. The 1st-class
/// Android AVF / Microdroid pVM scenario (vsock socket plane + TLS
/// crypto plane), now end-to-end through the public `RpcServer` API.
///
/// **Environment gate** — `#[ignore]` so default `cargo test`
/// (CI / macOS) never runs it. Loopback vsock requires the Linux
/// `vsock_loopback` kernel module (`sudo modprobe vsock_loopback` on a
/// host with no peer VM); a peer-VM environment skips the modprobe
/// step. CI does not load kernel modules, so this is hermetic-by-
/// `#[ignore]`; the canonical verification surface is the REMOTE_LINUX
/// box per memory.
///
/// **Compile gate** — `target_os = "linux"` plus the `rpc-vsock` and
/// `rpc-tls` features. macOS host compiles this file out of this test
/// (the `rpc-tls` `#![cfg]` at the top of the file keeps the build
/// graph clean elsewhere).
#[cfg(all(feature = "rpc-vsock", target_os = "linux"))]
#[test]
#[ignore = "needs Linux vsock loopback (modprobe vsock_loopback) or a peer VM"]
fn vsock_tls_loopback_e2e() {
    use rsbinder::rpc::transport::VsockTransport;
    use rsbinder::rpc::RpcServer;
    use vsock::VMADDR_CID_LOCAL;

    // Arbitrary unused port; mirrors `tests/rpc_vsock.rs` choice +
    // bumped one digit so a left-behind no-TLS server (the other test)
    // doesn't `EADDRINUSE` this one in a back-to-back run.
    const TLS_TEST_PORT: u32 = 0x52_43;

    let srv_cfg = server_config(SRV_CRT, SRV_KEY);
    let server = RpcServer::setup_vsock_server_tls(VMADDR_CID_LOCAL, TLS_TEST_PORT, srv_cfg)
        .expect("setup_vsock_server_tls");
    server.set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
        PingSvc,
    )))));
    assert_eq!(
        server.vsock_address(),
        Some((VMADDR_CID_LOCAL, TLS_TEST_PORT))
    );
    assert!(server.path().is_none(), "vsock+TLS server has no fs path");
    let bg = server.run_background();

    // Client: vsock connect → TLS handshake over the raw vsock stream.
    // No client-side `setup_vsock_client_tls` convenience function yet
    // (a separate small follow-up); the underlying composition is one
    // line per AOSP `RpcTransportCtx::newTransport(fd)` (socket-kind-
    // orthogonal TLS).
    //
    // `VsockTransport::connect` returns an `RpcTransport` already, but
    // here we need the raw `VsockStream` so the TLS handshake runs
    // *over* it (not as the framing transport itself). Build the
    // stream by hand — `vsock::VsockStream::connect` matches the
    // existing `VsockTransport::connect` body.
    let vsock_stream =
        vsock::VsockStream::connect(&vsock::VsockAddr::new(VMADDR_CID_LOCAL, TLS_TEST_PORT))
            .expect("client vsock connect");
    let client_t = TlsTransport::connect_stream(
        Box::new(vsock_stream),
        "localhost",
        client_config_trusting(CA),
    )
    .expect("client TLS handshake over vsock");
    // Over vsock+TLS: peer identity is the leaf-cert fingerprint
    // (not `Vsock { cid }` — that's the `VsockTransport` plain identity,
    // here the TLS layer overrides).
    match client_t.peer_identity() {
        PeerIdentity::Certificate(c) => assert_eq!(c.fingerprint().len(), 32),
        other => panic!("expected Certificate peer id over vsock+TLS, got {other}"),
    }
    let client =
        RpcSession::new(Box::new(client_t), AddressSpace::Initiator).expect("RpcSession::new");
    let root = client.get_root().expect("get_root over vsock+TLS");
    assert_eq!(ping_via(&root, "vsock-tls").unwrap(), "pong:vsock-tls");
    assert_eq!(ping_via(&root, "").unwrap(), "pong:");

    // Silence the `VsockTransport` import on Linux builds where it's
    // not referenced (we only use it as a type-witness that the vsock
    // client transport exists; the actual client builds the stream by
    // hand to keep the TLS wrap explicit).
    let _: fn(u32, u32) -> _ = VsockTransport::connect;

    drop(root);
    drop(client);
    server.stop_accepting();
    let _ = bg.join();
}

/// UDS+TLS server e2e via `RpcServer::setup_unix_server_tls`.
/// TLS is socket-kind-orthogonal (AOSP `RpcTransportCtx::newTransport(fd)`);
/// the same TLS handshake runs over a UnixStream once `RpcServer` no
/// longer pins the listener to UDS-without-TLS. Demonstrates the
/// listener generalization carrying the TLS path through it.
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
    server.set_root(Interface::as_binder(&Binder::new(BnPing(Box::new(
        PingSvc,
    )))));
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

    // Client: open the UDS by hand and run the TLS handshake on it
    // via `TlsTransport::connect_stream` (the socket-kind-orthogonal
    // client API). The R34 wire then carries the AIDL e2e.
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
