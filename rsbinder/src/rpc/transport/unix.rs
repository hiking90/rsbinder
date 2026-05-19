// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Unix-domain-socket transport (subplan 2-1).
//!
//! Trust boundary: filesystem permissions on the socket path plus
//! `SO_PEERCRED`. Plaintext is *correct* here — the kernel is the trust
//! boundary (plan 2 §5, the original cross-domain bridge use case).
//!
//! This subplan provides connected-stream wrapping, a `socketpair`
//! constructor for tests, and a `connect(path)` convenience. The
//! server-side `bind`/`listen`/`accept` loop is subplan 2-3.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use super::{read_frame, write_frame, PeerIdentity, RpcTransport, MAX_FRAME_LEN};
use crate::rpc::{RpcError, RpcResult};

/// Max fds a single RPC frame may carry (DoS bound — plan 2-7.d /
/// AC-7.5). Well under the kernel `SCM_MAX_FD` (253).
const MAX_FDS_PER_FRAME: usize = 64;

/// A framed transport over a connected Unix domain socket.
pub struct UnixTransport {
    stream: UnixStream,
    peer: PeerIdentity,
    desc: String,
    /// Buffered `recvmsg` leftover, used **only** by the
    /// `recv_frame_with_fds` (SCM_RIGHTS) path so a connection in
    /// `Unix` fd-mode never mixes `Read` and `recvmsg` on the same fd.
    /// The default (no-fd) path is untouched (AC-7.1 bit-identical).
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
        // `SocketFlags::CLOEXEC` is `cfg(not(apple))` in rustix
        // (Apple has no `SOCK_CLOEXEC`), so create without it and set
        // `FD_CLOEXEC` explicitly — portable Linux + macOS.
        let (a, b) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .map_err(std::io::Error::from)?;
        for fd in [&a, &b] {
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)
                .map_err(std::io::Error::from)?;
        }
        Ok((
            Self::from_stream(UnixStream::from(a))?,
            Self::from_stream(UnixStream::from(b))?,
        ))
    }

    /// Connect to a listening Unix socket at `path` (client side).
    /// `bind`/`listen`/`accept` is subplan 2-3.
    pub fn connect(path: impl AsRef<Path>) -> RpcResult<Self> {
        // `RpcError: From<std::io::Error>` — `?` does the conversion.
        let stream = UnixStream::connect(path)?;
        Self::from_stream(stream)
    }
}

/// Resolve the peer identity of a connected Unix socket.
///
/// * **Linux**: real `SO_PEERCRED` (the peer's actual uid/pid).
/// * **macOS / BSD** (subplan 2-9 Phase A): real `getpeereid` (peer
///   effective uid) + `LOCAL_PEERPID` (peer pid on macOS). This is the
///   **true peer** for an accepted cross-process socket, and *this
///   process* for a `socketpair` (correct — both ends are us; Phase
///   A.0 measured Darwin 25.4: `getpeereid`/`LOCAL_PEERPID` succeed for
///   both and return self for a socketpair, the real peer for an
///   accepted fd). A `getpeereid` failure is **never** reported as a
///   forged `Local`: a same-process / unconnected errno
///   (`ENOTCONN`/`EINVAL`) falls back to the self identity (still the
///   correct answer there), any other error to
///   [`PeerIdentity::Anonymous`] (no ACL possible — logged loudly).
///
/// Before Phase A the non-Linux arm reported the **current process**
/// for *every* socket — i.e. a server's `peer_identity()` for an
/// accepted cross-process connection was the *server itself*, an
/// authoritative-looking forged identity (the §0 contract defect).
fn resolve_peer(stream: &UnixStream) -> PeerIdentity {
    // Android's bionic has no `getpeereid`, but its kernel supports
    // `SO_PEERCRED` exactly like Linux — so android takes the Linux
    // arm (otherwise the `not(target_os="linux")` BSD arm would pull in
    // `libc::getpeereid` and break the aarch64-linux-android build,
    // e.g. the subplan 2-8/2-11 emulator interop harness).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        match rustix::net::sockopt::socket_peercred(stream) {
            Ok(ucred) => PeerIdentity::Local {
                uid: ucred.uid.as_raw(),
                pid: ucred.pid.as_raw_nonzero().get(),
            },
            // A socket without peer creds (rare) is anonymous, not a
            // forged local identity.
            Err(_) => PeerIdentity::Anonymous,
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

/// macOS/BSD peer resolution — the subplan 2-9 Phase A failure-mode
/// -driven fallback ladder. `getpeereid` is connect-time
/// kernel-vouched (the BSD analogue of `SO_PEERCRED`).
#[cfg(all(unix, not(target_os = "linux"), not(target_os = "android")))]
fn resolve_peer_bsd(stream: &UnixStream) -> PeerIdentity {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();

    let mut euid: libc::uid_t = 0;
    let mut egid: libc::gid_t = 0;
    // SAFETY: `fd` is a valid socket fd owned by `stream` for the
    // duration of this call; `euid`/`egid` are valid, initialized,
    // correctly-typed out-params. `getpeereid` does not retain `fd`.
    let rc = unsafe { libc::getpeereid(fd, &mut euid, &mut egid) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error();
        return match errno {
            // No peer credentials: an unconnected fd, or a socketpair
            // on a BSD that does not populate peercred over the pipe
            // path. Either way it is the *same process* (there is no
            // remote peer), so the self identity is the correct,
            // non-forged answer and the hermetic socketpair test still
            // holds on such platforms (Phase A ladder branch 2 —
            // defensive; not reached on Darwin 25.4, where a
            // socketpair *does* populate peercred → branch 1 → self).
            Some(libc::ENOTCONN) | Some(libc::EINVAL) => super::mem::self_identity(),
            // Any other error: NEVER a forged `Local`. No ACL is
            // possible against an unknown peer — surface it loudly.
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

/// Peer pid via `LOCAL_PEERPID` (macOS 10.8+). Other BSDs have no such
/// option, so the pid is reported as `-1` (the [`PeerIdentity::Local`]
/// contract documents `-1` = unavailable) — the uid from `getpeereid`
/// is still authoritative.
#[cfg(target_os = "macos")]
fn peer_pid(fd: std::os::fd::RawFd) -> i32 {
    let mut pid: libc::pid_t = -1;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: valid socket `fd`; `pid` and `len` are valid,
    // correctly-sized out-params for the `LOCAL_PEERPID` getsockopt.
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
        // `&UnixStream` implements Write, so a shared `&self` can send
        // while another thread receives (full-duplex, no lock needed).
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
        w.write_all(buf).map_err(RpcError::from)?;
        w.flush().map_err(RpcError::from)?;
        Ok(())
    }

    /// Raw, unframed read (one `read`; `Ok(0)` = peer closed). The
    /// android-13+ profile drives `RpcWireHeader`-based framing on top
    /// of this (`wire_android13::read_aosp_message`).
    fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
        let mut r = &self.stream;
        r.read(buf).map_err(RpcError::from)
    }

    /// Raw, **unframed** write + `SCM_RIGHTS` (subplan 2-11 Phase A0:
    /// the android-13+ v1+ `Unix` FD-over-RPC path). Identical to
    /// [`UnixTransport::send_frame_with_fds`] minus the 4-byte length
    /// prefix — the AOSP RPC wire has none. The fds ride the **first**
    /// `sendmsg` (AOSP `RpcTransportRaw::interruptableWriteFully`,
    /// `sentFds |= ret > 0`); the rest (rare — fd transactions are
    /// tiny) follow without ancillary.
    fn send_raw_with_fds(&self, buf: &[u8], fds: &[std::os::fd::BorrowedFd<'_>]) -> RpcResult<()> {
        use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
        use std::io::IoSlice;
        use std::mem::MaybeUninit;

        if fds.is_empty() {
            return self.send_raw(buf);
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
                let ok = anc.push(SendAncillaryMessage::ScmRights(fds));
                debug_assert!(ok, "cmsg_space sized for exactly these fds");
            }
            let n = rustix::net::sendmsg(
                &self.stream,
                &[IoSlice::new(&buf[sent..])],
                &mut anc,
                SendFlags::empty(),
            )
            .map_err(std::io::Error::from)?;
            if n == 0 {
                return Err(RpcError::PeerClosed);
            }
            sent += n;
        }
        Ok(())
    }

    /// Raw, **unframed** read (one `recvmsg`) + any `SCM_RIGHTS` fds
    /// (subplan 2-11 Phase A0). Pairs with
    /// [`UnixTransport::send_raw_with_fds`]; received fds are
    /// `O_CLOEXEC` (set explicitly — `MSG_CMSG_CLOEXEC` is Linux-only).
    /// `Ok((0, _))` ⇒ peer closed. Unlike
    /// [`UnixTransport::recv_frame_with_fds`] there is **no** leftover
    /// buffer: the android-13+ message reader (`read_aosp_message
    /// _with_fds`) drives exact header/body byte counts and accumulates
    /// fds across those `recvmsg`s (AOSP
    /// `RpcTransportRaw::interruptableReadFully`).
    fn recv_raw_with_fds(&self, buf: &mut [u8]) -> RpcResult<(usize, Vec<std::os::fd::OwnedFd>)> {
        use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
        use std::io::IoSliceMut;
        use std::mem::MaybeUninit;

        let mut fds: Vec<std::os::fd::OwnedFd> = Vec::new();
        let mut space =
            vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_PER_FRAME))];
        let mut anc = RecvAncillaryBuffer::new(&mut space);
        let r = rustix::net::recvmsg(
            &self.stream,
            &mut [IoSliceMut::new(buf)],
            &mut anc,
            RecvFlags::empty(),
        )
        .map_err(std::io::Error::from)?;
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

    /// Send `buf` as a length-prefixed frame, passing `fds` out-of-band
    /// via `SCM_RIGHTS` (subplan 2-7, `Unix` fd-mode). The ancillary
    /// fds ride the **first** `sendmsg`; remaining bytes (rare — fd
    /// transactions are tiny) follow without ancillary.
    fn send_frame_with_fds(
        &self,
        buf: &[u8],
        fds: &[std::os::fd::BorrowedFd<'_>],
    ) -> RpcResult<()> {
        use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
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
                let ok = anc.push(SendAncillaryMessage::ScmRights(fds));
                debug_assert!(ok, "cmsg_space sized for exactly these fds");
            }
            let n = rustix::net::sendmsg(
                &self.stream,
                &[IoSlice::new(&framed[sent..])],
                &mut anc,
                SendFlags::empty(),
            )
            .map_err(std::io::Error::from)?;
            if n == 0 {
                return Err(RpcError::PeerClosed);
            }
            sent += n;
        }
        Ok(())
    }

    /// Receive one length-prefixed frame plus any `SCM_RIGHTS` fds.
    /// Received fds are `O_CLOEXEC` (`RecvFlags::CMSG_CLOEXEC`).
    /// Connections in `Unix` fd-mode use this for *every* frame, so
    /// `recvmsg` and `Read` are never mixed on one fd.
    fn recv_frame_with_fds(&self) -> RpcResult<(Vec<u8>, Vec<std::os::fd::OwnedFd>)> {
        use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
        use std::io::IoSliceMut;
        use std::mem::MaybeUninit;

        let mut leftover = self.fd_recv_buf.lock().expect("fd recv buf poisoned");
        let mut fds: Vec<std::os::fd::OwnedFd> = Vec::new();
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
            let mut space =
                vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_PER_FRAME))];
            let mut anc = RecvAncillaryBuffer::new(&mut space);
            // `RecvFlags::CMSG_CLOEXEC` (`MSG_CMSG_CLOEXEC`) is
            // Linux-only; for portability set `FD_CLOEXEC` explicitly
            // on each received fd (plan 2-7.d "received fd O_CLOEXEC").
            let r = rustix::net::recvmsg(
                &self.stream,
                &mut [IoSliceMut::new(&mut tmp)],
                &mut anc,
                RecvFlags::empty(),
            )
            .map_err(std::io::Error::from)?;
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
                    RpcError::PeerClosed
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
        // 1 MiB + 1 crosses the u32 framing and exercises partial-read
        // reassembly in `read_body`. A large payload can exceed the
        // socket buffer, so send from a worker thread to avoid a
        // same-thread write/read deadlock.
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

    #[test]
    fn unix_peer_identity_is_this_process_for_socketpair() {
        let (a, _b) = UnixTransport::pair().expect("socketpair");
        // Both ends of a socketpair live in this process. On Linux this
        // exercises the real SO_PEERCRED syscall; elsewhere the
        // documented best-effort. Either way it must be *this* process.
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
        // First recv sees EOF -> clean PeerClosed (T1.5).
        assert!(matches!(a.recv_frame(), Err(RpcError::PeerClosed)));
    }

    #[test]
    fn unix_partial_header_then_close_is_truncated_or_closed() {
        // Send a lone (incomplete) length header then close (T1.6).
        let (a, b) = UnixTransport::pair().expect("socketpair");
        {
            use std::io::Write;
            let mut s = &a.stream;
            s.write_all(&[1u8, 0]).unwrap(); // 2 of 4 header bytes
        }
        drop(a);
        let r = b.recv_frame();
        assert!(
            matches!(r, Err(RpcError::Truncated) | Err(RpcError::PeerClosed)),
            "expected Truncated/PeerClosed, got {r:?}"
        );
    }
}
