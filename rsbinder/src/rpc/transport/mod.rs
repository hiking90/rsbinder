// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Transport abstraction for the RPC stack.
//!
//! A [`RpcTransport`] carries **length-framed byte messages** for one
//! RPC connection and reports the [`PeerIdentity`] of the other end.
//! The implementation *defines the trust boundary*: a `unix` socket
//! trusts filesystem permissions + `SO_PEERCRED`; `vsock` trusts
//! hypervisor VM isolation; `tls` trusts a certificate; the gated
//! `tcp_debug` backend trusts **nothing** and is debug/interop only.
//!
//! Framing is the transport's responsibility (not the wire codec's), so
//! the wire layer can think purely in whole messages. Stream backends
//! (`unix`, `tcp_debug`) share the length-prefix helpers in this
//! module; the in-process `mem` backend frames implicitly (one channel
//! message == one frame).
//!
//! The shared stream frame is `u32 little-endian length | <length> body
//! bytes`, with no magic and no self-sync: the length alone delimits the
//! frame and is bounded by `MAX_FRAME_LEN` before allocation. Frames do not
//! interleave because a connection has one writer at a time: every send,
//! a `DEC_STRONG` from `RpcProxy::drop` included, goes out on a slot its
//! thread holds (`session.rs` `ConnGuard`, `exclusive_tid`), so a frame
//! written in more than one call stays whole. `unix` and `tcp_debug` send
//! the length and the body as two slices of one `sendmsg`
//! (`unix::send_frame_vectored`). `tls` and `vsock` coalesce them into one
//! `write_all` (`write_frame`): over `tls` two writes would let a
//! `close_notify` from `shutdown` land between them, and `vsock` has no
//! vectored send path here.
//!
//! The trait is **synchronous / blocking** (matches android-12 r34's
//! blocking-thread model). An `async` adapter can be layered *on top*
//! without changing this trait.
//!
//! # Short reads and writes
//!
//! A frame header read that sees EOF before any byte is a clean
//! [`RpcError::EndOfStream`]; a partial header then EOF is
//! [`RpcError::Truncated`]. A transport that can tell an unclean end apart
//! ([`RpcError::UncleanEndOfStream`], TLS with no `close_notify`) reports it
//! as itself before any progress and as `Truncated` after: mid-frame, the
//! stream position is what is lost. Once the header is committed, any short
//! body read has lost the position too: `Truncated` if the stream ended,
//! [`RpcError::DeadlineMidFrame`] if a read deadline cut it.
//!
//! `write_all_reporting` is `write_all` that tells a failure which put
//! nothing on the wire apart from one that stopped part-way; `write_all`
//! reports the same error either way. A frame that stopped part-way left the
//! peer a header it will complete out of whatever arrives next, while a frame
//! that never started left the stream frame-synchronized. A send deadline
//! (`SO_SNDTIMEO`) that expires before the first byte is therefore
//! [`RpcError::Timeout`] (the value a read deadline that consumed nothing
//! already yields), and every other failure, at any position, stays the
//! transport error. The session ends on both (its "Failed sends" rule: the
//! peer did not read for the whole deadline either way); the variant keeps
//! saying which position the stream was left in. The writer must report
//! partial progress honestly, as a socket does. An adapter that hands the
//! whole buffer to another
//! all-or-nothing send does not, and classifies at its own level instead:
//! `tls` never reports this, because a record its socket write dropped is
//! gone from the sequence whether or not a byte of it went out.
//!
//! `is_timeout` decides whether an I/O operation failed on a deadline this
//! end armed: `SO_RCVTIMEO` for the framing readers, `SO_SNDTIMEO` for
//! `write_all_reporting` and for `tls`'s teardown control flush. On the
//! supported platforms both surface as `EAGAIN` (`WouldBlock`), and the
//! `Read`/`Write` adapters carry `RpcError::Timeout` across the
//! `RpcError` ⇄ `io::Error` boundary with that same kind, so the framing
//! readers see one kind whichever layer they sit on. `TimedOut` is not a
//! deadline: it is the kernel's own `ETIMEDOUT`, TCP keepalive or
//! retransmission giving up on a peer whose host stopped answering. That
//! connection is gone, so the error stays [`RpcError::Io`], never a
//! frame-synchronized `Timeout`. A transport of the caller's own follows the
//! same split: `Timeout` only for a deadline of this end's that consumed
//! nothing, any loss of the connection as another variant.
//!
//! # Mutation gates
//!
//! - `a_send_deadline_is_a_timeout_only_before_the_first_byte`: dropping the
//!   `sent == 0` guard in `write_all_reporting` makes the second case report
//!   `Timeout` as well, a frame-synchronized stream the peer was left half a
//!   frame of.
//! - `the_kernels_etimedout_is_a_lost_connection_not_a_deadline`: counting
//!   `TimedOut` in `is_timeout` makes a send that failed before its
//!   first byte and a read that consumed nothing report `Timeout`: a serve
//!   loop with an idle deadline armed reads the kernel's drop as its own
//!   idle expiry (`Local`, `InSync`).

use std::fmt;
use std::io::{ErrorKind, Read, Write};

use super::{RpcError, RpcResult};

mod mem;
#[cfg(feature = "rpc-tcp-debug")]
mod tcp_debug;
#[cfg(feature = "rpc-tls")]
mod tls;
pub(crate) mod unix;
#[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
mod vsock;

pub use mem::MemTransport;
#[cfg(feature = "rpc-tcp-debug")]
pub use tcp_debug::{insecure_warning_emitted, TcpDebugTransport};
#[cfg(feature = "rpc-tls")]
pub use tls::{TlsStream, TlsTransport};
pub use unix::UnixTransport;
#[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
pub use vsock::VsockTransport;

/// Hard cap on a single decoded frame.
///
/// A length header declaring more than this is rejected **before any
/// allocation** — an adversarial peer cannot trigger an OOM by claiming
/// a huge body. 64 MiB is far above any legitimate binder transaction
/// yet bounded.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

/// One RPC connection: framed byte transport + peer identity.
///
/// Synchronous and blocking. `&self` (not `&mut self`) so a session can
/// hold one transport and use it from a sender thread and a receiver
/// thread concurrently — full-duplex sockets and the `mem` channel pair
/// both support that without a deadlock. Implementations must keep
/// `send_frame`/`recv_frame` independently callable from two threads.
pub trait RpcTransport: Send + Sync {
    /// Send exactly one logical frame. The implementation guarantees
    /// framing (length prefix or channel message boundary).
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()>;

    /// Receive exactly one logical frame.
    ///
    /// A clean peer close with nothing pending is
    /// [`RpcError::EndOfStream`]; a header received but body short is
    /// [`RpcError::Truncated`]. Never panics or loops forever on a
    /// hostile peer.
    fn recv_frame(&self) -> RpcResult<Vec<u8>>;

    /// The other end's identity, as established by this transport.
    ///
    /// This is the RPC equivalent of kernel binder's
    /// `getCallingUid()`/SELinux context — **but only as strong as the
    /// transport's trust boundary**. [`PeerIdentity::Anonymous`] means
    /// no identity at all (no ACL possible); callers must log it as
    /// such and not grant trust.
    fn peer_identity(&self) -> PeerIdentity;

    /// Short human-readable description for diagnostics/logging
    /// (e.g. socket path, `"mem"`, vsock cid). Never carries secrets.
    fn describe(&self) -> &str;

    /// Set a read deadline for subsequent [`RpcTransport::recv_frame`]
    /// calls. `None` clears it (fully blocking). The
    /// default is a no-op for backends with no read-timeout notion;
    /// `unix` / `mem` / `tcp_debug` / `vsock` / `tls` override it. A deadline that
    /// elapses with **nothing consumed** surfaces as
    /// [`RpcError::Timeout`] (the stream stays frame-synchronized); a
    /// deadline that elapses mid-frame is [`RpcError::DeadlineMidFrame`].
    /// Neither variant is for anything but this deadline: a connection the
    /// platform gave up on (the kernel's `ETIMEDOUT`) is lost, and reports
    /// as [`RpcError::Io`] or an end of stream.
    fn set_read_timeout(&self, _timeout: Option<std::time::Duration>) -> RpcResult<()> {
        Ok(())
    }

    /// Set a write deadline for subsequent sends. `None` clears it. The
    /// default is a no-op for backends with no write-timeout notion
    /// (`mem`'s send never blocks on a peer); socket-backed transports
    /// (`unix` / `vsock` / `tcp_debug` / `tls`) override it. This bounds
    /// the reply-send phase so a peer that completes the handshake/
    /// admission and then stops reading cannot pin its worker thread (and,
    /// under [`set_max_connections`](super::server::RpcServer::set_max_connections),
    /// its admission slot) forever by stalling our blocking `write_all`
    /// once the kernel send buffer fills.
    ///
    /// A transport that keeps the default gives a stalled send no bound at
    /// all. A server session counts a frame being written as activity
    /// until the write returns
    /// ([`set_idle_timeout`](super::server::RpcServer::set_idle_timeout)),
    /// so its idle judgment cannot end a session stuck in such a send; a
    /// transport handed to a server with an idle timeout implements this.
    fn set_write_timeout(&self, _timeout: Option<std::time::Duration>) -> RpcResult<()> {
        Ok(())
    }

    /// Arm the kernel's own check that the peer's host still answers, sized
    /// to `timeout` — the session's
    /// [`set_timeout`](super::RpcSession::set_timeout), which calls this on
    /// every connection. The default is a no-op: `unix`, `vsock` and `mem`
    /// have no such check, and a caller's own transport gets the default
    /// until it overrides this. `tcp_debug` and `tls` over TCP implement it.
    /// This is the one place the crate states the values and what the
    /// kernel does with them; every other document points here.
    ///
    /// # What is set
    ///
    /// - `SO_KEEPALIVE` on, whatever `timeout` is.
    /// - With `Some(d)`: `TCP_KEEPIDLE` = `d / 2` and `TCP_KEEPINTVL` =
    ///   `d / 6`, each in whole seconds rounded down and then held to
    ///   1..=32767 s (the options' unit, and Linux's `MAX_TCP_KEEPIDLE` /
    ///   `MAX_TCP_KEEPINTVL`, past which `setsockopt` fails with `EINVAL`);
    ///   `TCP_KEEPCNT` = 3; and on Linux and Android only, `TCP_USER_TIMEOUT`
    ///   = `d` in milliseconds, held to 1..=`i32::MAX`.
    /// - With `None`: on Linux and Android `TCP_USER_TIMEOUT` = 0, the
    ///   kernel default. The probe options are not touched, so after an
    ///   earlier `Some(d)` they keep that call's values: no socket option
    ///   restores the system's defaults.
    ///
    /// # Linux and Android
    ///
    /// Read from `net/ipv4/tcp_timer.c` (kernel `android17-6.18`):
    ///
    /// - **A quiet connection** (nothing unacknowledged, nothing queued):
    ///   `tcp_keepalive_timer` sends the first probe once nothing has come
    ///   from the peer's host for `TCP_KEEPIDLE`, then one per
    ///   `TCP_KEEPINTVL`; any segment from the peer, a probe's ACK included,
    ///   restarts the count. With `TCP_USER_TIMEOUT` set, `TCP_KEEPCNT` is
    ///   ignored and the connection is reset at the first timer run that
    ///   finds a probe out and nothing received for `TCP_USER_TIMEOUT`: for
    ///   `Some(d)` about `d` after the host went silent, later by at most one
    ///   interval since the check runs on the probe schedule; for a `d` under
    ///   a second, at `TCP_KEEPIDLE + TCP_KEEPINTVL` (2 s), since both floor
    ///   at 1 s and the first run only sends a probe. Without it the
    ///   reset follows the `TCP_KEEPCNT`-th unanswered probe. The system
    ///   defaults (`include/net/tcp.h`) are 7200 s, 75 s and 9 probes, so a
    ///   `None` connection whose peer's host vanished ends after about two
    ///   hours.
    /// - **Data sent and not acknowledged**: keepalive does not run while
    ///   there is any (`tcp_keepalive_timer` skips a socket with packets out
    ///   or a non-empty write queue); retransmission does. `tcp_write_timeout`
    ///   ends the connection once `TCP_USER_TIMEOUT` has passed since the
    ///   oldest unacknowledged segment first went out, and
    ///   `tcp_clamp_rto_to_user_timeout` makes the last retransmission timer
    ///   fire at that point. With `TCP_USER_TIMEOUT` = 0 the bound is
    ///   `net.ipv4.tcp_retries2` (15 by default, about 925 s).
    /// - **A zero receive window**: when the peer's host answers every probe
    ///   but keeps its window closed while this end has data queued,
    ///   `tcp_probe_timer` ends the connection once `TCP_USER_TIMEOUT` has
    ///   passed since the first window probe; with 0 it never does.
    ///
    /// The end reads and writes as `ETIMEDOUT` (or an ICMP error the socket
    /// recorded before, such as `EHOSTUNREACH`), an [`RpcError::Io`], which
    /// ends the session.
    ///
    /// # macOS
    ///
    /// The same keepalive options are set (`TCP_KEEPALIVE` is the idle
    /// option's name there) and no `TCP_USER_TIMEOUT`, which the platform
    /// lacks. How the macOS kernel acts on them is not stated here, and
    /// unacknowledged data is left to the system's retransmission limit, not
    /// to `d`.
    ///
    /// # Limits
    ///
    /// The session's send deadline is separate: it sets `SO_SNDTIMEO` through
    /// [`set_write_timeout`](Self::set_write_timeout), which bounds only a
    /// send blocked on a full send buffer, never data already accepted into
    /// it. The probes reach only the first TCP endpoint on the path; a relay
    /// that ends TCP (`adb forward`, `ssh -L`, a TLS terminator) answers them
    /// itself, so a break behind it goes unnoticed here.
    fn set_liveness(&self, _timeout: Option<std::time::Duration>) -> RpcResult<()> {
        Ok(())
    }

    /// Whether the peer has closed this connection, judged from the socket
    /// without reading it: `Some(true)` closed, `Some(false)` open as far as
    /// this end's kernel knows, `None` when the transport cannot tell. `None`
    /// is the default and means "unknown", never "open".
    ///
    /// A session with no incoming connection and no serve loop reads its
    /// connection only during a call, so a peer that left between calls goes
    /// unnoticed until the next one fails. The reconnect helper asks this
    /// before running a call on such a session: a closed peer found here means
    /// nothing has been sent yet, so the call can go to a new session instead.
    ///
    /// Bytes waiting to be read do not count either way. The bundled socket
    /// transports answer on Linux and Android from `POLLRDHUP` (the peer's FIN
    /// or reset, even behind unread data), and on Apple platforms from
    /// `POLLHUP`, which XNU sets on the same events for TCP and on a peer's
    /// close for Unix-domain sockets. On other systems Unix-domain sockets
    /// answer from `POLLHUP` and TCP is `None`: what their `poll` reports for
    /// a FIN has not been measured. A custom transport overrides this to take
    /// part.
    fn peer_closed(&self) -> Option<bool> {
        None
    }

    /// Shut the connection down in both directions: wake a reader blocked
    /// in [`recv_frame`](Self::recv_frame) / [`recv_raw`](Self::recv_raw)
    /// and make this end's later sends fail. (The *peer's* sends are the
    /// platform's business — a Linux `AF_UNIX` peer gets `EPIPE` at once,
    /// a macOS one has its writes accepted and discarded, a TCP peer on
    /// either sees the reset on a later write; it reads the end of stream
    /// either way.)
    /// This is how a session ends its
    /// connections — `RpcSession::close_session`, `RpcServer::terminate`, a
    /// fault on any of its connections. Required, with no default on
    /// purpose: a transport that silently did nothing here would leave a
    /// serve loop or an incoming-connection thread parked in `recv`
    /// forever, and `RpcSession::close_session` would hang on the join. The
    /// same reasoning already made `TlsStream::shutdown_stream` required
    /// (the `rpc-tls` backend).
    ///
    /// The contract is narrower than the name suggests, and every caller
    /// in rsbinder is written to it (plan 2-21 §3.4):
    ///
    /// - **What happens to bytes already received is platform- and
    ///   backend-dependent, and nothing may assume it.** A kernel may keep
    ///   its receive queue across a local shutdown — Linux: a reader gets
    ///   the queued bytes, then end of stream — or drop it (macOS). A
    ///   transport's own buffer may survive (`tls`'s decrypted plaintext)
    ///   or be cleared (`unix` fd-mode clears its leftover here). A caller
    ///   that must not read what is buffered keeps that decision in
    ///   session state — the session's end empties the slot pool — not
    ///   here. `mem`
    ///   models the Linux behaviour, so a hermetic test exercises the case
    ///   that hides bugs.
    /// - **A reader woken by this returns the end of stream** —
    ///   [`RpcError::EndOfStream`] at a frame boundary, [`RpcError::Truncated`]
    ///   mid-frame — never a distinct "shut down locally" error. Who ended
    ///   the connection is session knowledge (`SessionEnd::by`), not
    ///   transport knowledge; a TLS transport does not report its own
    ///   shutdown as an unclean end.
    /// - **Idempotent.** A second call returns `Ok(())`; the socket
    ///   backends absorb the `ENOTCONN` a second shutdown raises on macOS.
    /// - **The `Err` is diagnostic.** The connection is being ended
    ///   whatever it says; callers log it and never branch on it.
    /// - **This is not `close`.** The kernel's queue is certainly gone only
    ///   when the last `Arc<dyn RpcTransport>` drops.
    ///
    /// The measured behaviour of `unix` (both modes), `mem`, `tcp_debug`
    /// and `tls` on both platforms is pinned by
    /// `tests/rpc_transport_conformance.rs`. `vsock` is not measured: it
    /// needs a VM peer, so its behaviour here is inferred from `vsock(7)`
    /// and covered only by its own `#[ignore]`d tests.
    fn shutdown(&self) -> RpcResult<()>;

    /// Whether this transport can actually carry file descriptors.
    ///
    /// Default `false`, matching the fd-rejecting defaults of
    /// [`send_frame_with_fds`](Self::send_frame_with_fds) /
    /// [`send_raw_with_fds`](Self::send_raw_with_fds): a backend that
    /// does not override those cannot pass an fd whatever fd mode the
    /// session negotiated, and `SCM_RIGHTS` is `unix` only.
    ///
    /// **An implementor that overrides the fd send/recv methods must
    /// override this too.** The two are separate switches: leaving this
    /// at `false` while the sends work makes the session report no
    /// [`FD_PASSING`](crate::TransportCaps::FD_PASSING), and a caller
    /// that branches on that bit gives up a path that would have
    /// worked. The reverse — `true` with the defaults in place — trips
    /// a `debug_assert` on the first fd send.
    ///
    /// A session reads this off its founding connection and refuses a
    /// later connection that answers differently.
    fn supports_fd_passing(&self) -> bool {
        false
    }

    /// Send one frame plus passed file descriptors out-of-band (opt-in
    /// `FileDescriptorTransportMode::Unix`).
    ///
    /// The **default rejects any fd** — so `mem`/`vsock`/`tls` are
    /// fd-incapable *by type*, with no extra code. Only `unix` overrides
    /// this with `SCM_RIGHTS`. An empty `fds` slice falls back to the
    /// plain framed send.
    fn send_frame_with_fds(
        &self,
        buf: &[u8],
        fds: &[std::os::fd::BorrowedFd<'_>],
    ) -> RpcResult<()> {
        if fds.is_empty() {
            self.send_frame(buf)
        } else {
            // A `true` predicate here would advertise FD_PASSING for a send that always fails.
            debug_assert!(!self.supports_fd_passing());
            Err(RpcError::Protocol(
                "this transport cannot pass file descriptors (UDS only)",
            ))
        }
    }

    /// Receive one frame plus any out-of-band file descriptors.
    /// Default: never yields fds (the plain framed recv); only `unix`
    /// overrides with `SCM_RIGHTS`.
    fn recv_frame_with_fds(&self) -> RpcResult<(Vec<u8>, Vec<std::os::fd::OwnedFd>)> {
        Ok((self.recv_frame()?, Vec::new()))
    }

    /// Send raw bytes with **no framing**. The real android RPC wire
    /// has no length prefix (`RpcState::rpcSend` writes the
    /// `RpcWireHeader` + body directly) — the android-13+ profile drives
    /// framing itself via `wire_android13`. The default is
    /// **unsupported**: right for a frame-only backend (`mem`), and a
    /// silent trap for a byte-stream one — an android-13+ session over a
    /// backend that does not override it fails at its first handshake
    /// byte. Every stream backend (`unix`, `tcp_debug`, `vsock`, `tls`)
    /// must override both this and [`recv_raw`](Self::recv_raw). The R34
    /// path never calls this; it uses `send_frame`/`recv_frame`.
    fn send_raw(&self, _buf: &[u8]) -> RpcResult<()> {
        Err(RpcError::Protocol("this transport has no raw byte access"))
    }

    /// Read up to `buf.len()` raw bytes (one `read`; `Ok(0)` = peer
    /// closed). Pairs with [`RpcTransport::send_raw`]. Default:
    /// unsupported (see [`RpcTransport::send_raw`]).
    fn recv_raw(&self, _buf: &mut [u8]) -> RpcResult<usize> {
        Err(RpcError::Protocol("this transport has no raw byte access"))
    }

    /// Send raw bytes with **no framing**, passing `fds` out-of-band via
    /// `SCM_RIGHTS` (the android-13+ v1+ `Unix` FD-over-RPC path). This
    /// is [`RpcTransport::send_raw`] + the ancillary channel of
    /// [`RpcTransport::send_frame_with_fds`],
    /// minus the length prefix: the real android RPC wire has none
    /// (`RpcWireHeader.bodySize` is authoritative) and AOSP rides the
    /// fds on the **first** `sendmsg` of the message
    /// (`RpcTransportRaw::interruptableWriteFully`, `sentFds`). An empty
    /// `fds` slice is exactly [`RpcTransport::send_raw`]. Default:
    /// unsupported unless `fds` is empty (frame-only transports stay
    /// fd-incapable *by type*, no extra code); only `unix` overrides.
    fn send_raw_with_fds(&self, buf: &[u8], fds: &[std::os::fd::BorrowedFd<'_>]) -> RpcResult<()> {
        if fds.is_empty() {
            self.send_raw(buf)
        } else {
            debug_assert!(!self.supports_fd_passing());
            Err(RpcError::Protocol(
                "this transport cannot pass file descriptors (UDS only)",
            ))
        }
    }

    /// Read up to `buf.len()` raw bytes (one `recvmsg`; `Ok((0, _))` =
    /// peer closed) plus any `SCM_RIGHTS` fds delivered with them.
    /// Pairs with
    /// [`RpcTransport::send_raw_with_fds`]; received fds are
    /// `O_CLOEXEC`. AOSP accumulates ancillary fds across the
    /// `recvmsg`s that read one message
    /// (`RpcTransportRaw::interruptableReadFully`), so the caller
    /// gathers fds across the header+body reads. Default: never yields
    /// fds (plain [`RpcTransport::recv_raw`]); only `unix` overrides.
    fn recv_raw_with_fds(&self, buf: &mut [u8]) -> RpcResult<(usize, Vec<std::os::fd::OwnedFd>)> {
        Ok((self.recv_raw(buf)?, Vec::new()))
    }
}

/// Identity of the peer on the other end of a [`RpcTransport`].
///
/// `#[non_exhaustive]`: matching code must keep a wildcard arm, since
/// further variants may be added.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerIdentity {
    /// A local peer whose credentials the kernel vouches for
    /// (`SO_PEERCRED` over a Unix domain socket, or the current
    /// process for the in-memory test transport).
    Local {
        /// Peer process effective UID.
        uid: u32,
        /// Peer process PID (`-1` if unavailable on this platform).
        pid: i32,
    },
    /// A vsock peer, identified by its context id. **Not an ACL
    /// basis** — `cid` is a routing address, and the trust boundary is
    /// hypervisor VM isolation. Logged with the cid.
    Vsock {
        /// vsock context id of the peer VM/host.
        cid: u32,
    },
    /// A TLS peer authenticated by its leaf certificate. The trust
    /// boundary is the certificate chain.
    Certificate(CertId),
    /// No identity is available. **ACL is not possible** against an
    /// anonymous peer; this must be surfaced in logs and never treated
    /// as trusted. Used by the debug-only plaintext TCP backend.
    Anonymous,
}

/// Identity extracted from a peer's TLS leaf certificate. Carries the
/// subject and a SHA-256 fingerprint; ACL is the caller's
/// responsibility on top of this.
#[derive(Clone, PartialEq, Eq)]
pub struct CertId {
    subject: String,
    fingerprint: [u8; 32],
}

impl CertId {
    /// Construct from a subject string and the leaf cert SHA-256.
    pub fn new(subject: impl Into<String>, fingerprint: [u8; 32]) -> Self {
        CertId {
            subject: subject.into(),
            fingerprint,
        }
    }
    /// The certificate subject (DN / SAN summary).
    pub fn subject(&self) -> &str {
        &self.subject
    }
    /// The leaf certificate SHA-256 fingerprint.
    pub fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }
    /// Lowercase hex of the fingerprint (for logging / pinning).
    pub fn fingerprint_hex(&self) -> String {
        self.fingerprint
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

impl fmt::Debug for CertId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertId")
            .field("subject", &self.subject)
            .field("fingerprint", &self.fingerprint_hex())
            .finish()
    }
}

impl PeerIdentity {
    /// `true` for [`PeerIdentity::Local`].
    pub fn is_local(&self) -> bool {
        matches!(self, PeerIdentity::Local { .. })
    }

    /// Peer UID, if this identity carries one.
    pub fn uid(&self) -> Option<u32> {
        match self {
            PeerIdentity::Local { uid, .. } => Some(*uid),
            _ => None,
        }
    }

    /// Peer PID, if this identity carries a meaningful one
    /// (`Some(-1)` is filtered to `None`).
    pub fn pid(&self) -> Option<i32> {
        match self {
            PeerIdentity::Local { pid, .. } if *pid >= 0 => Some(*pid),
            _ => None,
        }
    }
}

impl fmt::Display for PeerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeerIdentity::Local { uid, pid } => write!(f, "local(uid={uid}, pid={pid})"),
            PeerIdentity::Vsock { cid } => {
                write!(f, "vsock(cid={cid}; routing only, NOT an ACL basis)")
            }
            PeerIdentity::Certificate(c) => {
                write!(
                    f,
                    "cert(subject={:?}, sha256={})",
                    c.subject(),
                    c.fingerprint_hex()
                )
            }
            // Make the security-relevant "no identity" state loud.
            PeerIdentity::Anonymous => {
                write!(
                    f,
                    "anonymous(NO peer identity — access control NOT possible)"
                )
            }
        }
    }
}

// --- Length-prefix framing shared by stream backends (module doc) ---

/// Write one length-prefixed frame to a blocking stream.
#[cfg(any(
    test,
    feature = "rpc-tls",
    all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android"))
))]
pub(crate) fn write_frame<W: Write>(w: &mut W, buf: &[u8]) -> RpcResult<()> {
    if buf.len() > MAX_FRAME_LEN {
        return Err(RpcError::FrameTooLarge {
            declared: buf.len(),
            max: MAX_FRAME_LEN,
        });
    }
    // One buffer, one `write_all`; the slot's single writer keeps the frame whole (module doc).
    let mut framed = Vec::with_capacity(4 + buf.len());
    framed.extend_from_slice(&(buf.len() as u32).to_le_bytes());
    framed.extend_from_slice(buf);
    w.write_all(&framed)?;
    w.flush()?;
    Ok(())
}

/// `write_all`, `Timeout` only before the first byte; see module doc "Short reads and writes".
pub(crate) fn write_all_reporting<W: Write>(w: &mut W, buf: &[u8]) -> RpcResult<()> {
    let mut sent = 0;
    while sent < buf.len() {
        match w.write(&buf[sent..]) {
            Ok(0) => return Err(std::io::Error::from(ErrorKind::WriteZero).into()),
            Ok(n) => sent += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if sent == 0 && is_timeout(&e) => return Err(RpcError::Timeout),
            Err(e) => return Err(RpcError::from(e)),
        }
    }
    Ok(())
}

/// Read exactly `buf.len()` header bytes; see module doc "Short reads and writes".
fn read_header<R: Read>(r: &mut R, buf: &mut [u8]) -> RpcResult<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(if filled == 0 {
                    RpcError::EndOfStream
                } else {
                    RpcError::Truncated
                });
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            // Nothing consumed: clean Timeout (still frame-synchronized); mid-header: desync.
            Err(e) if is_timeout(&e) => {
                return Err(if filled == 0 {
                    RpcError::Timeout
                } else {
                    RpcError::DeadlineMidFrame
                });
            }
            Err(e) if e.kind() == ErrorKind::UnexpectedEof && filled > 0 => {
                return Err(RpcError::Truncated);
            }
            // Past the first byte any disconnect, even one folded to `EndOfStream`, lost position.
            Err(e) => {
                return Err(match RpcError::from(e) {
                    RpcError::EndOfStream if filled > 0 => RpcError::Truncated,
                    other => other,
                })
            }
        }
    }
    Ok(())
}

/// A deadline this end armed (`WouldBlock`; `TimedOut` is the kernel's); see module doc.
pub(crate) fn is_timeout(e: &std::io::Error) -> bool {
    e.kind() == ErrorKind::WouldBlock
}

/// `TCP_KEEPCNT`; `RpcTransport::set_liveness` says where it decides the verdict.
#[cfg(any(feature = "rpc-tcp-debug", feature = "rpc-tls"))]
const KEEPALIVE_PROBES: u32 = 3;

/// Linux `MAX_TCP_KEEPIDLE` = `MAX_TCP_KEEPINTVL` (`include/net/tcp.h`); more is `EINVAL`.
#[cfg(any(feature = "rpc-tcp-debug", feature = "rpc-tls"))]
const MAX_KEEPALIVE_SECS: u64 = 32767;

/// Which socket family [`socket_peer_closed`] polls: the flag a peer's close sets differs.
#[derive(Clone, Copy)]
pub(crate) enum SocketKind {
    UnixDomain,
    // TCP and vsock: only their transports' features use it.
    #[cfg_attr(
        not(any(feature = "rpc-tcp-debug", feature = "rpc-tls", feature = "rpc-vsock")),
        allow(dead_code)
    )]
    TcpOrVsock,
}

/// `RpcTransport::peer_closed` for a stream socket; see that method's doc for the flags.
pub(crate) fn socket_peer_closed(
    fd: std::os::fd::BorrowedFd<'_>,
    kind: SocketKind,
) -> Option<bool> {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let (ask, closed) = {
        let _ = kind;
        (
            PollFlags::RDHUP,
            PollFlags::RDHUP | PollFlags::HUP | PollFlags::ERR,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let (ask, closed) = match kind {
        // XNU reports `POLLHUP` only from a read filter, which it registers only when asked.
        SocketKind::UnixDomain => (PollFlags::HUP, PollFlags::HUP | PollFlags::ERR),
        // XNU sets `POLLHUP` on a TCP FIN or reset as well; other systems are unmeasured.
        #[cfg(target_vendor = "apple")]
        SocketKind::TcpOrVsock => (PollFlags::HUP, PollFlags::HUP | PollFlags::ERR),
        #[cfg(not(target_vendor = "apple"))]
        SocketKind::TcpOrVsock => return None,
    };
    let mut fds = [PollFd::from_borrowed_fd(fd, ask)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    match poll(&mut fds, Some(&now)) {
        Ok(_) => Some(fds[0].revents().intersects(closed)),
        Err(_) => None,
    }
}

/// `TCP_KEEPIDLE` and `TCP_KEEPINTVL` for `d`, as `RpcTransport::set_liveness` states them.
#[cfg(any(feature = "rpc-tcp-debug", feature = "rpc-tls"))]
fn keepalive_intervals(d: std::time::Duration) -> (std::time::Duration, std::time::Duration) {
    let secs = |n: u64| std::time::Duration::from_secs(n.clamp(1, MAX_KEEPALIVE_SECS));
    (secs(d.as_secs() / 2), secs(d.as_secs() / 6))
}

/// `RpcTransport::set_liveness` for a TCP socket: see that method's doc for the values.
#[cfg(any(feature = "rpc-tcp-debug", feature = "rpc-tls"))]
pub(crate) fn tcp_liveness(
    fd: std::os::fd::BorrowedFd<'_>,
    timeout: Option<std::time::Duration>,
) -> std::io::Result<()> {
    use rustix::net::sockopt;
    // Every option is tried after a failure too, so none is left at an earlier call's value.
    let mut first_err: Option<std::io::Error> = None;
    let mut apply = |r: rustix::io::Result<()>| {
        if let Err(e) = r {
            first_err.get_or_insert(e.into());
        }
    };
    apply(sockopt::set_socket_keepalive(fd, true));
    // 0 is the kernel default; a positive deadline is at least 1 ms; the kernel reads an `int`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    apply(sockopt::set_tcp_user_timeout(
        fd,
        timeout.map_or(0, |d| {
            u32::try_from(d.as_millis())
                .unwrap_or(u32::MAX)
                .clamp(1, i32::MAX as u32)
        }),
    ));
    if let Some(d) = timeout {
        let (idle, interval) = keepalive_intervals(d);
        apply(sockopt::set_tcp_keepidle(fd, idle));
        apply(sockopt::set_tcp_keepintvl(fd, interval));
        apply(sockopt::set_tcp_keepcnt(fd, KEEPALIVE_PROBES));
    }
    first_err.map_or(Ok(()), Err)
}

/// Socket `shutdown` result, absorbing macOS's `ENOTCONN` on a second call (trait: idempotent).
pub(crate) fn absorb_already_shut(r: std::io::Result<()>) -> RpcResult<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotConnected => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Read exactly `buf.len()` body bytes; see module doc "Short reads and writes".
fn read_body<R: Read>(r: &mut R, buf: &mut [u8]) -> RpcResult<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Err(RpcError::Truncated),
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if is_timeout(&e) => return Err(RpcError::DeadlineMidFrame),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Err(RpcError::Truncated),
            // Header consumed: any disconnect, even one folded to `EndOfStream`, is a cut frame.
            Err(e) => {
                return Err(match RpcError::from(e) {
                    RpcError::EndOfStream => RpcError::Truncated,
                    other => other,
                })
            }
        }
    }
    Ok(())
}

/// Read one length-prefixed frame from a blocking stream.
pub(crate) fn read_frame<R: Read>(r: &mut R) -> RpcResult<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    read_header(r, &mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_LEN {
        // Reject *before* allocating `len` bytes.
        return Err(RpcError::FrameTooLarge {
            declared: len,
            max: MAX_FRAME_LEN,
        });
    }
    let mut body = vec![0u8; len];
    read_body(r, &mut body)?;
    Ok(body)
}

/// Decode-only entrypoint for the `rpc_frame_decode` fuzz target and
/// the deterministic adversarial-input regression tests. Feeds
/// arbitrary bytes through the same deframing path
/// `recv_frame` uses. `#[doc(hidden)]`: not part of the supported API
/// surface (and absent entirely without the `rpc` feature).
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub fn __fuzz_decode_frame(input: &[u8]) -> RpcResult<Vec<u8>> {
    read_frame(&mut std::io::Cursor::new(input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_over_cursor() {
        for size in [0usize, 1, 4, 64, 4096, 1 << 20] {
            let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut buf = Vec::new();
            write_frame(&mut buf, &payload).expect("write");
            let got = read_frame(&mut std::io::Cursor::new(&buf)).expect("read");
            assert_eq!(got, payload, "roundtrip mismatch at size {size}");
        }
    }

    #[test]
    fn two_frames_back_to_back_preserve_order() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"first").unwrap();
        write_frame(&mut buf, b"second").unwrap();
        let mut cur = std::io::Cursor::new(&buf);
        assert_eq!(read_frame(&mut cur).unwrap(), b"first");
        assert_eq!(read_frame(&mut cur).unwrap(), b"second");
    }

    /// Adversarial headers are rejected without allocating, panicking or looping (as the fuzzer).
    #[test]
    fn hostile_frame_headers_are_rejected_safely() {
        // Declared u32::MAX, no body: rejected pre-allocation.
        let huge = u32::MAX.to_le_bytes();
        assert!(matches!(
            __fuzz_decode_frame(&huge),
            Err(RpcError::FrameTooLarge { .. })
        ));

        // Declared MAX_FRAME_LEN + 1.
        let over = ((MAX_FRAME_LEN + 1) as u32).to_le_bytes();
        assert!(matches!(
            __fuzz_decode_frame(&over),
            Err(RpcError::FrameTooLarge { .. })
        ));

        // Empty input: clean peer-closed, no header at all.
        assert!(matches!(
            __fuzz_decode_frame(&[]),
            Err(RpcError::EndOfStream)
        ));

        // Partial header (2 of 4 bytes): truncated, not a panic.
        assert!(matches!(
            __fuzz_decode_frame(&[1, 0]),
            Err(RpcError::Truncated)
        ));

        // Header says 8 bytes, only 3 present: truncated body.
        let mut framed = 8u32.to_le_bytes().to_vec();
        framed.extend_from_slice(&[1, 2, 3]);
        assert!(matches!(
            __fuzz_decode_frame(&framed),
            Err(RpcError::Truncated)
        ));

        // A run of zero-length frames must not spin or panic.
        let zeros = vec![0u8; 4 * 1000];
        let mut cur = std::io::Cursor::new(&zeros[..]);
        for _ in 0..1000 {
            assert_eq!(read_frame(&mut cur).unwrap(), Vec::<u8>::new());
        }
    }

    #[test]
    fn write_frame_rejects_oversize_payload() {
        // `Trap` panics on any write: reaching it means the length guard did not fire.
        struct Trap;
        impl Write for Trap {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                panic!("oversize payload must be rejected before any write");
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let big = vec![0u8; MAX_FRAME_LEN + 1];
        assert!(matches!(
            write_frame(&mut Trap, &big),
            Err(RpcError::FrameTooLarge { .. })
        ));
    }

    /// `Timeout` (stream in step) only while nothing went out; see module doc "Mutation gates".
    #[test]
    fn a_send_deadline_is_a_timeout_only_before_the_first_byte() {
        struct Stall(usize);
        impl Write for Stall {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(std::io::Error::from(ErrorKind::WouldBlock));
                }
                let n = self.0.min(buf.len());
                self.0 -= n;
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        assert!(matches!(
            write_all_reporting(&mut Stall(0), b"frame"),
            Err(RpcError::Timeout)
        ));
        assert!(
            matches!(
                write_all_reporting(&mut Stall(2), b"frame"),
                Err(RpcError::Io(ref e)) if e.kind() == ErrorKind::WouldBlock
            ),
            "a deadline past the first byte left a partial frame on the wire"
        );
        assert!(write_all_reporting(&mut Stall(5), b"frame").is_ok());
    }

    /// `tcp_liveness`'s socket options, read back with `getsockopt`.
    #[cfg(any(feature = "rpc-tcp-debug", feature = "rpc-tls"))]
    #[test]
    fn tcp_liveness_sizes_keepalive_to_the_timeout() {
        use rustix::net::sockopt;
        use std::os::fd::AsFd;
        use std::time::Duration;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).expect("connect");
        let fd = stream.as_fd();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let user_timeout = || sockopt::tcp_user_timeout(fd).expect("TCP_USER_TIMEOUT");

        tcp_liveness(fd, None).expect("no timeout");
        assert!(
            sockopt::socket_keepalive(fd).unwrap(),
            "keepalive is on with no timeout"
        );
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(user_timeout(), 0, "the kernel default");

        tcp_liveness(fd, Some(Duration::from_secs(12))).expect("12 s");
        assert_eq!(sockopt::tcp_keepidle(fd).unwrap(), Duration::from_secs(6));
        assert_eq!(sockopt::tcp_keepintvl(fd).unwrap(), Duration::from_secs(2));
        assert_eq!(sockopt::tcp_keepcnt(fd).unwrap(), KEEPALIVE_PROBES);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(user_timeout(), 12_000);

        // Under two seconds the intervals floor at the socket option's one-second unit.
        tcp_liveness(fd, Some(Duration::from_millis(1500))).expect("1.5 s");
        assert_eq!(sockopt::tcp_keepidle(fd).unwrap(), Duration::from_secs(1));
        assert_eq!(sockopt::tcp_keepintvl(fd).unwrap(), Duration::from_secs(1));
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(user_timeout(), 1_500);

        // Past the kernel's caps each value saturates, rather than failing and keeping the last.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let month = Duration::from_secs(30 * 24 * 3600);
            tcp_liveness(fd, Some(month)).expect("30 days");
            let cap = Duration::from_secs(MAX_KEEPALIVE_SECS);
            assert_eq!(sockopt::tcp_keepidle(fd).unwrap(), cap);
            assert_eq!(sockopt::tcp_keepintvl(fd).unwrap(), cap);
            assert_eq!(user_timeout(), i32::MAX as u32);
        }

        tcp_liveness(fd, None).expect("back to none");
        assert!(sockopt::socket_keepalive(fd).unwrap());
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(
            user_timeout(),
            0,
            "None turns TCP_USER_TIMEOUT back to the default"
        );
    }

    /// `ETIMEDOUT` fails as `Io`, a read deadline as `Timeout`; see module doc "Mutation gates".
    #[test]
    fn the_kernels_etimedout_is_a_lost_connection_not_a_deadline() {
        struct Fails(ErrorKind);
        impl Read for Fails {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(self.0.into())
            }
        }
        impl Write for Fails {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(self.0.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let lost =
            |r: RpcResult<_>| matches!(r, Err(RpcError::Io(e)) if e.kind() == ErrorKind::TimedOut);

        let mut dead = Fails(ErrorKind::TimedOut);
        assert!(
            lost(write_all_reporting(&mut dead, b"frame")),
            "nothing went out, yet the connection is gone"
        );
        assert!(lost(read_frame(&mut dead).map(drop)));
        assert!(matches!(
            read_frame(&mut Fails(ErrorKind::WouldBlock)),
            Err(RpcError::Timeout)
        ));
    }

    #[test]
    fn peer_identity_display_and_accessors() {
        let local = PeerIdentity::Local { uid: 1000, pid: 42 };
        assert!(local.is_local());
        assert_eq!(local.uid(), Some(1000));
        assert_eq!(local.pid(), Some(42));
        assert_eq!(format!("{local}"), "local(uid=1000, pid=42)");

        let no_pid = PeerIdentity::Local { uid: 0, pid: -1 };
        assert_eq!(no_pid.pid(), None, "-1 pid is reported as unavailable");

        let anon = PeerIdentity::Anonymous;
        assert!(!anon.is_local());
        assert_eq!(anon.uid(), None);
        assert!(
            format!("{anon}").contains("NO peer identity"),
            "Anonymous Display must make the missing-identity state loud"
        );
    }
}
