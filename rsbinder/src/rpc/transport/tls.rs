// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! TLS transport over **rustls**.
//!
//! Trust boundary: the TLS certificate chain. rsbinder **never invents
//! crypto** — key/cert/root management and all verification are the
//! caller's `rustls::ClientConfig`/`ServerConfig` and rustls itself.
//! A failed certificate check is rejected at the handshake, before a
//! single RPC payload byte is exchanged.
//!
//! The peer identity is the leaf certificate: subject label + SHA-256
//! fingerprint ([`super::CertId`]). There is deliberately **no
//! plaintext-network backend** — `tcp_debug` is debug-only and
//! `Anonymous`; real networks must use this.
//!
//! ## Decoupled from TCP and from framing
//!
//! TLS is **orthogonal to the socket kind** (mirrors AOSP
//! `RpcTransportCtx::newTransport(fd)`): the crypto state machine
//! (`rustls::Connection`) is held separately from the byte stream, so it
//! runs over **any** [`TlsStream`] — `TcpStream`, `UnixStream`, or a
//! `vsock` stream — not just TCP. It is also **profile-agnostic**: it
//! implements both the R34 length-framed I/O (`send_frame`/`recv_frame`)
//! and the android-13+ raw I/O (`send_raw`/`recv_raw`), so the opt-in
//! android-13+ `RpcSession` profile can run over TLS.
//!
//! ## Concurrency
//!
//! `Connection` (crypto) is behind a `Mutex` that is held **only for
//! in-memory work** — ciphertext is produced into a buffer, then the
//! lock is released and the socket write happens outside it (serialized
//! by a separate `wlock` for TLS-record atomicity). Blocking socket
//! reads happen lock-free. So a reader thread can `recv_*` while writer
//! threads `send_*` without the blocking-while-holding deadlock a single
//! coupled `StreamOwned`-behind-one-`Mutex` would cause — and a full
//! TCP send buffer never blocks while holding the crypto lock.
//!
//! TLS cannot carry out-of-band file descriptors (no `SCM_RIGHTS` over an
//! encrypted byte stream), so `send_*_with_fds` keep the trait's
//! rejecting default — fd-incapable *by type*, exactly as AOSP's
//! `FileDescriptorTransportMode::Unix` is incompatible with TLS.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConnection, Connection, ServerConnection};
use sha2::{Digest, Sha256};

use super::{read_frame, write_frame, CertId, PeerIdentity, RpcTransport};
use crate::rpc::{RpcError, RpcResult};

/// Ciphertext read chunk: one TLS record is ≤ 16 KiB, so this reads at
/// most a record-or-so worth of bytes per blocking socket `read`.
const TLS_READ_CHUNK: usize = 16 * 1024;

/// A connected, byte-oriented stream that TLS can run over.
///
/// Methods take `&self` (not the `&mut self` of `Read`/`Write`) so a
/// reader thread and a writer thread can share `&stream` and do
/// `read`/`write` concurrently — full-duplex sockets support this
/// without a lock (the same property `UnixTransport`/`VsockTransport`
/// rely on). `Send + Sync` so the owning `TlsTransport` is too.
pub trait TlsStream: Send + Sync {
    /// Read up to `buf.len()` bytes (`Ok(0)` = peer closed at the TCP
    /// layer). One underlying `read`; may be short.
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize>;
    /// Write up to `buf.len()` bytes; may be short.
    fn write(&self, buf: &[u8]) -> std::io::Result<usize>;
    /// Flush the underlying stream.
    fn flush(&self) -> std::io::Result<()>;
    /// Set the read deadline for subsequent `read`s (`None` = blocking).
    fn set_read_timeout(&self, t: Option<Duration>) -> std::io::Result<()>;
    /// Set the write deadline for subsequent `write`s (`None` = blocking).
    fn set_write_timeout(&self, t: Option<Duration>) -> std::io::Result<()>;
}

// All std stream types implement `Read`/`Write` for `&Stream`, so the
// `&self` methods forward through a shared reference with no lock.
impl TlsStream for TcpStream {
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&mut &*self).read(buf)
    }
    fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        (&mut &*self).write(buf)
    }
    fn flush(&self) -> std::io::Result<()> {
        (&mut &*self).flush()
    }
    fn set_read_timeout(&self, t: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, t)
    }
    fn set_write_timeout(&self, t: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_write_timeout(self, t)
    }
}

impl TlsStream for UnixStream {
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&mut &*self).read(buf)
    }
    fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        (&mut &*self).write(buf)
    }
    fn flush(&self) -> std::io::Result<()> {
        (&mut &*self).flush()
    }
    fn set_read_timeout(&self, t: Option<Duration>) -> std::io::Result<()> {
        UnixStream::set_read_timeout(self, t)
    }
    fn set_write_timeout(&self, t: Option<Duration>) -> std::io::Result<()> {
        UnixStream::set_write_timeout(self, t)
    }
}

#[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
impl TlsStream for vsock::VsockStream {
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&mut &*self).read(buf)
    }
    fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        (&mut &*self).write(buf)
    }
    fn flush(&self) -> std::io::Result<()> {
        (&mut &*self).flush()
    }
    fn set_read_timeout(&self, t: Option<Duration>) -> std::io::Result<()> {
        vsock::VsockStream::set_read_timeout(self, t)
    }
    fn set_write_timeout(&self, t: Option<Duration>) -> std::io::Result<()> {
        vsock::VsockStream::set_write_timeout(self, t)
    }
}

/// Bridges a `&dyn TlsStream` to `std::io::{Read, Write}` so rustls's
/// blocking `complete_io` can drive the handshake over it.
struct IoAdapter<'a>(&'a dyn TlsStream);
impl Read for IoAdapter<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}
impl Write for IoAdapter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// A framed-or-raw transport over a completed TLS connection, decoupled
/// from the socket kind and from the wire profile.
pub struct TlsTransport {
    /// rustls crypto state (both directions). Held **only for in-memory
    /// work** — never across a blocking socket op.
    conn: Mutex<Connection>,
    /// Write-path lock: held across the **encrypt-drain + transmit** of
    /// every writer (data send and control flush) so the on-wire TLS
    /// record order always equals rustls's sequence-number order. The
    /// reader takes it with `try_lock` for an opportunistic control
    /// flush and never blocks on it (see `flush_control`).
    wlock: Mutex<()>,
    /// The byte stream; reads are lock-free, writes go under `wlock`.
    stream: Box<dyn TlsStream>,
    peer: PeerIdentity,
    desc: String,
}

/// SHA-256 of the peer's leaf certificate, as a [`CertId`]. `subject`
/// is a caller-meaningful label (the SNI for a client-side peer, a
/// fixed marker for an mTLS client) — rsbinder does not parse X.509;
/// the fingerprint is the authoritative identity.
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

/// Drive the TLS handshake to completion over `stream` (blocking,
/// single-threaded — before the connection is shared). A verification
/// failure surfaces here, before any RPC payload.
fn drive_handshake(conn: &mut Connection, stream: &dyn TlsStream) -> RpcResult<()> {
    let mut io = IoAdapter(stream);
    while conn.is_handshaking() {
        let (_rd, _wr) = conn.complete_io(&mut io)?;
    }
    // Flush any trailing handshake flight still queued.
    while conn.wants_write() {
        conn.write_tls(&mut io)?;
    }
    Ok(())
}

impl TlsTransport {
    /// Client side over **any** stream: TLS-handshake to `server_name`,
    /// verifying the server per `config`. Returns only after a
    /// successful handshake; a bad/untrusted server certificate is an
    /// `Err` here, with **no RPC bytes exchanged**.
    pub fn connect_stream(
        stream: Box<dyn TlsStream>,
        server_name: &str,
        config: Arc<rustls::ClientConfig>,
    ) -> RpcResult<Self> {
        let name = ServerName::try_from(server_name.to_string())
            .map_err(|_| RpcError::Protocol("invalid TLS server name"))?;
        let cc = ClientConnection::new(config, name)
            .map_err(|_| RpcError::Protocol("rustls ClientConnection::new failed"))?;
        let mut conn: Connection = cc.into();
        drive_handshake(&mut conn, &*stream)?;
        let peer = PeerIdentity::Certificate(cert_identity(conn.peer_certificates(), server_name)?);
        Ok(TlsTransport {
            conn: Mutex::new(conn),
            wlock: Mutex::new(()),
            stream,
            peer,
            desc: format!("tls:{server_name}"),
        })
    }

    /// Server side over **any** stream: TLS-handshake per `config`. With
    /// an mTLS config the client certificate is required + verified by
    /// rustls; its absence/invalidity fails the handshake here.
    ///
    /// Note: a non-mTLS `ServerConfig` (no client-auth verifier) yields a
    /// [`PeerIdentity::Anonymous`] connection — encrypted but with no
    /// authenticated peer. Authorization of such peers must be enforced
    /// by the caller's `set_authorizer` chokepoint.
    pub fn accept_stream(
        stream: Box<dyn TlsStream>,
        config: Arc<rustls::ServerConfig>,
    ) -> RpcResult<Self> {
        let sc = ServerConnection::new(config)
            .map_err(|_| RpcError::Protocol("rustls ServerConnection::new failed"))?;
        let mut conn: Connection = sc.into();
        drive_handshake(&mut conn, &*stream)?;
        let peer = match conn.peer_certificates() {
            Some(c) if !c.is_empty() => {
                PeerIdentity::Certificate(cert_identity(Some(c), "<mtls-client>")?)
            }
            _ => PeerIdentity::Anonymous,
        };
        Ok(TlsTransport {
            conn: Mutex::new(conn),
            wlock: Mutex::new(()),
            stream,
            peer,
            desc: "tls:server".to_string(),
        })
    }

    /// Client side over an established `tcp` stream (back-compat
    /// convenience; sets `TCP_NODELAY`). Equivalent to boxing the
    /// stream into [`TlsTransport::connect_stream`].
    pub fn connect(
        tcp: TcpStream,
        server_name: &str,
        config: Arc<rustls::ClientConfig>,
    ) -> RpcResult<Self> {
        tcp.set_nodelay(true)?;
        Self::connect_stream(Box::new(tcp), server_name, config)
    }

    /// Server side over an accepted `tcp` stream (back-compat
    /// convenience; sets `TCP_NODELAY`).
    pub fn accept(tcp: TcpStream, config: Arc<rustls::ServerConfig>) -> RpcResult<Self> {
        tcp.set_nodelay(true)?;
        Self::accept_stream(Box::new(tcp), config)
    }

    /// Transmit `cipher` to the socket. **Caller must hold `wlock`** so
    /// that, for every writer, ciphertext is drained from rustls
    /// (`write_tls`) and put on the wire under one continuous `wlock`
    /// hold — the on-wire TLS record order then always equals rustls's
    /// sequence-number (encryption) order. Decoupling the drain from the
    /// transmit (separate locks) would let a concurrent writer reorder
    /// records on the wire → AEAD sequence mismatch → fatal alert.
    fn write_socket_locked(&self, cipher: &[u8]) -> RpcResult<()> {
        if cipher.is_empty() {
            return Ok(());
        }
        let mut off = 0;
        while off < cipher.len() {
            match self.stream.write(&cipher[off..]) {
                Ok(0) => return Err(RpcError::PeerClosed),
                Ok(n) => off += n,
                Err(e) => return Err(e.into()),
            }
        }
        self.stream.flush()?;
        Ok(())
    }

    /// Flush any control-plane ciphertext rustls queued in response to
    /// inbound data (`KeyUpdate`/alert/`close_notify`) — **without
    /// blocking on `wlock`**. If a sender currently holds `wlock`, skip:
    /// that sender drains the *shared* output buffer (which now includes
    /// these control records) in sequence order under its own `wlock`,
    /// and `recv_raw` retries on its next iteration. This keeps the
    /// reader from ever blocking behind a (possibly back-pressured)
    /// socket write — preserving the lock-free-duplex liveness, while
    /// still ordering every write_tls → transmit under `wlock`.
    fn flush_control(&self) -> RpcResult<()> {
        let Ok(_g) = self.wlock.try_lock() else {
            return Ok(());
        };
        let cipher = {
            let mut c = self.conn.lock().expect("tls conn poisoned");
            if !c.wants_write() {
                return Ok(());
            }
            let mut v = Vec::new();
            c.write_tls(&mut v)?;
            v
        };
        self.write_socket_locked(&cipher)
    }

    /// Pull one chunk of ciphertext off the socket (lock-free) and feed
    /// it into the crypto state. Returns `false` on a clean TCP EOF.
    fn pump_incoming(&self) -> RpcResult<bool> {
        let mut tmp = [0u8; TLS_READ_CHUNK];
        let k = self.stream.read(&mut tmp)?;
        if k == 0 {
            return Ok(false); // peer closed the socket
        }
        let mut c = self.conn.lock().expect("tls conn poisoned");
        let mut src: &[u8] = &tmp[..k];
        while !src.is_empty() {
            let n = c.read_tls(&mut src)?;
            if n == 0 {
                break;
            }
            c.process_new_packets()
                .map_err(|_| RpcError::Protocol("TLS record processing failed"))?;
        }
        Ok(true)
    }
}

impl RpcTransport for TlsTransport {
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
        // R34 length-prefix framing reused verbatim over a Write adapter
        // that drives `send_raw`, so the framed bytes are byte-identical
        // to every other stream backend.
        write_frame(&mut RawIo(self), buf)
    }

    fn recv_frame(&self) -> RpcResult<Vec<u8>> {
        read_frame(&mut RawIo(self))
    }

    fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
        // `wlock` spans BOTH the rustls encrypt-drain and the socket
        // transmit so the on-wire record order equals the sequence-number
        // order even under a concurrent recv-side control flush (the
        // `conn` lock is released before the blocking transmit, so the
        // reader's `pump_incoming` can still drain — no flow-control
        // deadlock).
        let _g = self.wlock.lock().expect("tls wlock poisoned");
        let cipher = {
            let mut c = self.conn.lock().expect("tls conn poisoned");
            c.writer().write_all(buf)?;
            let mut v = Vec::new();
            c.write_tls(&mut v)?;
            v
        };
        self.write_socket_locked(&cipher)
    }

    /// Single-reader: one thread drives `recv_*` per connection (the RPC
    /// serve loop / the in-flight transact's reply wait). Concurrent
    /// multi-reader would interleave `pump_incoming` socket reads and
    /// corrupt the ciphertext stream — not a supported call pattern (the
    /// trait contract is one sender thread + one receiver thread).
    fn recv_raw(&self, out: &mut [u8]) -> RpcResult<usize> {
        loop {
            // 1. Drain already-decrypted plaintext (crypto lock only —
            //    reads emit no wire bytes, so no `wlock` is needed).
            {
                let mut c = self.conn.lock().expect("tls conn poisoned");
                match c.reader().read(out) {
                    Ok(n) if n > 0 => return Ok(n),
                    Ok(_) => return Ok(0), // clean close_notify, all drained
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    // Unclean TCP EOF after rustls drained: treat as close.
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(0),
                    Err(e) => return Err(e.into()),
                }
            }
            // 2. Opportunistically flush any control-plane output rustls
            //    queued (non-blocking on `wlock`; see `flush_control`).
            self.flush_control()?;
            // 3. Block on the socket (lock-free) for more ciphertext.
            if !self.pump_incoming()? {
                return Ok(0);
            }
        }
    }

    fn peer_identity(&self) -> PeerIdentity {
        self.peer.clone()
    }

    fn describe(&self) -> &str {
        &self.desc
    }

    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        self.stream.set_read_timeout(timeout)?;
        Ok(())
    }

    fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        self.stream.set_write_timeout(timeout)?;
        Ok(())
    }
}

/// `Read`/`Write` adapter that drives R34 framing over the raw TLS I/O,
/// so [`write_frame`]/[`read_frame`] produce byte-identical framed bytes.
struct RawIo<'a>(&'a TlsTransport);
impl Read for RawIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.0.recv_raw(buf) {
            Ok(n) => Ok(n),
            // Preserve the io kind so `read_header`'s timeout detection
            // (`WouldBlock`/`TimedOut`) and clean-close handling stay
            // byte-for-byte the R34 behavior over a plain socket.
            Err(RpcError::Io(e)) => Err(e),
            Err(RpcError::PeerClosed) => Ok(0),
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    }
}
impl Write for RawIo<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_all(buf)?;
        Ok(buf.len())
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.0
            .send_raw(buf)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(()) // send_raw already flushes the socket
    }
}
