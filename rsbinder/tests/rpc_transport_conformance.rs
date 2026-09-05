// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-21 D-7 — what every in-tree transport's `shutdown` does, pinned
//! per backend **and per platform**.
//!
//! The contract on `RpcTransport::shutdown` says that what happens to
//! bytes already received is platform- and backend-dependent and must not
//! be assumed. This suite is what keeps that sentence true rather than
//! merely written: it asserts the measured behaviour, so the next
//! refactor that quietly relies on a queued frame vanishing (or
//! surviving) fails here, on whichever platform it would have been wrong.
//!
//! | backend | queued frame, then local shutdown | blocked reader | peer after our shutdown | 2nd shutdown |
//! |---|---|---|---|---|
//! | unix, unix fd-mode, tls (over unix) | **macOS:** end of stream (queue dropped) · **Linux:** the frame, then end of stream | end of stream | reads end of stream; its first send **fails at once on Linux (`EPIPE`), is accepted on macOS** | `Ok` |
//! | tcp_debug | as above | end of stream | reads end of stream; its first send is **accepted on both** — TCP reports the reset on a later write | `Ok` |
//! | mem | the frame, then end of stream (models Linux unix) | end of stream (≤ one 20 ms tick) | reads end of stream; its sends fail at once | `Ok` |
//!
//! `vsock` is not here: it needs a VM peer (its own tests are `#[ignore]`),
//! and its behaviour is inferred from `vsock(7)`, not measured.
//!
//! This suite's first run found a deadlock: `UnixTransport::shutdown` took
//! the fd-mode leftover lock before shutting the socket down, and a reader
//! parked in `recvmsg` holds that lock until the socket wakes it. Every
//! `shutdown` here runs under a deadline so the next such bug fails
//! instead of hanging the runner.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`.

#![cfg(feature = "rpc")]

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{RpcError, RpcTransport};

type Shared = Arc<dyn RpcTransport>;

/// What a reader finds after a local shutdown when a frame had already
/// reached the local kernel queue (or channel).
#[derive(Clone, Copy)]
enum Queued {
    /// Delivered first, then the end of stream (Linux; `mem`).
    Delivered,
    /// Gone: the very next read is the end of stream (macOS).
    Dropped,
}

/// The socket backends follow the kernel; `mem` follows Linux by design.
fn socket_expectation() -> Queued {
    if cfg!(target_os = "macos") {
        Queued::Dropped
    } else {
        Queued::Delivered
    }
}

/// What the peer's *first* send after our shutdown does — measured, and
/// three different answers.
#[derive(Clone, Copy)]
enum PeerSend {
    /// Refused at once (`EPIPE`): Linux `AF_UNIX`, and `mem` by design.
    FailsAtOnce,
    /// Accepted (and discarded, or reset on a later write): macOS
    /// `AF_UNIX`, and TCP on both platforms.
    Accepted,
}

fn unix_peer_send() -> PeerSend {
    if cfg!(target_os = "linux") {
        PeerSend::FailsAtOnce
    } else {
        PeerSend::Accepted
    }
}

/// `shutdown` on its own thread under a deadline: a shutdown that blocks
/// (this suite found one — a lock held by a parked reader) must fail the
/// test, not hang the runner.
fn shutdown_within(name: &str, t: &Shared) {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), RpcError>>(1);
    let t = Arc::clone(t);
    let h = std::thread::spawn(move || {
        let _ = tx.send(t.shutdown());
    });
    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("{name}: shutdown failed: {e}"),
        Err(RecvTimeoutError::Timeout) => panic!("{name}: shutdown blocked"),
        Err(RecvTimeoutError::Disconnected) => panic!("{name}: shutdown thread died"),
    }
    h.join().expect("shutdown thread");
}

fn recv(t: &dyn RpcTransport, fd_mode: bool) -> Result<Vec<u8>, RpcError> {
    if fd_mode {
        t.recv_frame_with_fds().map(|(frame, _fds)| frame)
    } else {
        t.recv_frame()
    }
}

/// Scenario A (queued frame), C (idempotence) and D (the peer's view).
fn queued_then_shutdown(
    name: &str,
    local: &Shared,
    peer: &Shared,
    fd_mode: bool,
    queued: Queued,
    peer_send: PeerSend,
) {
    peer.send_frame(b"queued")
        .expect("peer sends into our queue");
    shutdown_within(name, local);
    if let Queued::Delivered = queued {
        assert_eq!(
            recv(&**local, fd_mode).expect("the queued frame is still delivered"),
            b"queued",
            "{name}: a frame queued before shutdown is delivered on this platform"
        );
    }
    let end = recv(&**local, fd_mode);
    assert!(
        matches!(end, Err(RpcError::EndOfStream)),
        "{name}: after shutdown (and any queued frame) the read is the end of stream, got {end:?}"
    );
    // A second shutdown is Ok (idempotent).
    shutdown_within(name, local);
    // The peer: our shutdown is its end of stream.
    let peer_end = peer.recv_frame();
    assert!(
        matches!(peer_end, Err(RpcError::EndOfStream)),
        "{name}: the peer reads our shutdown as its end of stream, got {peer_end:?}"
    );
    // Its first send afterwards is measured per backend and platform (the
    // table in the module doc); if it changes, the table and the
    // `shutdown` contract change with it.
    let sent = peer.send_frame(b"x");
    match peer_send {
        PeerSend::FailsAtOnce => assert!(
            sent.is_err(),
            "{name}: the peer's first send after our shutdown fails at once here, got {sent:?}"
        ),
        PeerSend::Accepted => assert!(
            sent.is_ok(),
            "{name}: the peer's first send after our shutdown is accepted here, got {sent:?}"
        ),
    }
}

/// Scenario B: a reader parked in `recv` is woken by a local shutdown and
/// returns the end of stream — within a bound, on every backend.
fn blocked_reader_is_woken(name: &str, local: &Shared, fd_mode: bool) {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, RpcError>>(1);
    let reader = {
        let local = Arc::clone(local);
        std::thread::spawn(move || {
            let _ = tx.send(recv(&*local, fd_mode));
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    shutdown_within(name, local);
    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Err(RpcError::EndOfStream)) => {}
        Ok(other) => panic!("{name}: the woken reader must see the end of stream, got {other:?}"),
        Err(RecvTimeoutError::Timeout) => panic!("{name}: shutdown did not wake the parked reader"),
        Err(RecvTimeoutError::Disconnected) => panic!("{name}: the reader thread died"),
    }
    reader.join().expect("reader thread");
}

fn unix_pair() -> (Shared, Shared) {
    let (a, b) = UnixTransport::pair().expect("socketpair");
    (Arc::new(a), Arc::new(b))
}

fn mem_pair() -> (Shared, Shared) {
    let (a, b) = MemTransport::pair();
    (Arc::new(a), Arc::new(b))
}

#[test]
fn unix_queued_frame_then_shutdown() {
    let (local, peer) = unix_pair();
    queued_then_shutdown(
        "unix",
        &local,
        &peer,
        false,
        socket_expectation(),
        unix_peer_send(),
    );
}

#[test]
fn unix_blocked_reader_is_woken() {
    let (local, _peer) = unix_pair();
    blocked_reader_is_woken("unix", &local, false);
}

#[test]
fn unix_fd_mode_queued_frame_then_shutdown() {
    let (local, peer) = unix_pair();
    queued_then_shutdown(
        "unix fd-mode",
        &local,
        &peer,
        true,
        socket_expectation(),
        unix_peer_send(),
    );
}

#[test]
fn unix_fd_mode_blocked_reader_is_woken() {
    let (local, _peer) = unix_pair();
    blocked_reader_is_woken("unix fd-mode", &local, true);
}

#[test]
fn mem_queued_frame_then_shutdown() {
    let (local, peer) = mem_pair();
    queued_then_shutdown(
        "mem",
        &local,
        &peer,
        false,
        Queued::Delivered,
        PeerSend::FailsAtOnce,
    );
}

#[test]
fn mem_blocked_reader_is_woken() {
    let (local, _peer) = mem_pair();
    blocked_reader_is_woken("mem", &local, false);
}

#[cfg(feature = "rpc-tcp-debug")]
mod tcp {
    use super::*;
    use rsbinder::rpc::transport::TcpDebugTransport;

    fn tcp_pair() -> (Shared, Shared) {
        let (client, server) = TcpDebugTransport::pair_loopback().expect("loopback pair");
        (Arc::new(client), Arc::new(server))
    }

    #[test]
    fn tcp_debug_queued_frame_then_shutdown() {
        let (local, peer) = tcp_pair();
        // TCP accepts the first write on both platforms and reports the
        // reset on a later one.
        queued_then_shutdown(
            "tcp_debug",
            &local,
            &peer,
            false,
            socket_expectation(),
            PeerSend::Accepted,
        );
    }

    #[test]
    fn tcp_debug_blocked_reader_is_woken() {
        let (local, _peer) = tcp_pair();
        blocked_reader_is_woken("tcp_debug", &local, false);
    }
}

#[cfg(feature = "rpc-tls")]
mod tls {
    use super::*;
    use rsbinder::rpc::rustls::pki_types::pem::PemObject;
    use rsbinder::rpc::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rsbinder::rpc::rustls::{ClientConfig, RootCertStore, ServerConfig};
    use rsbinder::rpc::transport::TlsTransport;
    use std::os::unix::net::UnixStream;

    const CA: &str = include_str!("tls_fixtures/ca.crt");
    const SRV_CRT: &str = include_str!("tls_fixtures/srv.crt");
    const SRV_KEY: &str = include_str!("tls_fixtures/srv.key");

    fn certs(pem: &str) -> Vec<CertificateDer<'static>> {
        CertificateDer::pem_slice_iter(pem.as_bytes())
            .collect::<Result<_, _>>()
            .expect("parse certs")
    }

    /// Both handshakes complete before the pair is handed out, so no
    /// post-handshake write is still pending when a test shuts an end down.
    fn tls_pair() -> (Shared, Shared) {
        let (s_srv, s_cli) = UnixStream::pair().expect("unix socketpair");
        let srv_cfg = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    certs(SRV_CRT),
                    PrivateKeyDer::from_pem_slice(SRV_KEY.as_bytes()).expect("key"),
                )
                .expect("server config"),
        );
        let server = std::thread::spawn(move || {
            TlsTransport::accept_stream(Box::new(s_srv), srv_cfg).expect("server handshake")
        });
        let mut roots = RootCertStore::empty();
        for c in certs(CA) {
            roots.add(c).expect("add ca");
        }
        let cli_cfg = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let client = TlsTransport::connect_stream(Box::new(s_cli), "localhost", cli_cfg)
            .expect("client handshake");
        let server = server.join().expect("server thread");
        (Arc::new(client), Arc::new(server))
    }

    #[test]
    fn tls_queued_frame_then_shutdown() {
        let (local, peer) = tls_pair();
        // Over a unix socketpair here, so the peer's send follows AF_UNIX.
        queued_then_shutdown(
            "tls",
            &local,
            &peer,
            false,
            socket_expectation(),
            unix_peer_send(),
        );
    }

    #[test]
    fn tls_blocked_reader_is_woken() {
        let (local, _peer) = tls_pair();
        blocked_reader_is_woken("tls", &local, false);
    }
}
