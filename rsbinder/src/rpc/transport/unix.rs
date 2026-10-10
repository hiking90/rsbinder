// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Unix-domain-socket transport.
//!
//! Trust boundary: filesystem permissions on the socket path plus
//! `SO_PEERCRED`. Plaintext is *correct* here — the kernel is the trust
//! boundary (the original cross-domain bridge use case).
//!
//! Provides connected-stream wrapping, a `socketpair` constructor for
//! tests, and a `connect(path)` convenience.
//!
//! ## Peer identity
//!
//! Linux and Android read `SO_PEERCRED` (the peer's uid/pid). macOS and
//! the BSDs read `getpeereid` (the peer's effective uid, vouched by the
//! kernel at connect time) plus, on macOS only, `LOCAL_PEERPID` for the
//! pid; other BSDs have no pid option and report `-1`, the
//! [`PeerIdentity::Local`] "unavailable" value, while the `getpeereid` uid
//! still decides. This is the true peer for an accepted cross-process
//! socket and this process for a `socketpair` (both ends are us). A
//! `getpeereid` failure is never reported as a forged `Local`: see the
//! `ENOTCONN`/`EINVAL` rule below; any other errno yields
//! [`PeerIdentity::Anonymous`] with a warning, since no peer ACL is possible.
//!
//! Android takes the Linux `SO_PEERCRED` arm: bionic has no `getpeereid`,
//! and the BSD arm would pull in `libc::getpeereid` and break the
//! aarch64-linux-android build. `SO_PEERCRED` is read through libc rather
//! than `rustix::net::sockopt::socket_peercred`: rustix types the pid as
//! `Pid(NonZeroI32)` without checking it, but the kernel reports `pid == 0`
//! when the peer is not visible in our PID namespace (a container client on
//! a bind-mounted host socket), and that invalid `NonZeroI32` is UB before
//! the caller sees it.
//!
//! On macOS, `getpeereid` on a socket that is not `AF_UNIX` succeeds and
//! reports uid 0 (measured on Darwin 25: a TCP socket yields `rc = 0,
//! euid = 0`), so a foreign-family fd handed to `from_owned_fd` would mint
//! a root identity; the BSD path checks the address family first and
//! reports anything else as `Anonymous`. A `getpeereid` failure with
//! `ENOTCONN`/`EINVAL` means there is no remote peer — an unconnected fd,
//! or a socketpair on a BSD that does not populate peercred over the pipe
//! path — so the self identity is the non-forged answer there and the
//! hermetic socketpair test holds (defensive: a macOS socketpair does
//! populate peercred, so that branch is not reached on macOS).
//!
//! ## fd passing
//!
//! `recv_frame_with_fds` reads exact byte counts — the 4-byte header, then
//! the body straight into the frame's own buffer — so it never reads past
//! the last byte of the frame in progress. `AF_UNIX` on Linux glues stream
//! data across skbs and stops only after consuming the one that carried
//! fds, so a `recvmsg` spilling into the next frame would attach that
//! frame's `SCM_RIGHTS` fds to this one and leave the next frame with none.
//! The android-13+ reader (`wire_android13::read_aosp_message_with_fds`)
//! reads exact byte counts for the same reason.
//!
//! The reader keeps no bytes between frames: what an error cut short goes
//! with the error, which ends the session (`DeadlineMidFrame`, `Truncated`).
//! It holds `fd_recv_lock` for a whole frame, so two readers on one socket
//! cannot split a frame's fds between them. `shutdown` does not take that
//! lock: a reader parked in `recvmsg` holds it until the shutdown wakes it.
//!
//! The sender (`send_frame_vectored`) passes the length and the body as two
//! slices of one `sendmsg`, so the body is not copied into a joined buffer,
//! and attaches the fds to the first `sendmsg` only. It does not use std's
//! `write_vectored`: that is `writev`, which takes no `MSG_NOSIGNAL`. Apple
//! has no `MSG_NOSIGNAL` at all, so there `from_stream` sets `SO_NOSIGPIPE`
//! on the socket; std sets it only on sockets std itself creates, not on a
//! `socketpair` from `pair` or an fd adopted by `from_owned_fd`.
//!
//! ## Tests
//!
//! - `fd_mode_frames_queued_back_to_back_keep_their_own_fds`: a frame that
//!   takes several reads, with the next frame already queued, gets only its
//!   own fds. A reader that reads past the frame fails it on the Linux and
//!   Android kernels; on macOS the test passes either way (measured), so
//!   the macOS gate alone does not cover it.
//! - `unix_fd_mode_cut_frame_leaves_nothing_behind` (plan 2-21): after a
//!   deadline cuts a frame and `shutdown` runs, the reader reports the end
//!   of stream, not a frame decoded from the cut one. What the kernel still
//!   holds is pinned by `rpc_transport_conformance`, not here.
//! - `unix_mid_frame_deadline_is_our_own_cut` (plan 2-21): a read deadline
//!   part-way through a frame is this end's own cut (`DeadlineMidFrame`),
//!   told apart from a stream that ended mid-frame (`Truncated`); with
//!   nothing consumed it stays the boundary `Timeout`. The framed reader and
//!   the fd-mode reader classify the same way.
//! - `an_fd_send_to_a_closed_peer_is_an_error_not_a_signal`: the test
//!   harness ignores `SIGPIPE`, so the sends run in a child process that
//!   restores `SIG_DFL`; a `SIGPIPE` there kills the child and fails the test.

use std::io::Write;
#[cfg(target_os = "android")]
use std::os::android::net::SocketAddrExt;
use std::os::fd::{AsFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::linux::net::SocketAddrExt;
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::os::unix::net::SocketAddr as UnixSocketAddr;
use std::os::unix::net::UnixStream;
use std::path::Path;

use super::{queued_bytes, read_frame, PeerIdentity, ReadDeadline, RpcTransport, MAX_FRAME_LEN};
use crate::rpc::{RpcError, RpcResult};

/// Per-message fd cap (DoS bound, < `SCM_MAX_FD` 253); `wire_android13` applies it across recvmsgs.
pub(crate) const MAX_FDS_PER_FRAME: usize = 64;

/// A send to a closed peer is `EPIPE`, not `SIGPIPE`; Apple uses `SO_NOSIGPIPE` (module doc).
#[cfg(not(target_vendor = "apple"))]
pub(super) const SEND_FLAGS: rustix::net::SendFlags = rustix::net::SendFlags::NOSIGNAL;
#[cfg(target_vendor = "apple")]
pub(super) const SEND_FLAGS: rustix::net::SendFlags = rustix::net::SendFlags::empty();

/// Received fds are `O_CLOEXEC` atomically, as AOSP `OS_unix_base.cpp`; Apple sets it after.
#[cfg(not(target_vendor = "apple"))]
const RECV_FLAGS: rustix::net::RecvFlags = rustix::net::RecvFlags::CMSG_CLOEXEC;
#[cfg(target_vendor = "apple")]
const RECV_FLAGS: rustix::net::RecvFlags = rustix::net::RecvFlags::empty();

/// Ancillary space for one `sendmsg`/`recvmsg` of a frame: `MAX_FDS_PER_FRAME` fds.
const FD_SPACE: usize = rustix::cmsg_space!(ScmRights(MAX_FDS_PER_FRAME));

/// Unframed bytes via `send(2)` with `SEND_FLAGS`, classified as `write_all_reporting`.
pub(crate) fn send_raw_nosignal(sock: std::os::fd::BorrowedFd<'_>, buf: &[u8]) -> RpcResult<()> {
    struct NoSignal<'a>(std::os::fd::BorrowedFd<'a>);
    impl Write for NoSignal<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            rustix::net::send(self.0, buf, SEND_FLAGS).map_err(std::io::Error::from)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    super::write_all_reporting(&mut NoSignal(sock), buf)
}

/// One length-prefixed frame (module doc "fd passing"); rejects oversize or too many fds first.
pub(crate) fn send_frame_vectored(
    sock: std::os::fd::BorrowedFd<'_>,
    buf: &[u8],
    fds: &[std::os::fd::BorrowedFd<'_>],
) -> RpcResult<()> {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage};
    use std::io::IoSlice;
    use std::mem::MaybeUninit;

    if fds.len() > MAX_FDS_PER_FRAME {
        return Err(RpcError::Protocol("too many fds in one RPC frame"));
    }
    if buf.len() > MAX_FRAME_LEN {
        return Err(RpcError::FrameTooLarge {
            declared: buf.len(),
            max: MAX_FRAME_LEN,
        });
    }
    let header = (buf.len() as u32).to_le_bytes();
    let mut slices = [IoSlice::new(&header), IoSlice::new(buf)];
    let mut rest: &mut [IoSlice<'_>] = &mut slices;
    let total = header.len() + buf.len();
    let mut space = [MaybeUninit::uninit(); FD_SPACE];
    let mut sent = 0;
    while sent < total {
        let mut anc = SendAncillaryBuffer::new(&mut space);
        // Sending without the fds would leave the parcel's fd table pointing at nothing.
        if sent == 0 && !fds.is_empty() && !anc.push(SendAncillaryMessage::ScmRights(fds)) {
            return Err(RpcError::Protocol(
                "failed to attach SCM_RIGHTS ancillary data",
            ));
        }
        match rustix::net::sendmsg(sock, rest, &mut anc, SEND_FLAGS) {
            Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into()),
            Ok(n) => {
                sent += n;
                IoSlice::advance_slices(&mut rest, n);
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(std::io::Error::from(e).into()),
        }
    }
    Ok(())
}

/// A send that fails with `EAGAIN` instead of blocking; XNU ignores `MSG_DONTWAIT` on a send.
pub(crate) fn send_nonblocking<T>(
    sock: std::os::fd::BorrowedFd<'_>,
    send: impl FnOnce(rustix::net::SendFlags) -> rustix::io::Result<T>,
) -> rustix::io::Result<T> {
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = sock;
        send(SEND_FLAGS | rustix::net::SendFlags::DONTWAIT)
    }
    #[cfg(target_vendor = "apple")]
    {
        use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
        let flags = fcntl_getfl(sock)?;
        if flags.contains(OFlags::NONBLOCK) {
            return send(SEND_FLAGS);
        }
        fcntl_setfl(sock, flags | OFlags::NONBLOCK)?;
        let sent = send(SEND_FLAGS);
        // A socket left non-blocking would fail every later blocking read with `EAGAIN`.
        fcntl_setfl(sock, flags)?;
        sent
    }
}

/// Whether a send without `drain` waits in `poll` too: Apple has no `SIOCOUTQ` to gain from it,
/// and its non-blocking send costs two `fcntl` (`send_nonblocking`).
pub(crate) const WAITING_SENDS: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// A send on a stream socket that waits in `poll` (`SendWait`), `fds` on the first byte.
///
/// With `drain` it is `RpcTransport::send_raw_draining`; without, a plain send that judges its
/// deadline by the peer's progress as the draining one does (`transport` doc "A slow peer").
pub(crate) fn send_waiting(
    sock: std::os::fd::BorrowedFd<'_>,
    slices: &mut [std::io::IoSlice<'_>],
    fds: &[std::os::fd::BorrowedFd<'_>],
    mut drain: Option<&mut dyn FnMut() -> RpcResult<()>>,
) -> RpcResult<()> {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage};
    use std::io::IoSlice;
    use std::mem::MaybeUninit;

    if fds.len() > MAX_FDS_PER_FRAME {
        return Err(RpcError::Protocol("too many fds in one RPC frame"));
    }
    let total: usize = slices.iter().map(|s| s.len()).sum();
    if !fds.is_empty() && total == 0 {
        // No payload means no `sendmsg` to carry the fds; AOSP frames are never empty.
        return Err(RpcError::Protocol(
            "cannot attach fds to an empty RPC frame",
        ));
    }
    let mut rest: &mut [IoSlice<'_>] = slices;
    let mut space = [MaybeUninit::uninit(); FD_SPACE];
    let mut sent = 0;
    let mut waiting = SendWait::new(sock);
    while sent < total {
        let mut anc = SendAncillaryBuffer::new(&mut space);
        // Sending without the fds would leave the parcel's fd table pointing at nothing.
        if sent == 0 && !fds.is_empty() && !anc.push(SendAncillaryMessage::ScmRights(fds)) {
            return Err(RpcError::Protocol(
                "failed to attach SCM_RIGHTS ancillary data",
            ));
        }
        match send_nonblocking(sock, |flags| {
            rustix::net::sendmsg(sock, rest, &mut anc, flags)
        }) {
            Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into()),
            Ok(n) => {
                sent += n;
                IoSlice::advance_slices(&mut rest, n);
                waiting.progressed();
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::AGAIN) => match waiting.wait(drain.is_some())? {
                Some(true) => {
                    if let Some(drain) = drain.as_mut() {
                        drain().map_err(super::read_side_failure)?;
                    }
                }
                Some(false) => {}
                // As `write_all_reporting`: a deadline before any byte keeps frame sync.
                None if sent == 0 => return Err(RpcError::Timeout),
                None => return Err(RpcError::Io(std::io::ErrorKind::WouldBlock.into())),
            },
            Err(e) => return Err(std::io::Error::from(e).into()),
        }
    }
    Ok(())
}

/// The waits of one send: `SO_SNDTIMEO` counted from the last progress (`transport` "A slow peer").
pub(crate) struct SendWait<'a> {
    sock: std::os::fd::BorrowedFd<'a>,
    since: Option<std::time::Instant>,
    /// Read at the first wait: most sends never wait, and the option costs a syscall.
    limit: Option<Option<std::time::Duration>>,
    /// `queued_bytes` at the last look in this wait; `None` before it or where unsupported.
    queued: Option<u32>,
}

impl<'a> SendWait<'a> {
    pub(crate) fn new(sock: std::os::fd::BorrowedFd<'a>) -> Self {
        SendWait {
            sock,
            since: None,
            limit: None,
            queued: None,
        }
    }

    /// A byte went out: the next wait starts its own deadline.
    pub(crate) fn progressed(&mut self) {
        self.since = None;
        self.queued = None;
    }

    /// What is left of this wait's deadline: `Some(None)` unbounded, `None` passed.
    fn left(&mut self) -> Option<Option<std::time::Duration>> {
        use rustix::net::sockopt::{socket_timeout, Timeout};

        let sock = self.sock;
        let since = match self.since {
            Some(since) => since,
            None => {
                // A new wait: what the peer has yet to take is the baseline its progress lowers.
                self.queued = queued_bytes(sock);
                *self.since.insert(std::time::Instant::now())
            }
        };
        let limit = *self
            .limit
            .get_or_insert_with(|| socket_timeout(sock, Timeout::Send).ok().flatten());
        match limit {
            Some(d) => match d.checked_sub(since.elapsed()) {
                Some(left) if !left.is_zero() => Some(Some(left)),
                _ => None,
            },
            None => Some(None),
        }
    }

    /// The peer took bytes since the last look: the deadline starts over from now.
    fn look_for_peer_progress(&mut self) {
        let Some(before) = self.queued else { return };
        let Some(now) = queued_bytes(self.sock) else {
            return;
        };
        self.queued = Some(now);
        if now < before {
            self.since = Some(std::time::Instant::now());
        }
    }

    /// Whether the send deadline passed with no progress; starts the wait if none is running.
    #[cfg(feature = "rpc-tls")]
    pub(crate) fn expired(&mut self) -> bool {
        self.left().is_none()
    }

    /// Wait for room (and, with `input`, for input or the peer's end) until the deadline.
    ///
    /// `Some(true)` input or the peer's end, `Some(false)` room or a send error, `None` expired.
    pub(crate) fn wait(&mut self, input: bool) -> RpcResult<Option<bool>> {
        use rustix::event::{poll, PollFd, PollFlags, Timespec};

        let events = if input {
            PollFlags::IN | PollFlags::OUT
        } else {
            PollFlags::OUT
        };
        loop {
            let Some(left) = self.left() else {
                return Ok(None);
            };
            // Wake to look at the peer's progress, which `poll` reports only in large steps.
            let pause = match (left, self.limit, self.queued) {
                (Some(left), Some(Some(d)), Some(_)) => Some(left.min(super::look_every(d))),
                (left, _, _) => left,
            };
            let ts = pause.map(|d| Timespec {
                tv_sec: d.as_secs().try_into().unwrap_or(i64::MAX),
                tv_nsec: d.subsec_nanos() as _,
            });
            let mut fds = [PollFd::from_borrowed_fd(self.sock, events)];
            match poll(&mut fds, ts.as_ref()) {
                Ok(0) => self.look_for_peer_progress(),
                Ok(_) => return Ok(Some(fds[0].revents().contains(PollFlags::IN))),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(std::io::Error::from(e).into()),
            }
        }
    }
}

/// A framed transport over a connected Unix domain socket.
pub struct UnixTransport {
    stream: UnixStream,
    peer: PeerIdentity,
    desc: String,
    /// Held by the fd-mode reader for a whole frame; it keeps no bytes between frames.
    fd_recv_lock: std::sync::Mutex<()>,
    reads: ReadDeadline,
}

impl UnixTransport {
    /// Wrap an already-connected `UnixStream`. Peer identity is
    /// resolved once, here, from the socket.
    ///
    /// On Apple platforms a socket already shut down in both directions
    /// (its peer closed before the wrap) fails here with
    /// [`RpcError::EndOfStream`]: XNU refuses the `SO_NOSIGPIPE` this sets,
    /// and no I/O on such a socket could succeed anyway.
    pub fn from_stream(stream: UnixStream) -> RpcResult<Self> {
        // No `MSG_NOSIGNAL` on Apple (module doc "fd passing"); EINVAL = both directions shut.
        #[cfg(target_vendor = "apple")]
        rustix::net::sockopt::set_socket_nosigpipe(&stream, true).map_err(|e| match e {
            rustix::io::Errno::INVAL => RpcError::EndOfStream,
            e => std::io::Error::from(e).into(),
        })?;
        let peer = resolve_peer(&stream);
        let desc = match stream.peer_addr() {
            Ok(a) => format!("unix:{a:?}"),
            Err(_) => "unix:socketpair".to_string(),
        };
        Ok(UnixTransport {
            stream,
            peer,
            desc,
            fd_recv_lock: std::sync::Mutex::new(()),
            reads: ReadDeadline::default(),
        })
    }

    /// A connected pair via `socketpair(AF_UNIX, SOCK_STREAM)`. Both
    /// ends are this process, so both report this process's identity.
    /// Used by hermetic tests; no filesystem path involved.
    pub fn pair() -> RpcResult<(Self, Self)> {
        use rustix::net::{AddressFamily, SocketFlags, SocketType};
        // Atomic CLOEXEC: a `fork`+`exec` before a later `fcntl` would leak both ends (no EOF).
        #[cfg(not(target_vendor = "apple"))]
        let flags = SocketFlags::CLOEXEC;
        // Apple has no `SOCK_CLOEXEC`, so there the flag is set afterwards.
        #[cfg(target_vendor = "apple")]
        let flags = SocketFlags::empty();
        let (a, b) = rustix::net::socketpair(AddressFamily::UNIX, SocketType::STREAM, flags, None)
            .map_err(std::io::Error::from)?;
        #[cfg(target_vendor = "apple")]
        for fd in [&a, &b] {
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)
                .map_err(std::io::Error::from)?;
        }
        Ok((
            Self::from_stream(UnixStream::from(a))?,
            Self::from_stream(UnixStream::from(b))?,
        ))
    }

    /// Wrap a preconnected Unix-domain `OwnedFd` (the
    /// `IAccessor::addConnection()` fd-adopt path). `std`'s
    /// `From<OwnedFd> for UnixStream` is stable cross-platform (Linux +
    /// macOS), and the resulting transport is byte-identical to
    /// [`UnixTransport::from_stream`] — peer identity is resolved the
    /// same way over the same fd. The caller is responsible for
    /// asserting the fd's address family (`AF_UNIX`); see
    /// [`crate::rpc::RpcSession::from_preconnected_fd`].
    ///
    /// The fd must be in blocking mode: a read on an `O_NONBLOCK` fd that
    /// finds no data returns [`RpcError::Timeout`] at once. An Accessor fd
    /// arrives non-blocking; [`crate::rpc::RpcSession::from_preconnected_fd`]
    /// clears the flag, this does not.
    pub fn from_owned_fd(fd: OwnedFd) -> RpcResult<Self> {
        Self::from_stream(UnixStream::from(fd))
    }

    /// Connect to a listening Unix socket at `path` (client side).
    pub fn connect(path: impl AsRef<Path>) -> RpcResult<Self> {
        // `RpcError: From<std::io::Error>` — `?` does the conversion.
        let stream = UnixStream::connect(path)?;
        Self::from_stream(stream)
    }

    /// Connect to a Linux/Android abstract Unix socket.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn connect_abstract(name: &[u8]) -> RpcResult<Self> {
        let addr = UnixSocketAddr::from_abstract_name(name)?;
        Self::from_stream(UnixStream::connect_addr(&addr)?)
    }
}

/// Peer identity of a connected Unix socket; see module doc "Peer identity".
fn resolve_peer(stream: &UnixStream) -> PeerIdentity {
    // Android has Linux `SO_PEERCRED` but no `getpeereid`, so it takes the Linux arm.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // libc, not rustix `socket_peercred`: its `NonZeroI32` pid is UB for pid 0 (module doc).
        use std::os::fd::AsRawFd;
        let mut uc = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `uc`/`len` are sized SO_PEERCRED out-params; `stream` keeps the fd open.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut uc as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            // A socket without peer creds (rare) is anonymous, not a forged local identity.
            return PeerIdentity::Anonymous;
        }
        PeerIdentity::Local {
            uid: uc.uid,
            // `-1` is the documented "unavailable" pid.
            pid: if uc.pid > 0 { uc.pid } else { -1 },
        }
    }
    #[cfg(all(unix, not(target_os = "linux"), not(target_os = "android")))]
    {
        resolve_peer_bsd(stream)
    }
    #[cfg(not(unix))]
    {
        let _ = stream;
        PeerIdentity::Anonymous
    }
}

/// macOS/BSD peer resolution over `getpeereid`; see module doc "Peer identity".
#[cfg(all(unix, not(target_os = "linux"), not(target_os = "android")))]
fn resolve_peer_bsd(stream: &UnixStream) -> PeerIdentity {
    use std::os::fd::{AsFd, AsRawFd};
    let fd = stream.as_raw_fd();

    // macOS `getpeereid` succeeds with uid 0 on a non-`AF_UNIX` fd: check the family first.
    match rustix::net::getsockname(stream.as_fd()) {
        Ok(local) if local.address_family() == rustix::net::AddressFamily::UNIX => {}
        Ok(_) => {
            log::warn!("RPC unix peer-cred: fd is not AF_UNIX; reporting Anonymous");
            return PeerIdentity::Anonymous;
        }
        Err(e) => {
            log::warn!("RPC unix peer-cred: getsockname failed ({e}); reporting Anonymous");
            return PeerIdentity::Anonymous;
        }
    }

    let mut euid: libc::uid_t = 0;
    let mut egid: libc::gid_t = 0;
    // SAFETY: `stream` keeps `fd` open; `euid`/`egid` are typed out-params, not retained.
    let rc = unsafe { libc::getpeereid(fd, &mut euid, &mut egid) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error();
        return match errno {
            // No remote peer (unconnected fd, BSD socketpair): self identity (module doc).
            Some(libc::ENOTCONN) | Some(libc::EINVAL) => super::mem::self_identity(),
            // Any other error: never a forged `Local`; no ACL is possible, so log loudly.
            _ => {
                log::warn!(
                    "RPC unix peer-cred unavailable (getpeereid errno={errno:?}); \
                     reporting Anonymous — no peer ACL is possible"
                );
                PeerIdentity::Anonymous
            }
        };
    }
    PeerIdentity::Local {
        uid: euid as u32,
        pid: peer_pid(fd),
    }
}

/// Peer pid via `LOCAL_PEERPID` (macOS 10.8+); `-1` (unavailable) on failure.
#[cfg(target_os = "macos")]
fn peer_pid(fd: std::os::fd::RawFd) -> i32 {
    let mut pid: libc::pid_t = -1;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: valid socket `fd`; `pid`/`len` are correctly-sized `LOCAL_PEERPID` out-params.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut libc::pid_t as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 {
        pid
    } else {
        -1
    }
}

#[cfg(all(
    unix,
    not(target_os = "linux"),
    not(target_os = "macos"),
    not(target_os = "android")
))]
fn peer_pid(_fd: std::os::fd::RawFd) -> i32 {
    -1
}

impl RpcTransport for UnixTransport {
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
        // A send runs beside a receiving thread with no lock.
        send_frame_vectored(self.stream.as_fd(), buf, &[])
    }

    fn recv_frame(&self) -> RpcResult<Vec<u8>> {
        let mut r = &self.stream;
        read_frame(&mut r)
    }

    /// Raw, unframed write (android-13+ profile — the real android RPC
    /// wire has no length prefix). `send(2)` with `SEND_FLAGS` (no `SIGPIPE`)
    /// needs only `&self`, so it stays full-duplex (same as `send_frame`).
    fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
        if WAITING_SENDS {
            let slices = &mut [std::io::IoSlice::new(buf)];
            return send_waiting(self.stream.as_fd(), slices, &[], None);
        }
        send_raw_nosignal(self.stream.as_fd(), buf)
    }

    /// Raw, unframed read (one `read`; `Ok(0)` = peer closed). The
    /// android-13+ profile drives `RpcWireHeader`-based framing on top
    /// of this (`wire_android13::read_aosp_message`).
    fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
        let sock = Some(self.stream.as_fd());
        self.reads
            .read(sock, || super::read_raw(&mut &self.stream, buf))
    }

    /// Raw, **unframed** write + `SCM_RIGHTS` (the android-13+ v1+
    /// `Unix` FD-over-RPC path). Identical to
    /// [`UnixTransport::send_frame_with_fds`] minus the 4-byte length
    /// prefix — the AOSP RPC wire has none. The fds ride the **first**
    /// `sendmsg` (AOSP `RpcTransportRaw::interruptableWriteFully`,
    /// `sentFds |= ret > 0`); the rest (rare — fd transactions are
    /// tiny) follow without ancillary.
    fn send_raw_with_fds(&self, buf: &[u8], fds: &[std::os::fd::BorrowedFd<'_>]) -> RpcResult<()> {
        use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage};
        use std::io::IoSlice;
        use std::mem::MaybeUninit;

        if fds.is_empty() {
            return self.send_raw(buf);
        }
        if WAITING_SENDS {
            let slices = &mut [IoSlice::new(buf)];
            return send_waiting(self.stream.as_fd(), slices, fds, None);
        }
        if buf.is_empty() {
            // No payload means no `sendmsg` to carry the fds; AOSP frames are never empty.
            return Err(RpcError::Protocol(
                "cannot attach fds to an empty RPC frame",
            ));
        }
        if fds.len() > MAX_FDS_PER_FRAME {
            return Err(RpcError::Protocol("too many fds in one RPC frame"));
        }
        // As `send_raw`, no length check: `write_aosp_message_with_fds` caps the body.
        let mut space = vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(fds.len()))];
        let mut sent = 0;
        while sent < buf.len() {
            let mut anc = SendAncillaryBuffer::new(&mut space);
            if sent == 0 {
                // Sending without the fds would leave the parcel's fd table pointing at nothing.
                if !anc.push(SendAncillaryMessage::ScmRights(fds)) {
                    return Err(RpcError::Protocol(
                        "failed to attach SCM_RIGHTS ancillary data",
                    ));
                }
            }
            let n = match rustix::net::sendmsg(
                &self.stream,
                &[IoSlice::new(&buf[sent..])],
                &mut anc,
                SEND_FLAGS,
            ) {
                Ok(n) => n,
                // EINTR is benign — retry the syscall.
                Err(rustix::io::Errno::INTR) => continue,
                // As `write_all_reporting`: a deadline before any byte sent keeps frame sync.
                Err(e) => {
                    let e = std::io::Error::from(e);
                    return Err(if sent == 0 && super::is_timeout(&e) {
                        RpcError::Timeout
                    } else {
                        e.into()
                    });
                }
            };
            if n == 0 {
                return Err(RpcError::EndOfStream);
            }
            sent += n;
        }
        Ok(())
    }

    /// Raw, **unframed** read (one `recvmsg`) + any `SCM_RIGHTS` fds.
    /// Pairs with
    /// [`UnixTransport::send_raw_with_fds`]; received fds are
    /// `O_CLOEXEC` (`MSG_CMSG_CLOEXEC`; on Apple, set after `recvmsg`).
    /// `Ok((0, _))` ⇒ peer closed. The android-13+ message reader
    /// (`read_aosp_message_with_fds`) drives exact header/body byte counts
    /// and accumulates fds across those `recvmsg`s (AOSP
    /// `RpcTransportRaw::interruptableReadFully`).
    fn recv_raw_with_fds(&self, buf: &mut [u8]) -> RpcResult<(usize, Vec<std::os::fd::OwnedFd>)> {
        use rustix::net::RecvAncillaryBuffer;
        use std::io::IoSliceMut;
        use std::mem::MaybeUninit;

        let mut fds: Vec<std::os::fd::OwnedFd> = Vec::new();
        let mut space =
            vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_PER_FRAME))];
        let mut anc = RecvAncillaryBuffer::new(&mut space);
        let r = self.reads.read(Some(self.stream.as_fd()), || loop {
            match rustix::net::recvmsg(
                &self.stream,
                &mut [IoSliceMut::new(buf)],
                &mut anc,
                RECV_FLAGS,
            ) {
                Ok(r) => return Ok(r),
                // EINTR retry, symmetric with read_header.
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => {
                    let io_err = std::io::Error::from(e);
                    // Deadline → `Timeout`; mid-message the reader makes it `DeadlineMidFrame`.
                    if super::is_timeout(&io_err) {
                        return Err(RpcError::Timeout);
                    }
                    return Err(io_err.into());
                }
            }
        })?;
        take_scm_rights(r.flags, &mut anc, &mut fds)?;
        Ok((r.bytes, fds))
    }

    fn peer_identity(&self) -> PeerIdentity {
        self.peer.clone()
    }

    fn describe(&self) -> &str {
        &self.desc
    }

    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        self.stream.set_read_timeout(timeout)?;
        self.reads.note(timeout);
        Ok(())
    }

    fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        self.stream.set_write_timeout(timeout)?;
        Ok(())
    }

    fn shutdown(&self) -> RpcResult<()> {
        // No `fd_recv_lock`: a reader parked in `recvmsg` holds it until this call wakes it.
        super::absorb_already_shut(self.stream.shutdown(std::net::Shutdown::Both))
    }

    fn peer_closed(&self) -> Option<bool> {
        use std::os::fd::AsFd;
        super::socket_peer_closed(self.stream.as_fd(), super::SocketKind::UnixDomain)
    }

    fn supports_fd_passing(&self) -> bool {
        true
    }

    /// Send `buf` as a length-prefixed frame, passing `fds` out-of-band
    /// via `SCM_RIGHTS` (`Unix` fd-mode). The ancillary
    /// fds ride the **first** `sendmsg`; remaining bytes (rare — fd
    /// transactions are tiny) follow without ancillary.
    fn send_frame_with_fds(
        &self,
        buf: &[u8],
        fds: &[std::os::fd::BorrowedFd<'_>],
    ) -> RpcResult<()> {
        send_frame_vectored(self.stream.as_fd(), buf, fds)
    }

    fn send_raw_draining(
        &self,
        buf: &[u8],
        fds: &[std::os::fd::BorrowedFd<'_>],
        drain: &mut dyn FnMut() -> RpcResult<()>,
    ) -> RpcResult<()> {
        // No length check: raw bytes have no frame, and the caller caps the body.
        send_waiting(
            self.stream.as_fd(),
            &mut [std::io::IoSlice::new(buf)],
            fds,
            Some(drain),
        )
    }

    /// Receive one length-prefixed frame plus any `SCM_RIGHTS` fds.
    /// Received fds are `O_CLOEXEC` (`MSG_CMSG_CLOEXEC`; on Apple, which
    /// lacks it, `fcntl_setfd` after receipt — same as
    /// [`recv_raw_with_fds`](Self::recv_raw_with_fds)). Connections in `Unix`
    /// fd-mode use this for *every* frame, so `recvmsg` and `Read` are never
    /// mixed on one fd.
    fn recv_frame_with_fds(&self) -> RpcResult<(Vec<u8>, Vec<std::os::fd::OwnedFd>)> {
        use std::mem::MaybeUninit;

        // Whole frames under the lock: interleaved readers would hand one frame's fds to another.
        let _reader = self
            .fd_recv_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut fds: Vec<std::os::fd::OwnedFd> = Vec::new();
        // Reused: a frame costs two or more `recvmsg`s and `RecvAncillaryBuffer::new` resets it.
        let mut space = [MaybeUninit::uninit(); FD_SPACE];
        // Exact counts, header then body: never read past this frame (module doc).
        let mut header = [0u8; 4];
        let mut filled = 0;
        while filled < header.len() {
            filled += self.recv_fd_part(&mut header[filled..], &mut space, &mut fds, filled > 0)?;
        }
        let len = u32::from_le_bytes(header) as usize;
        if len > MAX_FRAME_LEN {
            // Reject *before* allocating `len` bytes.
            return Err(RpcError::FrameTooLarge {
                declared: len,
                max: MAX_FRAME_LEN,
            });
        }
        let mut frame = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            filled += self.recv_fd_part(&mut frame[filled..], &mut space, &mut fds, true)?;
        }
        Ok((frame, fds))
    }
}

impl UnixTransport {
    /// One `recvmsg` into non-empty `buf`; `started`: a byte was read, so an end or deadline cuts.
    fn recv_fd_part(
        &self,
        buf: &mut [u8],
        space: &mut [std::mem::MaybeUninit<u8>],
        fds: &mut Vec<OwnedFd>,
        started: bool,
    ) -> RpcResult<usize> {
        use rustix::net::RecvAncillaryBuffer;
        use std::io::IoSliceMut;

        let consumed = started || !fds.is_empty();
        let mut anc = RecvAncillaryBuffer::new(space);
        let r = loop {
            match rustix::net::recvmsg(
                &self.stream,
                &mut [IoSliceMut::new(buf)],
                &mut anc,
                RECV_FLAGS,
            ) {
                Ok(r) => break r,
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => {
                    // As `read_header`: idle deadline is `Timeout`, mid-frame our cut.
                    let io_err = std::io::Error::from(e);
                    if super::is_timeout(&io_err) {
                        return Err(if consumed {
                            RpcError::DeadlineMidFrame
                        } else {
                            RpcError::Timeout
                        });
                    }
                    // Past the first byte or fd, an `EndOfStream` is a cut.
                    return Err(match RpcError::from(io_err) {
                        RpcError::EndOfStream if consumed => RpcError::Truncated,
                        other => other,
                    });
                }
            }
        };
        take_scm_rights(r.flags, &mut anc, fds)?;
        if r.bytes == 0 {
            return Err(if consumed || !fds.is_empty() {
                RpcError::Truncated
            } else {
                RpcError::EndOfStream
            });
        }
        Ok(r.bytes)
    }
}

/// One `recvmsg`'s fds into `fds` (`O_CLOEXEC` on Apple too); the cap counts all of `fds`.
fn take_scm_rights(
    flags: rustix::net::ReturnFlags,
    anc: &mut rustix::net::RecvAncillaryBuffer<'_>,
    fds: &mut Vec<OwnedFd>,
) -> RpcResult<()> {
    use rustix::net::{RecvAncillaryMessage, ReturnFlags};

    // `MSG_CTRUNC`: the kernel dropped surplus fds; fail, as AOSP `OS_unix_base.cpp` (EPIPE).
    if flags.contains(ReturnFlags::CTRUNC) {
        return Err(RpcError::Protocol(
            "SCM_RIGHTS control message truncated (too many fds in one message)",
        ));
    }
    for msg in anc.drain() {
        if let RecvAncillaryMessage::ScmRights(iter) = msg {
            for fd in iter {
                #[cfg(target_vendor = "apple")]
                rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)
                    .map_err(std::io::Error::from)?;
                fds.push(fd);
                if fds.len() > MAX_FDS_PER_FRAME {
                    return Err(RpcError::Protocol("too many fds in one RPC frame"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::RpcError;
    use std::sync::Arc;

    #[test]
    fn unix_roundtrip_all_sizes() {
        // A worker sends: 1 MiB + 1 exceeds the socket buffer and tests `read_body` reassembly.
        let (a, b) = UnixTransport::pair().expect("socketpair");
        let a = Arc::new(a);
        for size in [0usize, 1, 64 * 1024, 1 << 20, (1 << 20) + 1] {
            let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let sender = {
                let a = a.clone();
                let p = payload.clone();
                std::thread::spawn(move || a.send_frame(&p).unwrap())
            };
            assert_eq!(b.recv_frame().expect("recv"), payload, "size {size}");
            sender.join().unwrap();
        }
    }

    /// The fd-mode pair round-trips every size with its fd, including an empty body (header only).
    #[test]
    fn fd_mode_roundtrip_all_sizes() {
        use std::os::fd::AsFd;
        let (a, b) = UnixTransport::pair().expect("socketpair");
        let a = Arc::new(a);
        for size in [0usize, 1, 8191, 8192, 8193, 64 * 1024, (1 << 20) + 1] {
            let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let sender = {
                let a = a.clone();
                let p = payload.clone();
                std::thread::spawn(move || {
                    let (fd, _keep) = UnixStream::pair().expect("an fd to pass");
                    a.send_frame_with_fds(&p, &[fd.as_fd()]).unwrap()
                })
            };
            let (got, fds) = b.recv_frame_with_fds().expect("recv");
            assert_eq!(got, payload, "size {size}");
            assert_eq!(fds.len(), 1, "size {size}");
            sender.join().unwrap();
        }
    }

    /// Each queued fd-mode frame gets exactly its own fds (module doc "fd passing" and "Tests").
    #[test]
    fn fd_mode_frames_queued_back_to_back_keep_their_own_fds() {
        use rustix::net::sockopt;
        use std::os::fd::AsFd;

        let (a, b) = UnixTransport::pair().expect("socketpair");
        // Both frames queue before the first read, so the reader sees them back to back.
        sockopt::set_socket_send_buffer_size(&a.stream, 1 << 20).expect("SO_SNDBUF");
        sockopt::set_socket_recv_buffer_size(&b.stream, 1 << 20).expect("SO_RCVBUF");
        a.set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("a full buffer fails the test instead of hanging it");
        let inode = |fd: std::os::fd::BorrowedFd<'_>| {
            let st = rustix::fs::fstat(fd).expect("fstat");
            (st.st_dev, st.st_ino)
        };
        let (first_fd, _keep1) = UnixStream::pair().expect("fd for the first frame");
        let (second_fd, _keep2) = UnixStream::pair().expect("fd for the second frame");
        let first: Vec<u8> = (0..20 * 1024).map(|i| (i % 251) as u8).collect();
        let second = b"second frame".to_vec();
        a.send_frame_with_fds(&first, &[first_fd.as_fd()])
            .expect("send the first frame");
        a.send_frame_with_fds(&second, &[second_fd.as_fd()])
            .expect("send the second frame");

        for (payload, sent) in [(&first, &first_fd), (&second, &second_fd)] {
            let (got, fds) = b.recv_frame_with_fds().expect("recv");
            assert_eq!(&got, payload, "frame of {} bytes", payload.len());
            let got_inodes: Vec<_> = fds.iter().map(|fd| inode(fd.as_fd())).collect();
            assert_eq!(
                got_inodes,
                [inode(sent.as_fd())],
                "the {}-byte frame must carry its own fd and no other",
                payload.len()
            );
        }
    }

    /// Not on s390x: CI runs it under qemu-user with no binfmt entry, so the child cannot start.
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        not(target_arch = "s390x")
    ))]
    #[test]
    fn an_fd_send_to_a_closed_peer_is_an_error_not_a_signal() {
        use std::os::fd::AsFd;
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "RSB_UNIX_SIGPIPE_CHILD";
        const NAME: &str =
            "rpc::transport::unix::tests::an_fd_send_to_a_closed_peer_is_an_error_not_a_signal";

        if std::env::var_os(CHILD).is_some() {
            // SAFETY: restores the default disposition; this child runs no other test.
            unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
            let (a, b) = UnixTransport::pair().expect("socketpair");
            drop(b);
            let file = std::fs::File::open("/dev/null").expect("/dev/null");
            let fds = [file.as_fd()];
            assert!(a.send_frame_with_fds(b"frame", &fds).is_err());
            assert!(a.send_raw_with_fds(b"raw", &fds).is_err());
            assert!(a.send_raw(b"raw").is_err());
            #[cfg(feature = "rpc-tls")]
            {
                let (s, peer) = UnixStream::pair().expect("socketpair");
                drop(peer);
                assert!(crate::rpc::transport::TlsStream::write(&s, b"tls record").is_err());
            }
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().expect("current_exe"))
            .args(["--exact", NAME, "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .expect("spawn the child");
        assert_eq!(out.status.signal(), None, "a send raised a signal");
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Zero tests run would also exit 0: the filter must have found this test.
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{:?}\n{stdout}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn unix_peer_identity_is_this_process_for_socketpair() {
        let (a, _b) = UnixTransport::pair().expect("socketpair");
        // Both ends live in this process; on Linux this is the real SO_PEERCRED syscall.
        let id = a.peer_identity();
        assert_eq!(
            id,
            PeerIdentity::Local {
                uid: rustix::process::getuid().as_raw(),
                pid: std::process::id() as i32,
            },
            "socketpair peer must be this process (got {id})"
        );
        assert!(a.describe().starts_with("unix:"));
    }

    #[test]
    fn unix_peer_closed_on_drop() {
        let (a, b) = UnixTransport::pair().expect("socketpair");
        drop(b);
        // First recv sees EOF -> clean EndOfStream.
        assert!(matches!(a.recv_frame(), Err(RpcError::EndOfStream)));
    }

    /// A frame a deadline cut leaves nothing for a later read; see module doc "Tests".
    #[test]
    fn unix_fd_mode_cut_frame_leaves_nothing_behind() {
        use std::io::Write;
        let (a, b) = UnixTransport::pair().expect("socketpair");
        b.set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .expect("deadline");
        // A header promising 8 bytes, then only 3 of them.
        let mut partial = 8u32.to_le_bytes().to_vec();
        partial.extend_from_slice(&[1, 2, 3]);
        (&a.stream).write_all(&partial).expect("partial frame");
        assert!(matches!(
            b.recv_frame_with_fds(),
            Err(RpcError::DeadlineMidFrame)
        ));
        b.shutdown().expect("shutdown");
        let after = b.recv_frame_with_fds();
        assert!(
            matches!(after, Err(RpcError::EndOfStream)),
            "the cut frame must not come back after shutdown, got {after:?}"
        );
        b.shutdown().expect("a second shutdown is Ok (idempotent)");
    }

    /// Both readers: a mid-frame deadline is `DeadlineMidFrame`, not `Truncated`; see module doc.
    #[test]
    fn unix_mid_frame_deadline_is_our_own_cut() {
        use std::io::Write;
        let (a, b) = UnixTransport::pair().expect("socketpair");
        b.set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .expect("deadline");
        assert!(matches!(b.recv_frame(), Err(RpcError::Timeout)));
        assert!(matches!(b.recv_frame_with_fds(), Err(RpcError::Timeout)));

        // A header promising 8 bytes, then only 3 of them.
        let mut partial = 8u32.to_le_bytes().to_vec();
        partial.extend_from_slice(&[1, 2, 3]);
        let mut w = &a.stream;
        w.write_all(&partial).expect("partial frame");
        assert!(
            matches!(b.recv_frame(), Err(RpcError::DeadlineMidFrame)),
            "the framed reader must name our own deadline, not a truncation"
        );

        w.write_all(&partial).expect("partial frame");
        assert!(
            matches!(b.recv_frame_with_fds(), Err(RpcError::DeadlineMidFrame)),
            "the fd-mode reader must classify the same way"
        );
    }

    /// A `from_owned_fd` half (the `IAccessor::addConnection` entry) round-trips frames.
    #[test]
    fn unix_from_owned_fd_roundtrip() {
        use rustix::net::{AddressFamily, SocketFlags, SocketType};

        let (a, b) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .expect("socketpair");
        let client = UnixTransport::from_owned_fd(a).expect("adopt a");
        let server = UnixTransport::from_owned_fd(b).expect("adopt b");
        let payload = b"hello-accessor".to_vec();
        let client = Arc::new(client);
        let sender = {
            let c = client.clone();
            let p = payload.clone();
            std::thread::spawn(move || c.send_frame(&p).unwrap())
        };
        assert_eq!(server.recv_frame().expect("recv"), payload);
        sender.join().unwrap();
    }

    #[test]
    fn unix_partial_header_then_close_is_truncated() {
        // 2-of-4 header bytes then EOF must be `Truncated` (`read_header`: `filled > 0`).
        let (a, b) = UnixTransport::pair().expect("socketpair");
        {
            use std::io::Write;
            let mut s = &a.stream;
            s.write_all(&[1u8, 0]).unwrap(); // 2 of 4 header bytes
        }
        drop(a);
        let r = b.recv_frame();
        assert!(
            matches!(r, Err(RpcError::Truncated)),
            "expected Truncated (2-of-4 header consumed before EOF), got {r:?}"
        );
    }

    /// A `MAX_FRAME_LEN` body goes out with its header, draining or not, as on other transports.
    #[test]
    fn a_max_frame_len_body_is_not_refused_for_its_header() {
        use crate::rpc::wire_android13::write_aosp_message_with_fds;
        use std::os::fd::AsFd;
        let (a, b) = UnixTransport::pair().expect("socketpair");
        let msg = vec![0u8; 16 + MAX_FRAME_LEN];
        let total = 2 * msg.len();
        let reader = std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 20];
            let mut got = 0;
            while got < total {
                match b.recv_raw_with_fds(&mut buf) {
                    Ok((0, _)) | Err(_) => break,
                    Ok((n, _)) => got += n,
                }
            }
            got
        });
        let (fd, _keep) = std::os::unix::net::UnixStream::pair().expect("an fd");
        write_aosp_message_with_fds(&a, &msg, &[], Some(&mut || Ok(()))).expect("draining");
        write_aosp_message_with_fds(&a, &msg, &[fd.as_fd()], None).expect("with an fd");
        assert_eq!(reader.join().expect("reader"), total);
    }
}
