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
//! by a separate `wlock` for TLS-record atomicity). A blocking socket
//! read holds neither lock (only the reader-only `pending_in`, which
//! parks ciphertext rustls cannot take yet). So a reader thread can `recv_*` while writer
//! threads `send_*` without the blocking-while-holding deadlock a single
//! coupled `StreamOwned`-behind-one-`Mutex` would cause — and a full
//! TCP send buffer never blocks while holding the crypto lock.
//!
//! `wlock` spans both the rustls encrypt-drain and the socket transmit, for
//! data sends and for the reader's control flush alike, so the on-wire
//! record order equals the sequence-number order. Separate locks for drain
//! and transmit would let a concurrent writer reorder records on the wire:
//! an AEAD sequence mismatch, then a fatal alert. The reader takes `wlock`
//! only with `try_lock` (`flush_control`) and never blocks on it: a sender
//! holding it drains the shared rustls output buffer, the reader's queued
//! control records included, in sequence order, and `recv_raw` retries on
//! its next iteration. The `conn` lock is
//! released before the blocking transmit, so the reader's `pump_incoming`
//! can still drain — no flow-control deadlock. rustls bounds its buffered
//! plaintext (`DEFAULT_BUFFER_LIMIT`, ~64 KiB) and one `write_all` of a
//! larger frame fails with `WriteZero`, so `send_raw` chunks the plaintext
//! and interleaves `write_tls` + transmit under a single `wlock` hold; any
//! frame up to `MAX_FRAME_LEN` streams, still in sequence order.
//!
//! TLS cannot carry out-of-band file descriptors (no `SCM_RIGHTS` over an
//! encrypted byte stream), so `send_*_with_fds` keep the trait's
//! rejecting default — fd-incapable *by type*, exactly as AOSP's
//! `FileDescriptorTransportMode::Unix` is incompatible with TLS.
//!
//! ## Shutdown
//!
//! `shutdown` sets `shut` first, so `send_raw` refuses every later send
//! before touching rustls or the lock — the trait's "later sends fail". At
//! most one send, the one already holding `wlock`, is then still running,
//! and its release is the only event `shutdown` waits for. Without the
//! refusal a busy sender re-takes the lock as soon as it lets go, starves
//! `shutdown` past its bound, and every frame it sent meanwhile, queued
//! behind an alert the peer discards it after (RFC 8446 §6.1), was
//! reported `Ok`.
//!
//! `shutdown` then bounds socket writes with `CLOSE_NOTIFY_TIMEOUT`: the
//! in-flight send's remaining chunks and the alert. `write_socket_locked`
//! blocks until the whole buffer is on the wire, and the caller
//! (`shutdown_all_transports`, then `terminate`'s join) has no other way
//! out. A stalled peer sees a partial record — an unclean end, which it was
//! getting anyway.
//!
//! It takes `wlock` (bounded by `CLOSE_NOTIFY_WLOCK_WAIT`) before queueing
//! `close_notify`, so the alert follows a complete frame rather than cutting
//! into one, and the peer reads that frame, then a clean end. It keeps
//! holding `wlock` across the socket cut: released earlier, a sender could
//! slip a frame in, be told `Ok`, and the peer would discard it (§6.1). If
//! the wait expires, the sender is parked writing to a peer that stopped
//! reading: no alert could reach that peer, the cut unparks the sender
//! (`EPIPE`), and it reports its frame as failed. A send that completes
//! releases `wlock` and wakes `shutdown` at once, so the bound is felt only
//! for a parked sender; `shutdown_all_transports` walks slots one at a time,
//! so it is paid once per stalled connection. `wlock` is a `WriteLock`
//! rather than a `std` `Mutex` because it needs this bounded acquire.
//!
//! On the read side, a `flush_control` failure after `shut` is set reads as
//! the end of stream when the shutdown explains it: `shutdown` breaks the
//! write half on purpose (`EPIPE`), and the trait promises a reader it wakes
//! sees the end, never a distinct "shut down locally" error. The bounded
//! `close_notify` write can also expire (`EAGAIN`) between setting the flag
//! and the cut; left as it is, the loop would take that `Timeout` for its own
//! read deadline — an idle eviction with one armed, a lost stream without —
//! so it too becomes `EndOfStream`. A failure the shutdown does not explain
//! — a connection the kernel gave up on (`ETIMEDOUT`) — stays a lost stream.
//! Without `shut`, a failed control flush means the outbound half is lost —
//! those records left rustls before the write, and a write stopped part-way
//! (a send deadline on a full socket buffer) left the peer a truncated record
//! it decrypts nothing after — so it surfaces as `UncleanEndOfStream`, never
//! as a boundary-preserving error that the session would treat as a deadline
//! of its own.

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

/// Ciphertext read chunk: one TLS record is ≤ 16 KiB, so a read takes about one record.
const TLS_READ_CHUNK: usize = 16 * 1024;

/// Bound on `shutdown`'s socket writes (in-flight chunks + alert); see module doc "Shutdown".
const CLOSE_NOTIFY_TIMEOUT: Duration = Duration::from_millis(500);

/// Bound on `shutdown`'s wait for the in-flight send's `wlock`; see module doc "Shutdown".
const CLOSE_NOTIFY_WLOCK_WAIT: Duration = Duration::from_millis(50);

/// `wlock`: a mutex with a bounded acquire; every release signals `freed` (no poll tick).
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
    ///
    /// A write to a closed peer must fail with `EPIPE`, not raise
    /// `SIGPIPE`. The bundled `TcpStream`/`UnixStream`/`vsock` impls send
    /// with `MSG_NOSIGNAL` on Linux and Android. Apple has no
    /// `MSG_NOSIGNAL` and relies on the socket's `SO_NOSIGPIPE`: std sets
    /// it on the sockets its `connect` creates and `RpcServer` sets it on
    /// the streams it accepts. On a stream from any other source (your own
    /// `accept`, `UnixStream::pair`, a raw fd) the caller sets it.
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
    /// The stream's own peer-liveness check, as
    /// [`RpcTransport::set_liveness`] describes it. The default is a
    /// no-op; `TcpStream` implements it, since keepalive and
    /// `TCP_USER_TIMEOUT` are TCP options.
    fn set_liveness(&self, _t: Option<Duration>) -> std::io::Result<()> {
        Ok(())
    }
    /// Whether the peer has closed the stream, as
    /// [`RpcTransport::peer_closed`] describes it; `TlsTransport` answers
    /// with this. The default is `None` (unknown); the bundled streams
    /// implement it.
    fn peer_closed(&self) -> Option<bool> {
        None
    }
    /// The connected stream socket that `read` and `write` use, for a
    /// transaction send that reads while it waits
    /// (`RpcTransport::send_raw_draining`). `TlsTransport` writes that send
    /// to the socket itself with `MSG_DONTWAIT` (on Apple platforms, by setting
    /// `O_NONBLOCK` on the socket for the duration of each `send`) and `poll`s it, so the
    /// socket must carry exactly the bytes `read` and `write` do. The
    /// default is `None`: such a send then blocks without reading, and a
    /// peer that writes on the connection meanwhile can stall it. The
    /// bundled streams return their socket.
    fn socket(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        None
    }
}

// std streams implement `Read`/`Write` for `&Stream`, so `&self` forwards with no lock.
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
    fn set_liveness(&self, t: Option<Duration>) -> std::io::Result<()> {
        use std::os::fd::AsFd;
        super::tcp_liveness(self.as_fd(), t)
    }
    fn peer_closed(&self) -> Option<bool> {
        use std::os::fd::AsFd;
        super::socket_peer_closed(self.as_fd(), super::SocketKind::TcpOrVsock)
    }
    fn socket(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        Some(self.as_fd())
    }
}

impl TlsStream for UnixStream {
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&mut &*self).read(buf)
    }
    fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        // Not std's `write`: on the MSRV std that is `write(2)`, which raises `SIGPIPE`.
        rustix::net::send(self, buf, super::unix::SEND_FLAGS).map_err(std::io::Error::from)
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
    fn peer_closed(&self) -> Option<bool> {
        use std::os::fd::AsFd;
        super::socket_peer_closed(self.as_fd(), super::SocketKind::UnixDomain)
    }
    fn socket(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        Some(self.as_fd())
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
    fn peer_closed(&self) -> Option<bool> {
        use std::os::fd::AsFd;
        super::socket_peer_closed(self.as_fd(), super::SocketKind::TcpOrVsock)
    }
    fn socket(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        Some(self.as_fd())
    }
}

/// `&dyn TlsStream` as `std::io::{Read, Write}` so rustls's blocking `complete_io` can drive it.
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
    /// rustls crypto state (both directions); held for in-memory work only, never over socket I/O.
    conn: Mutex<Connection>,
    /// Held across every encrypt-drain + transmit; reader only `try_lock`s it (module doc).
    wlock: WriteLock,
    /// The byte stream; reads hold neither `conn` nor `wlock`, writes go under `wlock`.
    stream: Box<dyn TlsStream>,
    peer: PeerIdentity,
    desc: String,
    /// Set by `shutdown`: our reader's later EOF lacks the peer's `close_notify` but is no cut.
    shut: std::sync::atomic::AtomicBool,
    /// Ciphertext read off the socket but not yet fed to rustls; reader-only.
    pending_in: Mutex<Vec<u8>>,
}

/// Leaf-cert SHA-256 as [`CertId`]; `subject` is only a label (no X.509 parse), the hash decides.
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

/// Run the handshake to completion (blocking, before sharing); verification failures surface here.
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
            pending_in: Mutex::new(Vec::new()),
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
            pending_in: Mutex::new(Vec::new()),
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

    /// Transmit `cipher`; caller holds `wlock` since its `write_tls` (module doc "Concurrency").
    fn write_socket_locked(&self, cipher: &[u8]) -> RpcResult<()> {
        if cipher.is_empty() {
            return Ok(());
        }
        let mut off = 0;
        while off < cipher.len() {
            match self.stream.write(&cipher[off..]) {
                Ok(0) => return Err(RpcError::EndOfStream),
                Ok(n) => off += n,
                // EINTR: retry, as the plain-socket backends do.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        self.stream.flush()?;
        Ok(())
    }

    /// `send_raw`'s body; with a socket and `drain`, a waiting send reads (`send_raw_draining`).
    fn send_records(
        &self,
        buf: &[u8],
        mut draining: Option<(
            std::os::fd::BorrowedFd<'_>,
            &mut dyn FnMut() -> RpcResult<()>,
        )>,
    ) -> RpcResult<()> {
        // Refused after our `shutdown` so its `wlock` wait is a handoff (module doc "Shutdown").
        if self.shut.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RpcError::EndOfStream);
        }
        // Spans encrypt-drain + transmit so wire record order = sequence order (module doc).
        let _g = self.wlock.lock();
        // rustls caps buffered plaintext (~64 KiB), so chunk and interleave encrypt + transmit.
        debug_assert!(!buf.is_empty(), "send_raw with an empty frame");
        let mut cipher = Vec::new();
        let mut off = 0;
        while off < buf.len() {
            {
                let mut c = self.conn.lock().expect("tls conn poisoned");
                let n = c.writer().write(&buf[off..])?;
                if n == 0 {
                    const MSG: &str = "rustls accepted no plaintext";
                    // Past the first chunk the peer holds part of a frame: end the session.
                    return Err(if off == 0 {
                        RpcError::Protocol(MSG)
                    } else {
                        RpcError::Io(std::io::Error::other(MSG))
                    });
                }
                off += n;
                cipher.clear();
                c.write_tls(&mut cipher)?;
            }
            match draining.as_mut() {
                Some((sock, drain)) => self.write_socket_draining(&cipher, *sock, &mut **drain)?,
                None => self.write_socket_locked(&cipher)?,
            }
        }
        Ok(())
    }

    /// `write_socket_locked` that hands input to `drain` while the socket is full.
    fn write_socket_draining(
        &self,
        cipher: &[u8],
        sock: std::os::fd::BorrowedFd<'_>,
        drain: &mut dyn FnMut() -> RpcResult<()>,
    ) -> RpcResult<()> {
        let mut waiting = super::unix::SendWait::new(sock);
        let mut off = 0;
        while off < cipher.len() {
            let rest = &cipher[off..];
            let sent =
                super::unix::send_nonblocking(sock, |flags| rustix::net::send(sock, rest, flags));
            match sent {
                Ok(0) => return Err(RpcError::EndOfStream),
                Ok(n) => {
                    off += n;
                    waiting.progressed();
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::AGAIN) => {
                    // A send deadline, wherever the record stopped: as `write_socket_locked`.
                    let expired = || RpcError::Io(std::io::ErrorKind::WouldBlock.into());
                    if waiting.expired() {
                        return Err(expired());
                    }
                    if self.input_waiting(sock).map_err(super::read_side_failure)? {
                        drain().map_err(super::read_side_failure)?;
                    } else if waiting.wait()?.is_none() {
                        return Err(expired());
                    }
                }
                Err(e) => return Err(std::io::Error::from(e).into()),
            }
        }
        self.stream.flush()?;
        Ok(())
    }

    /// Plaintext or the peer's end is ready for `drain`; takes in what the socket holds, unblocked.
    fn input_waiting(&self, sock: std::os::fd::BorrowedFd<'_>) -> RpcResult<bool> {
        use rustix::event::{poll, PollFd, PollFlags, Timespec};
        loop {
            {
                let mut c = self.conn.lock().expect("tls conn poisoned");
                let io = c.process_new_packets().map_err(|e| {
                    log::warn!("TLS record processing failed: {e}");
                    RpcError::Protocol("TLS record processing failed")
                })?;
                if io.plaintext_bytes_to_read() > 0 || io.peer_has_closed() {
                    return Ok(true);
                }
            }
            if !self
                .pending_in
                .lock()
                .expect("tls pending_in poisoned")
                .is_empty()
            {
                self.pump_incoming()?;
                continue;
            }
            let mut fds = [PollFd::from_borrowed_fd(sock, PollFlags::IN)];
            let now = Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            match poll(&mut fds, Some(&now)) {
                Ok(_) if fds[0].revents().contains(PollFlags::IN) => {}
                // Nothing to read; an error flag alone is the next send's to report.
                Ok(_) => return Ok(false),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(std::io::Error::from(e).into()),
            }
            // Readable, so this read returns at once; at EOF `drain` reads the end.
            if !self.pump_incoming()? {
                return Ok(true);
            }
        }
    }

    /// Flush queued control records (`KeyUpdate`/alert); skips if `wlock` is held (module doc).
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

    /// Feeds parked `pending_in`, else one socket read (no `conn`/`wlock`); `false` = EOF.
    fn pump_incoming(&self) -> RpcResult<bool> {
        let mut pending = self.pending_in.lock().expect("tls pending_in poisoned");
        if !pending.is_empty() {
            let held = std::mem::take(&mut *pending);
            let mut c = self.conn.lock().expect("tls conn poisoned");
            Self::feed(&mut c, &held, &mut pending)?;
            return Ok(true);
        }
        let mut tmp = [0u8; TLS_READ_CHUNK];
        let k = loop {
            match self.stream.read(&mut tmp) {
                Ok(k) => break k,
                // EINTR: retry the interrupted blocking read.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // Deadline → `Timeout`, as plain sockets (android-13+ `DeadlineMidFrame` split).
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
        Self::feed(&mut c, &tmp[..k], &mut pending)?;
        Ok(true)
    }

    /// Feed `src` to rustls, parking the rest in `pending` once plaintext waits (16 KiB read cap).
    fn feed(c: &mut Connection, mut src: &[u8], pending: &mut Vec<u8>) -> RpcResult<()> {
        while !src.is_empty() {
            let n = c.read_tls(&mut src)?;
            if n == 0 {
                break;
            }
            let io = c.process_new_packets().map_err(|e| {
                // The rustls detail is otherwise lost; a queued fatal alert drains best-effort.
                log::warn!("TLS record processing failed: {e}");
                RpcError::Protocol("TLS record processing failed")
            })?;
            if io.plaintext_bytes_to_read() > 0 {
                pending.extend_from_slice(src);
                break;
            }
        }
        Ok(())
    }
}

impl RpcTransport for TlsTransport {
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
        // R34 framing over a `Write` adapter: bytes identical to every other stream backend.
        write_frame(&mut RawIo(self), buf)
    }

    fn recv_frame(&self) -> RpcResult<Vec<u8>> {
        read_frame(&mut RawIo(self))
    }

    fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
        self.send_records(buf, None)
    }

    fn send_raw_draining(
        &self,
        buf: &[u8],
        fds: &[std::os::fd::BorrowedFd<'_>],
        drain: &mut dyn FnMut() -> RpcResult<()>,
    ) -> RpcResult<()> {
        match self.stream.socket() {
            Some(sock) if fds.is_empty() => self.send_records(buf, Some((sock, drain))),
            // A stream with no socket sends without reading; `fds` meets the refusing default.
            _ => self.send_raw_with_fds(buf, fds),
        }
    }

    /// Single-reader: one thread drives `recv_*` per connection (the RPC
    /// serve loop / the in-flight transact's reply wait). Concurrent
    /// multi-reader would interleave `pump_incoming` socket reads and
    /// corrupt the ciphertext stream — not a supported call pattern (the
    /// trait contract is one sender thread + one receiver thread).
    fn recv_raw(&self, out: &mut [u8]) -> RpcResult<usize> {
        loop {
            // 1. Drain decrypted plaintext (crypto lock only; reads emit no wire bytes).
            {
                let mut c = self.conn.lock().expect("tls conn poisoned");
                match c.reader().read(out) {
                    Ok(n) if n > 0 => return Ok(n),
                    Ok(_) => return Ok(0), // close_notify received, all drained
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    // TCP EOF without close_notify: a cut stream on this backend.
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        // Our own shutdown yields exactly this: a close, not a cut.
                        if self.shut.load(std::sync::atomic::Ordering::SeqCst) {
                            return Ok(0);
                        }
                        log::warn!("TLS stream ended without close_notify ({})", self.desc);
                        return Err(RpcError::UncleanEndOfStream);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            // 2. Flush control-plane output rustls queued (non-blocking on `wlock`).
            if let Err(e) = self.flush_control() {
                if self.shut.load(std::sync::atomic::Ordering::SeqCst) {
                    // Our shutdown broke the write half; its expiry too is the end (module doc).
                    let deadline = matches!(&e, RpcError::Io(io) if super::is_timeout(io));
                    return Err(if deadline { RpcError::EndOfStream } else { e });
                }
                // Outbound half lost: a boundary-preserving error would read as our own deadline.
                log::warn!(
                    "TLS control flush failed, outbound half lost: {e} ({})",
                    self.desc
                );
                return Err(RpcError::UncleanEndOfStream);
            }
            // 3. Parked ciphertext, else block on the socket; on EOF rustls knows, step 1 decides.
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

    fn set_liveness(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        self.stream.set_liveness(timeout)?;
        Ok(())
    }

    // The socket's FIN, not `close_notify`: a buffered alert is still an unread byte.
    fn peer_closed(&self) -> Option<bool> {
        self.stream.peer_closed()
    }

    fn shutdown(&self) -> RpcResult<()> {
        // Refuse new sends; only the one already holding `wlock` remains to wait for.
        self.shut.store(true, std::sync::atomic::Ordering::SeqCst);
        // Teardown has no other way out, so bound the remaining writes (module doc "Shutdown").
        let _ = self.stream.set_write_timeout(Some(CLOSE_NOTIFY_TIMEOUT));
        // Hold `wlock` through the cut: the alert follows a whole frame and none slips in after.
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
        // `None`: sender parked on a stalled peer; the cut unparks it (`EPIPE`), its frame fails.
        let cut = super::absorb_already_shut(self.stream.shutdown_stream());
        drop(held);
        cut
    }
}

/// R34 framing over raw TLS I/O, so [`write_frame`]/[`read_frame`] emit identical bytes.
struct RawIo<'a>(&'a TlsTransport);
impl Read for RawIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.0.recv_raw(buf) {
            Ok(n) => Ok(n),
            // Keep the io kind so `read_header` handles timeouts and clean close as R34 does.
            Err(RpcError::Io(e)) => Err(e),
            // `EAGAIN`'s kind, so `read_header`'s `is_timeout` fires (`DeadlineMidFrame` split).
            Err(RpcError::Timeout) => Err(std::io::ErrorKind::WouldBlock.into()),
            Err(RpcError::EndOfStream) => Ok(0),
            // Carried as the payload so `From<io::Error>` hands it back past `read_header`.
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
        // Kind-preserving: a closed-peer write must reach `write_frame` as `EndOfStream`.
        self.0.send_raw(buf).map_err(std::io::Error::from)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(()) // send_raw already flushes the socket
    }
}
