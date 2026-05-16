// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! TLS transport over **rustls** (subplan 2-4 track T).
//!
//! Trust boundary: the TLS certificate chain. rsbinder **never invents
//! crypto** — key/cert/root management and all verification are the
//! caller's `rustls::ClientConfig`/`ServerConfig` and rustls itself
//! (plan §5). A failed certificate check is rejected at the handshake,
//! before a single RPC payload byte is exchanged (AC-4.5).
//!
//! The peer identity is the leaf certificate: subject label + SHA-256
//! fingerprint ([`super::CertId`]). There is deliberately **no
//! plaintext-network backend** — `tcp_debug` is debug-only and
//! `Anonymous`; real networks must use this.
//!
//! Additive invariant (AC-4.1): this file + the feature + the
//! `PeerIdentity` variants are the *only* change — the 2-2/2-3 wire /
//! state / session / server core is untouched and is exercised
//! unmodified with the transport swapped.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::Mutex;

use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection, StreamOwned};
use sha2::{Digest, Sha256};

use super::{read_frame, write_frame, CertId, PeerIdentity, RpcTransport};
use crate::rpc::{RpcError, RpcResult};

/// Client or server rustls stream (both impl `Read`/`Write`).
enum TlsIo {
    Client(StreamOwned<ClientConnection, TcpStream>),
    Server(StreamOwned<ServerConnection, TcpStream>),
}

impl TlsIo {
    fn sock(&self) -> &TcpStream {
        match self {
            TlsIo::Client(s) => s.get_ref(),
            TlsIo::Server(s) => s.get_ref(),
        }
    }
}

impl Read for TlsIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            TlsIo::Client(s) => s.read(buf),
            TlsIo::Server(s) => s.read(buf),
        }
    }
}

impl Write for TlsIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            TlsIo::Client(s) => s.write(buf),
            TlsIo::Server(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            TlsIo::Client(s) => s.flush(),
            TlsIo::Server(s) => s.flush(),
        }
    }
}

/// A framed transport over a completed TLS connection.
pub struct TlsTransport {
    // One thread per connection in the RPC model, so a `Mutex` (not
    // concurrent read+write on one TLS connection) is sufficient and
    // keeps `&self`.
    io: Mutex<TlsIo>,
    peer: PeerIdentity,
    desc: String,
}

/// SHA-256 of the peer's leaf certificate, as a [`CertId`]. `subject`
/// is a caller-meaningful label (the SNI for a client-side peer, a
/// fixed marker for an mTLS client) — rsbinder does not parse X.509;
/// the fingerprint is the authoritative identity (plan 2-4.t2).
fn cert_identity(
    certs: Option<&[rustls::pki_types::CertificateDer<'_>]>,
    subject: &str,
) -> RpcResult<CertId> {
    let leaf = certs
        .and_then(|c| c.first())
        .ok_or(RpcError::Protocol("TLS peer presented no certificate"))?;
    let mut h = Sha256::new();
    h.update(leaf.as_ref());
    let mut fp = [0u8; 32];
    fp.copy_from_slice(&h.finalize());
    Ok(CertId::new(subject.to_string(), fp))
}

fn drive_handshake_client(conn: &mut ClientConnection, sock: &mut TcpStream) -> RpcResult<()> {
    while conn.is_handshaking() {
        // A verification failure surfaces here as an I/O error AFTER
        // the alert — i.e. before any RPC payload (AC-4.5).
        conn.complete_io(sock)?;
    }
    Ok(())
}

fn drive_handshake_server(conn: &mut ServerConnection, sock: &mut TcpStream) -> RpcResult<()> {
    while conn.is_handshaking() {
        conn.complete_io(sock)?;
    }
    Ok(())
}

impl TlsTransport {
    /// Client side: TLS-handshake over an established `tcp` stream to
    /// `server_name`, verifying the server per `config`. Returns only
    /// after a successful handshake; a bad/expired/untrusted server
    /// certificate is an `Err` here, with **no RPC bytes exchanged**.
    pub fn connect(
        mut tcp: TcpStream,
        server_name: &str,
        config: Arc<rustls::ClientConfig>,
    ) -> RpcResult<Self> {
        tcp.set_nodelay(true)?;
        let name = ServerName::try_from(server_name.to_string())
            .map_err(|_| RpcError::Protocol("invalid TLS server name"))?;
        let mut conn = ClientConnection::new(config, name)
            .map_err(|_| RpcError::Protocol("rustls ClientConnection::new failed"))?;
        drive_handshake_client(&mut conn, &mut tcp)?;
        let peer = PeerIdentity::Certificate(cert_identity(conn.peer_certificates(), server_name)?);
        let desc = format!("tls:{server_name}");
        Ok(TlsTransport {
            io: Mutex::new(TlsIo::Client(StreamOwned::new(conn, tcp))),
            peer,
            desc,
        })
    }

    /// Server side: TLS-handshake over an accepted `tcp` stream per
    /// `config`. With an mTLS config the client certificate is
    /// required + verified by rustls; its absence/invalidity fails the
    /// handshake here (AC-4.5).
    pub fn accept(mut tcp: TcpStream, config: Arc<rustls::ServerConfig>) -> RpcResult<Self> {
        tcp.set_nodelay(true)?;
        let mut conn = ServerConnection::new(config)
            .map_err(|_| RpcError::Protocol("rustls ServerConnection::new failed"))?;
        drive_handshake_server(&mut conn, &mut tcp)?;
        // mTLS: a client cert (if the config requires one) yields a
        // Certificate identity; otherwise the peer is Anonymous (the
        // server authenticated *to* the client, not vice-versa).
        let peer = match conn.peer_certificates() {
            Some(c) if !c.is_empty() => {
                PeerIdentity::Certificate(cert_identity(Some(c), "<mtls-client>")?)
            }
            _ => PeerIdentity::Anonymous,
        };
        Ok(TlsTransport {
            io: Mutex::new(TlsIo::Server(StreamOwned::new(conn, tcp))),
            peer,
            desc: "tls:server".to_string(),
        })
    }
}

impl RpcTransport for TlsTransport {
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
        let mut io = self.io.lock().expect("tls io poisoned");
        write_frame(&mut *io, buf)
    }

    fn recv_frame(&self) -> RpcResult<Vec<u8>> {
        let mut io = self.io.lock().expect("tls io poisoned");
        read_frame(&mut *io)
    }

    fn peer_identity(&self) -> PeerIdentity {
        self.peer.clone()
    }

    fn describe(&self) -> &str {
        &self.desc
    }

    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        let io = self.io.lock().expect("tls io poisoned");
        io.sock().set_read_timeout(timeout)?;
        Ok(())
    }
}
