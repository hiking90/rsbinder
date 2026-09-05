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

/// How long [`TlsTransport::shutdown`] may spend putting `close_notify` on
/// the wire. Teardown must stay finite: without a bound, a peer that
/// stopped reading (its send buffer full) holds `shutdown` — and every
/// transport queued behind it in `shutdown_all_transports` — forever.
const CLOSE_NOTIFY_TIMEOUT: Duration = Duration::from_millis(500);

/// How long [`TlsTransport::shutdown`] waits for the send in flight before
/// giving up on `close_notify` and cutting the socket anyway.
///
/// Only the thread holding `wlock` may put records on the wire, and
/// `shutdown` refuses every send that starts after it, so there is at
/// most one such send and it ends in one of two ways. It completes — the
/// lock's release wakes `shutdown` at once, and this bound is never felt.
/// Or it is parked in a socket write to a peer that has stopped reading —
/// then nothing short of the cut unparks it, the alert could not reach
/// that peer regardless, and this is how long teardown spends finding
/// that out. `shutdown_all_transports` walks slots one at a time, so it is
/// paid once per stalled connection.
const CLOSE_NOTIFY_WLOCK_WAIT: Duration = Duration::from_millis(50);

/// The write-path lock. A mutex with a bounded acquire, which `std`'s
/// lacks: [`TlsTransport::shutdown`] must wait for the send in flight, but
/// not for a sender parked on a peer that has stopped reading.
///
/// Every release signals `freed`, so a waiter wakes the moment the send
/// ends rather than on a poll tick.
struct WriteLock {
    busy: Mutex<bool>,
    freed: std::sync::Condvar,
}

/// Holding this is holding `wlock`; dropping it releases and signals.
struct WriteGuard<'a>(&'a WriteLock);

impl WriteLock {
    fn new() -> Self {
        Self {
            busy: Mutex::new(false),
            freed: std::sync::Condvar::new(),
        }
    }

    fn lock(&self) -> WriteGuard<'_> {
        let mut busy = self.busy.lock().expect("tls wlock poisoned");
        while *busy {
            busy = self.freed.wait(busy).expect("tls wlock poisoned");
        }
        *busy = true;
        WriteGuard(self)
    }

    fn try_lock(&self) -> Option<WriteGuard<'_>> {
        let mut busy = self.busy.lock().expect("tls wlock poisoned");
        if *busy {
            return None;
        }
        *busy = true;
        Some(WriteGuard(self))
    }

    /// `None` once `wait` has passed with the lock still held.
    fn lock_timeout(&self, wait: Duration) -> Option<WriteGuard<'_>> {
        let deadline = std::time::Instant::now() + wait;
        let mut busy = self.busy.lock().expect("tls wlock poisoned");
        while *busy {
            let now = std::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = self
                .freed
                .wait_timeout(busy, deadline - now)
                .expect("tls wlock poisoned");
            busy = guard;
        }
        *busy = true;
        Some(WriteGuard(self))
    }
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        *self.0.busy.lock().expect("tls wlock poisoned") = false;
        self.0.freed.notify_all();
    }
}

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
    /// Shut the underlying stream down in both directions (wakes a
    /// blocked `read`).
    fn shutdown_stream(&self) -> std::io::Result<()>;
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
    fn shutdown_stream(&self) -> std::io::Result<()> {
        TcpStream::shutdown(self, std::net::Shutdown::Both)
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
    fn shutdown_stream(&self) -> std::io::Result<()> {
        UnixStream::shutdown(self, std::net::Shutdown::Both)
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
    fn shutdown_stream(&self) -> std::io::Result<()> {
        vsock::VsockStream::shutdown(self, std::net::Shutdown::Both)
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
    /// flush and never blocks on it (see `flush_control`); `shutdown`
    /// takes it with a bound, after the send in flight (see
    /// `CLOSE_NOTIFY_WLOCK_WAIT`).
    wlock: WriteLock,
    /// The byte stream; reads are lock-free, writes go under `wlock`.
    stream: Box<dyn TlsStream>,
    peer: PeerIdentity,
    desc: String,
    /// Set by [`shutdown`](RpcTransport::shutdown): the end of stream our
    /// own reader then sees carries no `close_notify` from the peer, and
    /// must not be reported as a cut — it is ours.
    shut: std::sync::atomic::AtomicBool,
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
            wlock: WriteLock::new(),
            stream,
            peer,
            desc: format!("tls:{server_name}"),
            shut: std::sync::atomic::AtomicBool::new(false),
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
            wlock: WriteLock::new(),
            stream,
            peer,
            desc: "tls:server".to_string(),
            shut: std::sync::atomic::AtomicBool::new(false),
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
                Ok(0) => return Err(RpcError::EndOfStream),
                Ok(n) => off += n,
                // EINTR: a signal interrupted the write — retry (matches the
                // plain-socket backends).
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
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
        let Some(_g) = self.wlock.try_lock() else {
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
    /// it into the crypto state. Returns `false` on TCP EOF — after
    /// handing that EOF to rustls, so its reader can then tell a
    /// `close_notify` (clean) from a cut stream (`UnexpectedEof`).
    fn pump_incoming(&self) -> RpcResult<bool> {
        let mut tmp = [0u8; TLS_READ_CHUNK];
        let k = loop {
            match self.stream.read(&mut tmp) {
                Ok(k) => break k,
                // EINTR: retry the interrupted blocking read.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // A read deadline elapsed mid-stream: surface as `Timeout` so
                // the android-13+ frame path maps it to `TimedOut`/`Truncated`
                // like the plain-socket backends (this transport's module doc
                // lists android-13+-over-TLS as a supported profile).
                Err(e) if super::is_timeout(&e) => return Err(RpcError::Timeout),
                Err(e) => return Err(e.into()),
            }
        };
        let mut c = self.conn.lock().expect("tls conn poisoned");
        if k == 0 {
            let mut eof: &[u8] = &[];
            let _ = c.read_tls(&mut eof);
            return Ok(false);
        }
        let mut src: &[u8] = &tmp[..k];
        while !src.is_empty() {
            let n = c.read_tls(&mut src)?;
            if n == 0 {
                break;
            }
            c.process_new_packets().map_err(|e| {
                // Log the rustls detail (bad_record_mac, alert kind, …) — it is
                // otherwise lost behind the static `Protocol` message. rustls
                // has queued any fatal alert; the next `send_raw`/`flush_control`
                // drains it best-effort.
                log::warn!("TLS record processing failed: {e}");
                RpcError::Protocol("TLS record processing failed")
            })?;
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
        // After our own `shutdown` a send fails here, before touching
        // rustls or the lock. It is what the trait promises ("later sends
        // fail"), and it is what makes `shutdown`'s wait a handoff: with
        // new senders refused, the one already holding `wlock` is the only
        // one there is, and its release is the only event to wait for.
        // Without this a busy sender re-takes the lock the moment it lets
        // go and can starve `shutdown` past its bound — and every frame it
        // sent meanwhile, queued behind an alert the peer discards it
        // after (RFC 8446 §6.1), was reported `Ok`.
        if self.shut.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RpcError::EndOfStream);
        }
        // `wlock` spans BOTH the rustls encrypt-drain and the socket
        // transmit so the on-wire record order equals the sequence-number
        // order even under a concurrent recv-side control flush (the
        // `conn` lock is released before the blocking transmit, so the
        // reader's `pump_incoming` can still drain — no flow-control
        // deadlock).
        let _g = self.wlock.lock();
        // rustls bounds its plaintext sendable buffer (`DEFAULT_BUFFER_LIMIT`,
        // ~64 KiB); feeding a larger frame in a single `write_all` fails with
        // `WriteZero`. Chunk the plaintext and interleave encrypt
        // (`write_tls`) + transmit so any frame size (up to `MAX_FRAME_LEN` =
        // 64 MiB) streams. The record order still equals the sequence order
        // because the whole loop holds `wlock`.
        debug_assert!(!buf.is_empty(), "send_raw with an empty frame");
        let mut cipher = Vec::new();
        let mut off = 0;
        while off < buf.len() {
            {
                let mut c = self.conn.lock().expect("tls conn poisoned");
                let n = c.writer().write(&buf[off..])?;
                if n == 0 {
                    return Err(RpcError::Protocol("rustls accepted no plaintext"));
                }
                off += n;
                cipher.clear();
                c.write_tls(&mut cipher)?;
            }
            self.write_socket_locked(&cipher)?;
        }
        Ok(())
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
                    Ok(_) => return Ok(0), // close_notify received, all drained
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    // TCP EOF with no close_notify: on this backend that is
                    // what a cut stream looks like, so it is not a close.
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        // Our own shutdown produces exactly this on our
                        // side: an end of stream, no close_notify from the
                        // peer. That is a close, not a cut.
                        if self.shut.load(std::sync::atomic::Ordering::SeqCst) {
                            return Ok(0);
                        }
                        log::warn!("TLS stream ended without close_notify ({})", self.desc);
                        return Err(RpcError::UncleanEndOfStream);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            // 2. Opportunistically flush any control-plane output rustls
            //    queued (non-blocking on `wlock`; see `flush_control`).
            if let Err(e) = self.flush_control() {
                if self.shut.load(std::sync::atomic::Ordering::SeqCst) {
                    // Our own `shutdown` breaks the write half on purpose,
                    // and the trait promises a reader it wakes sees the end
                    // of the stream, never a distinct "shut down locally"
                    // error. `shutdown` bounds its own close_notify write, so
                    // between setting the flag and breaking the stream that
                    // write can expire rather than fail outright; report that
                    // deadline as the end too, since with no deadline of ours
                    // armed a timeout reads as the kernel's and the session
                    // would end saying the stream was lost.
                    let deadline = matches!(&e, RpcError::Io(io) if super::is_timeout(io));
                    return Err(if deadline { RpcError::EndOfStream } else { e });
                }
                // Otherwise the outbound half is lost: those records left
                // rustls before the write, so the peer never sees them —
                // and a write that stopped part-way (a send deadline on a
                // full socket buffer) left it a truncated record it can
                // decrypt nothing after. That must not surface on the read
                // side as a boundary-preserving error, which would keep the
                // connection in the pool as if the stream were still good.
                log::warn!(
                    "TLS control flush failed, outbound half lost: {e} ({})",
                    self.desc
                );
                return Err(RpcError::UncleanEndOfStream);
            }
            // 3. Block on the socket (lock-free) for more ciphertext. On EOF
            //    rustls now knows, and the next pass of step 1 decides.
            self.pump_incoming()?;
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

    fn shutdown(&self) -> RpcResult<()> {
        // Refuse every send from here on (`send_raw` checks this first). At
        // most one send is then still running — the one already holding
        // `wlock` — and it is the only thing the wait below is for.
        self.shut.store(true, std::sync::atomic::Ordering::SeqCst);
        // Bound the socket writes from here: that send's remaining chunks
        // and our own alert. `write_socket_locked` blocks until the whole
        // buffer is on the wire, and this is a teardown path whose caller
        // (`shutdown_all_transports`, then `terminate`'s join) has no other
        // way out. A peer that stalled sees a partial record — an unclean
        // end, which is what a stalled peer is getting anyway.
        let _ = self.stream.set_write_timeout(Some(CLOSE_NOTIFY_TIMEOUT));
        // Only the `wlock` holder may put records on the wire, so take it —
        // after the send in flight, if there is one — and only then queue
        // and transmit `close_notify`: the alert follows a complete frame
        // rather than cutting into one, and the peer reads that frame,
        // then a clean end. Keep holding it across the socket cut: released
        // before, a sender could slip a frame in between, be told `Ok`, and
        // the peer would discard it (RFC 8446 §6.1).
        let held = self.wlock.lock_timeout(CLOSE_NOTIFY_WLOCK_WAIT);
        if held.is_some() {
            let mut cipher = Vec::new();
            {
                let mut c = self.conn.lock().expect("tls conn poisoned");
                c.send_close_notify(); // idempotent in rustls
                let _ = c.write_tls(&mut cipher);
            }
            if !cipher.is_empty() {
                let _ = self.write_socket_locked(&cipher);
            }
        }
        // `None`: the sender is parked in a socket write to a peer that has
        // stopped reading. No alert could reach that peer; the cut is what
        // unparks the sender (`EPIPE`), and it reports its frame as failed.
        let cut = super::absorb_already_shut(self.stream.shutdown_stream());
        drop(held);
        cut
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
            // A read-deadline timeout (mapped in `pump_incoming`) must keep the
            // `TimedOut` io kind so `read_header`'s `is_timeout` still fires —
            // otherwise it degrades to a generic `Other` and the frame path
            // loses the Timeout/Truncated contract.
            Err(RpcError::Timeout) => Err(std::io::ErrorKind::TimedOut.into()),
            Err(RpcError::EndOfStream) => Ok(0),
            // Carried as the payload so `From<io::Error>` hands it back as
            // itself on the far side of `read_header`.
            Err(e @ RpcError::UncleanEndOfStream) => Err(std::io::Error::from(e)),
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
        // Kind-preserving like `RawTransportIo`: a write to a peer that
        // already closed must reach `write_frame`'s `?` as `EndOfStream`
        // (`DeadObject`), not an unclassified `Io(Other)`.
        self.0.send_raw(buf).map_err(std::io::Error::from)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(()) // send_raw already flushes the socket
    }
}
