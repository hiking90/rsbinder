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

use std::os::unix::net::UnixStream;
use std::path::Path;

use super::{read_frame, write_frame, PeerIdentity, RpcTransport};
use crate::rpc::RpcResult;

/// A framed transport over a connected Unix domain socket.
pub struct UnixTransport {
    stream: UnixStream,
    peer: PeerIdentity,
    desc: String,
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
        Ok(UnixTransport { stream, peer, desc })
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
/// * Linux: real `SO_PEERCRED` (the peer's actual uid/pid).
/// * Other Unix (macOS/BSD): `SO_PEERCRED` is unavailable, so this
///   reports the **current process** identity. That is exact for a
///   same-process `socketpair` and a documented best-effort for a
///   same-host connected socket; cross-credential ACL on non-Linux
///   must not rely on it (subplan 2-4 may add `LOCAL_PEERCRED`).
fn resolve_peer(stream: &UnixStream) -> PeerIdentity {
    #[cfg(target_os = "linux")]
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stream;
        super::mem::self_identity()
    }
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
