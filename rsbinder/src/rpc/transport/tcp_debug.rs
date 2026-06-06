// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! **INSECURE** plaintext TCP transport — DEBUG / interop bring-up
//! ONLY. Gated behind the `rpc-tcp-debug` feature so it is absent from
//! a plain `rpc` build.
//!
//! Android precedent: android-12 r34 RPC binder has `SocketType::INET`
//! and `binderRpcTest.cpp` uses an INET loopback as a *test* transport.
//! This is the rsbinder equivalent — for observing the R34 wire with
//! `tcpdump`, for live r34 interop over standard INET loopback, and for
//! macOS development (no `SO_PEERCRED`).
//!
//! **Never production.** Safeguards baked in by type/construction:
//! * [`PeerIdentity::Anonymous`] is hard-wired — there is no code path
//!   that returns any other identity, so ACL is impossible by
//!   construction.
//! * Default bind is loopback (`127.0.0.1`); a non-loopback bind is a
//!   separate, explicitly-named, warned constructor.
//! * A loud one-time warning is logged the first time this transport is
//!   used in a process.
//! * For real networks use the `tls` backend instead.

use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{read_frame, write_frame, PeerIdentity, RpcTransport};
use crate::rpc::RpcResult;

/// Set the first time *any* `TcpDebugTransport` is constructed in this
/// process. Drives the one-time insecure warning and lets tests assert
/// the warning fired without capturing the log backend.
static INSECURE_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_once() {
    if !INSECURE_WARNED.swap(true, Ordering::SeqCst) {
        log::warn!(
            "INSECURE plaintext TCP transport — debug/interop only, \
             NEVER production; use the `tls` backend for real networks"
        );
    }
}

/// `true` once the process-wide insecure warning has been emitted.
/// Test/diagnostic hook.
pub fn insecure_warning_emitted() -> bool {
    INSECURE_WARNED.load(Ordering::SeqCst)
}

/// A framed transport over a plaintext TCP connection. Debug/interop
/// only — see the module docs.
pub struct TcpDebugTransport {
    stream: TcpStream,
    desc: String,
}

impl TcpDebugTransport {
    /// Wrap an accepted/connected `TcpStream` (sets `TCP_NODELAY`).
    pub fn from_stream(stream: TcpStream) -> RpcResult<Self> {
        warn_once();
        // `RpcError: From<std::io::Error>` — `?` converts directly.
        stream.set_nodelay(true)?;
        let desc = match stream.peer_addr() {
            Ok(a) => format!("tcp_debug:{a}"),
            Err(_) => "tcp_debug".to_string(),
        };
        Ok(TcpDebugTransport { stream, desc })
    }

    /// Wrap a preconnected `OwnedFd` (the `IAccessor::addConnection()`
    /// fd-adopt path, `AF_INET` family).
    /// `std`'s `From<OwnedFd> for TcpStream` is stable cross-platform;
    /// the caller is responsible for asserting the fd's address family.
    pub fn from_owned_fd(fd: OwnedFd) -> RpcResult<Self> {
        Self::from_stream(TcpStream::from(fd))
    }

    /// Connect to `addr` (client side). Warns; identity is always
    /// [`PeerIdentity::Anonymous`].
    pub fn connect(addr: SocketAddr) -> RpcResult<Self> {
        let stream = TcpStream::connect(addr)?;
        Self::from_stream(stream)
    }

    /// Bind a listener on an ephemeral **loopback** port. The default,
    /// safe bind. Returns the raw listener for tests / bring-up.
    pub fn bind_loopback() -> RpcResult<TcpListener> {
        warn_once();
        Ok(TcpListener::bind(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            0,
        )))?)
    }

    /// Bind a listener on a caller-chosen, possibly **non-loopback**
    /// address. Separate name + extra warning so a non-loopback bind is
    /// always an explicit, visible choice (never an accident).
    pub fn bind_insecure(addr: SocketAddr) -> RpcResult<TcpListener> {
        warn_once();
        if !addr.ip().is_loopback() {
            log::warn!(
                "tcp_debug bound to NON-loopback {addr} — reachable off-host, \
                 plaintext, no peer identity; debug/interop ONLY"
            );
        }
        Ok(TcpListener::bind(addr)?)
    }

    /// Connected loopback pair for hermetic tests (analogous to
    /// `UnixTransport::pair`): bind ephemeral loopback, connect, accept.
    pub fn pair_loopback() -> RpcResult<(Self, Self)> {
        let listener = Self::bind_loopback()?;
        let addr = listener.local_addr()?;
        let client = Self::connect(addr)?;
        let (server_stream, _) = listener.accept()?;
        let server = Self::from_stream(server_stream)?;
        Ok((client, server))
    }
}

impl RpcTransport for TcpDebugTransport {
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
        let mut w = &self.stream;
        write_frame(&mut w, buf)
    }

    fn recv_frame(&self) -> RpcResult<Vec<u8>> {
        let mut r = &self.stream;
        read_frame(&mut r)
    }

    /// **Always** [`PeerIdentity::Anonymous`]. There is deliberately no
    /// other return path: plaintext TCP carries no trustworthy peer
    /// identity, so ACL against it is impossible *by type*.
    fn peer_identity(&self) -> PeerIdentity {
        PeerIdentity::Anonymous
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn tcp_debug_roundtrip_and_safeguards() {
        let (client, server) = TcpDebugTransport::pair_loopback().expect("loopback pair");

        // Identity is hard-wired Anonymous on both ends.
        assert_eq!(client.peer_identity(), PeerIdentity::Anonymous);
        assert_eq!(server.peer_identity(), PeerIdentity::Anonymous);
        // The one-time insecure warning must have fired by now.
        assert!(insecure_warning_emitted());

        let client = Arc::new(client);
        for size in [0usize, 1, 64 * 1024, 1 << 20] {
            let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let sender = {
                let c = client.clone();
                let p = payload.clone();
                std::thread::spawn(move || c.send_frame(&p).unwrap())
            };
            assert_eq!(server.recv_frame().expect("recv"), payload, "size {size}");
            sender.join().unwrap();
        }
    }

    /// `from_owned_fd` adopt → frame roundtrip. Mirrors the `unix`
    /// counterpart but uses the TCP loopback pair.
    #[test]
    fn tcp_debug_from_owned_fd_roundtrip() {
        use std::os::fd::OwnedFd;

        // Bind a listener, connect a client, accept on the server end —
        // exactly what an external bridge would hand us as two fds.
        let listener = TcpDebugTransport::bind_loopback().expect("bind");
        let addr = listener.local_addr().unwrap();
        let client_stream = TcpStream::connect(addr).expect("connect");
        let (server_stream, _) = listener.accept().expect("accept");

        let client_fd: OwnedFd = client_stream.into();
        let server_fd: OwnedFd = server_stream.into();
        let client = TcpDebugTransport::from_owned_fd(client_fd).expect("adopt client");
        let server = TcpDebugTransport::from_owned_fd(server_fd).expect("adopt server");

        let payload = b"hello-accessor-tcp".to_vec();
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
    fn tcp_debug_default_bind_is_loopback() {
        let l = TcpDebugTransport::bind_loopback().expect("bind");
        assert!(
            l.local_addr().unwrap().ip().is_loopback(),
            "default bind must be loopback"
        );
    }
}
