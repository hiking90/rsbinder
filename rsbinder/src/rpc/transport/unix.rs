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
//! `recv_frame_with_fds` never reads past the last byte of the frame in
//! progress. `AF_UNIX` glues stream data across skbs and stops only after
//! consuming the one that carried fds, so a `recvmsg` spilling into the
//! next frame would attach that frame's `SCM_RIGHTS` fds to this one and
//! leave the next frame with none. The android-13+ reader
//! (`wire_android13::read_aosp_message_with_fds`) reads exact byte counts
//! for the same reason.
//!
//! `shutdown` cuts the socket before taking `fd_recv_buf`: a reader parked
//! in `recvmsg` holds that lock for the whole call and releases it only
//! once it wakes, so locking first would deadlock against it. What that
//! reader appends on waking was queued in the kernel before `shutdown` (the
//! platform's business); what is left in the buffer after it returns is the
//! prefix of a frame an error cut short, and `shutdown` clears it so a
//! later reader does not decode the ended connection out of it.
//!
//! ## Tests
//!
//! - `unix_shutdown_drops_the_fd_mode_leftover` (plan 2-21): the reader never
//!   reads past the frame in progress, so `fd_recv_buf` can only hold the
//!   prefix an error left behind; a deadline part-way through a frame puts
//!   one there. What the kernel still holds is pinned by
//!   `rpc_transport_conformance`, not here.
//! - `unix_mid_frame_deadline_is_our_own_cut` (plan 2-21): a read deadline
//!   part-way through a frame is this end's own cut (`DeadlineMidFrame`),
//!   told apart from a stream that ended mid-frame (`Truncated`); with
//!   nothing consumed it stays the boundary `Timeout`. The framed reader and
//!   the fd-mode reader classify the same way.

use std::io::{Read, Write};
#[cfg(target_os = "android")]
use std::os::android::net::SocketAddrExt;
use std::os::fd::OwnedFd;
#[cfg(target_os = "linux")]
use std::os::linux::net::SocketAddrExt;
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::os::unix::net::SocketAddr as UnixSocketAddr;
use std::os::unix::net::UnixStream;
use std::path::Path;

use super::{read_frame, write_frame, PeerIdentity, RpcTransport, MAX_FRAME_LEN};
use crate::rpc::{RpcError, RpcResult};

/// Per-message fd cap (DoS bound, < `SCM_MAX_FD` 253); `wire_android13` applies it across recvmsgs.
pub(crate) const MAX_FDS_PER_FRAME: usize = 64;

/// `sendmsg` flags: a send to a closed peer is `EPIPE`, not `SIGPIPE`, as std's own `send`.
/// Apple has no `MSG_NOSIGNAL`; std sets `SO_NOSIGPIPE` on the sockets it creates there instead.
#[cfg(any(target_os = "linux", target_os = "android"))]
const SEND_FLAGS: rustix::net::SendFlags = rustix::net::SendFlags::NOSIGNAL;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SEND_FLAGS: rustix::net::SendFlags = rustix::net::SendFlags::empty();

/// A framed transport over a connected Unix domain socket.
pub struct UnixTransport {
    stream: UnixStream,
    peer: PeerIdentity,
    desc: String,
    /// `recvmsg` leftover of the fd-mode path only, so `Read` and `recvmsg` never mix on one fd.
    fd_recv_buf: std::sync::Mutex<Vec<u8>>,
}

impl UnixTransport {
    /// Wrap an already-connected `UnixStream`. Peer identity is
    /// resolved once, here, from the socket.
    pub fn from_stream(stream: UnixStream) -> RpcResult<Self> {
        let peer = resolve_peer(&stream);
        let desc = match stream.peer_addr() {
            Ok(a) => format!("unix:{a:?}"),
            Err(_) => "unix:socketpair".to_string(),
        };
        Ok(UnixTransport {
            stream,
            peer,
            desc,
            fd_recv_buf: std::sync::Mutex::new(Vec::new()),
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
        // SAFETY: `ucred` is three plain integers, for which all-zero is a valid value.
        let mut uc: libc::ucred = unsafe { std::mem::zeroed() };
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
        // `&UnixStream: Write`: a send runs beside a receiving thread with no lock.
        let mut w = &self.stream;
        write_frame(&mut w, buf)
    }

    fn recv_frame(&self) -> RpcResult<Vec<u8>> {
        let mut r = &self.stream;
        read_frame(&mut r)
    }

    /// Raw, unframed write (android-13+ profile — the real android RPC
    /// wire has no length prefix). `&UnixStream: Write`, so a shared
    /// `&self` stays full-duplex (same as `send_frame`).
    fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
        let mut w = &self.stream;
        super::write_all_reporting(&mut w, buf)?;
        w.flush().map_err(RpcError::from)?;
        Ok(())
    }

    /// Raw, unframed read (one `read`; `Ok(0)` = peer closed). The
    /// android-13+ profile drives `RpcWireHeader`-based framing on top
    /// of this (`wire_android13::read_aosp_message`).
    fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
        let mut r = &self.stream;
        loop {
            return match r.read(buf) {
                Ok(n) => Ok(n),
                // EINTR: retry, as `recv_raw_with_fds` and AOSP `interruptableReadFully` do.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // Deadline → `Timeout`: `read_exact_raw` splits `Timeout`/`DeadlineMidFrame`.
                Err(e) if super::is_timeout(&e) => Err(RpcError::Timeout),
                Err(e) => Err(RpcError::from(e)),
            };
        }
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
        if buf.is_empty() {
            // No payload means no `sendmsg` to carry the fds; AOSP frames are never empty.
            return Err(RpcError::Protocol(
                "cannot attach fds to an empty RPC frame",
            ));
        }
        if fds.len() > MAX_FDS_PER_FRAME {
            return Err(RpcError::Protocol("too many fds in one RPC frame"));
        }
        if buf.len() > MAX_FRAME_LEN {
            return Err(RpcError::FrameTooLarge {
                declared: buf.len(),
                max: MAX_FRAME_LEN,
            });
        }
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
    /// `O_CLOEXEC` (set explicitly — `MSG_CMSG_CLOEXEC` is Linux-only).
    /// `Ok((0, _))` ⇒ peer closed. Unlike
    /// [`UnixTransport::recv_frame_with_fds`] there is **no** leftover
    /// buffer: the android-13+ message reader (`read_aosp_message
    /// _with_fds`) drives exact header/body byte counts and accumulates
    /// fds across those `recvmsg`s (AOSP
    /// `RpcTransportRaw::interruptableReadFully`).
    fn recv_raw_with_fds(&self, buf: &mut [u8]) -> RpcResult<(usize, Vec<std::os::fd::OwnedFd>)> {
        use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags};
        use std::io::IoSliceMut;
        use std::mem::MaybeUninit;

        let mut fds: Vec<std::os::fd::OwnedFd> = Vec::new();
        let mut space =
            vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_PER_FRAME))];
        let mut anc = RecvAncillaryBuffer::new(&mut space);
        let r = loop {
            match rustix::net::recvmsg(
                &self.stream,
                &mut [IoSliceMut::new(buf)],
                &mut anc,
                RecvFlags::empty(),
            ) {
                Ok(r) => break r,
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
        };
        // `MSG_CTRUNC`: the kernel dropped surplus fds; fail, as AOSP `OS_unix_base.cpp` (EPIPE).
        if r.flags.contains(ReturnFlags::CTRUNC) {
            return Err(RpcError::Protocol(
                "SCM_RIGHTS control message truncated (too many fds in one message)",
            ));
        }
        for msg in anc.drain() {
            if let RecvAncillaryMessage::ScmRights(iter) = msg {
                for fd in iter {
                    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)
                        .map_err(std::io::Error::from)?;
                    fds.push(fd);
                    if fds.len() > MAX_FDS_PER_FRAME {
                        return Err(RpcError::Protocol("too many fds in one RPC frame"));
                    }
                }
            }
        }
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
        Ok(())
    }

    fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        self.stream.set_write_timeout(timeout)?;
        Ok(())
    }

    fn shutdown(&self) -> RpcResult<()> {
        // Socket first: a reader parked in `recvmsg` holds `fd_recv_buf` until it wakes.
        let shut = self.stream.shutdown(std::net::Shutdown::Both);
        // What remains is a cut frame's prefix; a later reader must not decode it.
        if let Ok(mut leftover) = self.fd_recv_buf.lock() {
            leftover.clear();
        }
        super::absorb_already_shut(shut)
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
        use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage};
        use std::io::IoSlice;
        use std::mem::MaybeUninit;

        if fds.is_empty() {
            return self.send_frame(buf);
        }
        if fds.len() > MAX_FDS_PER_FRAME {
            return Err(RpcError::Protocol("too many fds in one RPC frame"));
        }
        if buf.len() > MAX_FRAME_LEN {
            return Err(RpcError::FrameTooLarge {
                declared: buf.len(),
                max: MAX_FRAME_LEN,
            });
        }
        let mut framed = Vec::with_capacity(4 + buf.len());
        framed.extend_from_slice(&(buf.len() as u32).to_le_bytes());
        framed.extend_from_slice(buf);

        let mut space = vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(fds.len()))];
        let mut sent = 0;
        while sent < framed.len() {
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
                &[IoSlice::new(&framed[sent..])],
                &mut anc,
                SEND_FLAGS,
            ) {
                Ok(n) => n,
                // EINTR retry, symmetric with read_header.
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(std::io::Error::from(e).into()),
            };
            if n == 0 {
                return Err(RpcError::EndOfStream);
            }
            sent += n;
        }
        Ok(())
    }

    /// Receive one length-prefixed frame plus any `SCM_RIGHTS` fds.
    /// Received fds are made `O_CLOEXEC` explicitly via `fcntl_setfd`
    /// (`recvmsg` runs with `RecvFlags::empty()`; `MSG_CMSG_CLOEXEC` is
    /// Linux-only, so the portable path sets the flag after receipt — same as
    /// [`recv_raw_with_fds`](Self::recv_raw_with_fds)). Connections in `Unix`
    /// fd-mode use this for *every* frame, so `recvmsg` and `Read` are never
    /// mixed on one fd.
    fn recv_frame_with_fds(&self) -> RpcResult<(Vec<u8>, Vec<std::os::fd::OwnedFd>)> {
        use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags};
        use std::io::IoSliceMut;
        use std::mem::MaybeUninit;

        let mut leftover = self.fd_recv_buf.lock().expect("fd recv buf poisoned");
        let mut fds: Vec<std::os::fd::OwnedFd> = Vec::new();
        // Reused: a frame costs two or more `recvmsg`s and `RecvAncillaryBuffer::new` resets it.
        let mut space =
            vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_PER_FRAME))];
        loop {
            if leftover.len() >= 4 {
                let len = u32::from_le_bytes(leftover[0..4].try_into().unwrap()) as usize;
                if len > MAX_FRAME_LEN {
                    return Err(RpcError::FrameTooLarge {
                        declared: len,
                        max: MAX_FRAME_LEN,
                    });
                }
                if leftover.len() >= 4 + len {
                    let frame = leftover[4..4 + len].to_vec();
                    leftover.drain(0..4 + len);
                    return Ok((frame, fds));
                }
            }
            let mut tmp = [0u8; 8192];
            // Never read past this frame: a spill takes the next frame's fds (module doc).
            let want = if leftover.len() < 4 {
                4 - leftover.len()
            } else {
                // Bounded by `MAX_FRAME_LEN` and short of `4 + len` (both checked above).
                let len = u32::from_le_bytes(leftover[0..4].try_into().unwrap()) as usize;
                4 + len - leftover.len()
            };
            let want = want.min(tmp.len());
            let mut anc = RecvAncillaryBuffer::new(&mut space);
            // `MSG_CMSG_CLOEXEC` is Linux-only; `FD_CLOEXEC` is set on each fd below.
            let r = loop {
                match rustix::net::recvmsg(
                    &self.stream,
                    &mut [IoSliceMut::new(&mut tmp[..want])],
                    &mut anc,
                    RecvFlags::empty(),
                ) {
                    Ok(r) => break r,
                    // EINTR retry.
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => {
                        // As `read_header`: idle deadline is `Timeout`, mid-frame our cut.
                        let io_err = std::io::Error::from(e);
                        if super::is_timeout(&io_err) {
                            return Err(if leftover.is_empty() && fds.is_empty() {
                                RpcError::Timeout
                            } else {
                                RpcError::DeadlineMidFrame
                            });
                        }
                        // Past the first byte or fd, an `EndOfStream` is a cut.
                        return Err(match RpcError::from(io_err) {
                            RpcError::EndOfStream if !leftover.is_empty() || !fds.is_empty() => {
                                RpcError::Truncated
                            }
                            other => other,
                        });
                    }
                }
            };
            // `MSG_CTRUNC`: surplus fds were dropped; reject, as AOSP `OS_unix_base.cpp` (EPIPE).
            if r.flags.contains(ReturnFlags::CTRUNC) {
                return Err(RpcError::Protocol(
                    "SCM_RIGHTS control message truncated (too many fds in one message)",
                ));
            }
            for msg in anc.drain() {
                if let RecvAncillaryMessage::ScmRights(iter) = msg {
                    for fd in iter {
                        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)
                            .map_err(std::io::Error::from)?;
                        fds.push(fd);
                        if fds.len() > MAX_FDS_PER_FRAME {
                            return Err(RpcError::Protocol("too many fds in one RPC frame"));
                        }
                    }
                }
            }
            if r.bytes == 0 {
                return Err(if leftover.is_empty() && fds.is_empty() {
                    RpcError::EndOfStream
                } else {
                    RpcError::Truncated
                });
            }
            leftover.extend_from_slice(&tmp[..r.bytes]);
        }
    }
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

    /// An fd send to a closed peer fails with `EPIPE`: a `SIGPIPE` would kill a host whose runtime
    /// does not ignore it. The harness ignores it, so the sends run in a child at `SIG_DFL`.
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

    /// `shutdown` drops the cut-frame prefix left in `fd_recv_buf`; see module doc "Tests".
    #[test]
    fn unix_shutdown_drops_the_fd_mode_leftover() {
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
        assert!(
            !b.fd_recv_buf.lock().unwrap().is_empty(),
            "the prefix of the cut frame is buffered"
        );
        b.shutdown().expect("shutdown");
        assert!(
            b.fd_recv_buf.lock().unwrap().is_empty(),
            "shutdown must drop what this end had buffered"
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
}
