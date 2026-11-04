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
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

use super::unix::{send_no_wait_no_fds, send_raw_no_fds, wait_for_room, Carry};
use super::{OutQueue, PeerIdentity, ReadDeadline, RpcTransport};
use crate::rpc::RpcResult;

/// Set on the first `TcpDebugTransport` construction; gates the one-time warning, read by tests.
static INSECURE_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_once() {
    if !INSECURE_WARNED.swap(true, Ordering::SeqCst) {
        log::warn!(
            "INSECURE plaintext TCP transport — debug/interop only, \
             NEVER production; use the `tls` backend for real networks"
        );
    }
}

/// A framed transport over a plaintext TCP connection. Debug/interop
/// only — see the module docs.
pub struct TcpDebugTransport {
    stream: TcpStream,
    desc: String,
    reads: ReadDeadline,
    queue: OutQueue,
    carry: Carry,
}

impl std::fmt::Debug for TcpDebugTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpDebugTransport")
            .field("desc", &self.desc)
            .finish_non_exhaustive()
    }
}

impl TcpDebugTransport {
    /// Wrap an accepted/connected `TcpStream` (sets `TCP_NODELAY`).
    pub fn from_stream(stream: TcpStream) -> crate::Result<Self> {
        Ok(Self::wrap(stream)?)
    }

    fn wrap(stream: TcpStream) -> RpcResult<Self> {
        warn_once();
        // No `MSG_NOSIGNAL` on Apple, as `UnixTransport::from_stream`.
        #[cfg(target_vendor = "apple")]
        super::set_nosigpipe(stream.as_fd())?;
        // `RpcError: From<std::io::Error>` — `?` converts directly.
        stream.set_nodelay(true)?;
        let desc = match stream.peer_addr() {
            Ok(a) => format!("tcp_debug:{a}"),
            Err(_) => "tcp_debug".to_string(),
        };
        Ok(TcpDebugTransport {
            stream,
            desc,
            reads: ReadDeadline::default(),
            queue: OutQueue::default(),
            carry: Carry::default(),
        })
    }

    /// Wrap a preconnected `OwnedFd` (the `IAccessor::addConnection()`
    /// fd-adopt path, `AF_INET` family).
    /// `std`'s `From<OwnedFd> for TcpStream` is stable cross-platform;
    /// the caller is responsible for asserting the fd's address family.
    ///
    /// The fd must be in blocking mode: a read on an `O_NONBLOCK` fd that
    /// finds no data returns `RpcError::Timeout` at once. An Accessor fd
    /// arrives non-blocking; [`crate::rpc::RpcSession::from_preconnected_fd`]
    /// clears the flag, this does not.
    pub fn from_owned_fd(fd: OwnedFd) -> crate::Result<Self> {
        Self::from_stream(TcpStream::from(fd))
    }

    /// Connect to `addr` (client side). Warns; identity is always
    /// [`PeerIdentity::Anonymous`].
    pub fn connect(addr: SocketAddr) -> crate::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        Self::from_stream(stream)
    }

    /// Bind a listener on an ephemeral **loopback** port. The default,
    /// safe bind. Returns the raw listener for tests / bring-up.
    pub fn bind_loopback() -> crate::Result<TcpListener> {
        warn_once();
        Ok(TcpListener::bind(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            0,
        )))?)
    }

    /// Bind a listener on a caller-chosen, possibly **non-loopback**
    /// address. Separate name + extra warning so a non-loopback bind is
    /// always an explicit, visible choice (never an accident).
    pub fn bind_insecure(addr: SocketAddr) -> crate::Result<TcpListener> {
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
    pub fn pair_loopback() -> crate::Result<(Self, Self)> {
        let listener = Self::bind_loopback()?;
        let addr = listener.local_addr()?;
        let client = Self::connect(addr)?;
        let (server_stream, _) = listener.accept()?;
        let server = Self::from_stream(server_stream)?;
        Ok((client, server))
    }

    /// Fill the socket until it has stayed full, then carry half of a frame the socket did not
    /// begin; the bytes the peer reads through the end of it, counted and not framed.
    #[cfg(test)]
    pub(crate) fn fill_and_carry(&self) -> usize {
        let sock = self.stream.as_fd();
        let chunk = [7u8; 16 * 1024];
        let mut total = 0;
        // What was queued can still move to the peer's buffer after the first `EAGAIN`.
        let mut refused = 0;
        while refused < 10 {
            match super::unix::send_nonblocking(sock, |flags| {
                rustix::net::send(sock, &chunk, flags)
            }) {
                Ok(n) => {
                    total += n;
                    refused = 0;
                }
                Err(rustix::io::Errno::AGAIN) => {
                    refused += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => panic!("filling the socket: {e}"),
            }
        }
        let frame = [7u8; 2048];
        let mut first = true;
        let taken = self.carry.take_frame(&frame, |rest| {
            if std::mem::take(&mut first) {
                Ok(rest.len() / 2)
            } else {
                Err(rustix::io::Errno::AGAIN)
            }
        });
        assert!(matches!(taken, Ok(true)));
        total + frame.len() / 2
    }
}

impl RpcTransport for TcpDebugTransport {
    fn send_raw_draining(
        &self,
        buf: &[u8],
        fds: &[std::os::fd::BorrowedFd<'_>],
        drain: &mut dyn FnMut() -> RpcResult<()>,
    ) -> RpcResult<()> {
        let sock = self.stream.as_fd();
        send_raw_no_fds(sock, &self.queue, &self.carry, buf, fds, Some(drain))
    }

    fn __send_no_wait(
        &self,
        buf: &[u8],
        fds: &[std::os::fd::BorrowedFd<'_>],
    ) -> Option<RpcResult<bool>> {
        Some(send_no_wait_no_fds(
            self.stream.as_fd(),
            &self.carry,
            buf,
            fds,
        ))
    }

    fn __carrying(&self) -> bool {
        !self.carry.is_empty()
    }

    fn __flush_carried(&self) -> RpcResult<()> {
        self.carry.flush(self.stream.as_fd(), &self.queue, None)
    }

    fn __wait_for_room(&self) -> RpcResult<bool> {
        wait_for_room(self.stream.as_fd(), &self.queue)
    }

    /// Raw, unframed write for the android-13+ profile (the real android
    /// RPC wire has no length prefix), as `VsockTransport`'s;
    /// the trait default refuses raw access, which is right only for a
    /// frame-only backend and wrong here — `RpcSession::from_preconnected_fd`
    /// wraps an `AF_INET` fd in this transport and goes straight into the
    /// android-13+ handshake, whose first byte is a raw write.
    fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
        send_raw_no_fds(
            self.stream.as_fd(),
            &self.queue,
            &self.carry,
            buf,
            &[],
            None,
        )
    }

    /// Raw, unframed read: `read_raw`, shared by the stream backends.
    fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
        self.carry
            .write_for_read(self.stream.as_fd(), &self.queue)?;
        let sock = Some(self.stream.as_fd());
        self.reads.read(sock, &self.queue, || {
            super::read_raw(&mut &self.stream, buf)
        })
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
        self.reads.note(timeout);
        Ok(())
    }

    fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        self.stream.set_write_timeout(timeout)?;
        Ok(())
    }

    fn set_liveness(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        use std::os::fd::AsFd;
        super::tcp_liveness(self.stream.as_fd(), timeout)?;
        Ok(())
    }

    fn shutdown(&self) -> RpcResult<()> {
        super::absorb_already_shut(self.stream.shutdown(std::net::Shutdown::Both))
    }

    fn peer_closed(&self) -> Option<bool> {
        use std::os::fd::AsFd;
        super::socket_peer_closed(self.stream.as_fd(), super::SocketKind::TcpOrVsock)
    }

    fn shutdown_handle(&self) -> Option<Box<dyn FnOnce() + Send>> {
        super::socket_shutdown_handle(self.stream.as_fd())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::RpcError;
    use std::sync::Arc;

    #[test]
    fn tcp_debug_roundtrip_and_safeguards() {
        let (client, server) = TcpDebugTransport::pair_loopback().expect("loopback pair");

        // Identity is hard-wired Anonymous on both ends.
        assert_eq!(client.peer_identity(), PeerIdentity::Anonymous);
        assert_eq!(server.peer_identity(), PeerIdentity::Anonymous);

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

    /// `from_owned_fd` adopt → frame roundtrip over TCP loopback, mirroring the `unix` test.
    #[test]
    fn tcp_debug_from_owned_fd_roundtrip() {
        use std::os::fd::OwnedFd;

        // Connect + accept: the two fds an external bridge would hand us.
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

    /// Plan 2-21: TCP serves the raw (unframed) android-13+ path the trait default refuses.
    #[test]
    fn tcp_debug_raw_bytes_roundtrip() {
        let (client, server) = TcpDebugTransport::pair_loopback().expect("loopback pair");
        client.send_raw(b"\x01\x02\x03").expect("send_raw");
        let mut buf = [0u8; 8];
        let n = server.recv_raw(&mut buf).expect("recv_raw");
        assert_eq!(&buf[..n], b"\x01\x02\x03");
    }

    /// `len` bytes from `peer`, whatever the reads they take.
    fn read_all(peer: &TcpDebugTransport, len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        let mut read = 0;
        while read < len {
            let n = peer.recv_raw(&mut bytes[read..]).expect("the peer's read");
            assert!(n > 0, "the end of stream after {read} of {len} bytes");
            read += n;
        }
        bytes
    }

    /// A pair whose sender put 100 bytes of `FRAME` on the socket and carries the rest.
    fn pair_carrying_part_of_a_frame() -> (TcpDebugTransport, TcpDebugTransport) {
        let (sender, peer) = TcpDebugTransport::pair_loopback().expect("loopback pair");
        peer.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("peer timeout");
        let sock = sender.stream.as_fd();
        let mut first = true;
        let taken = sender.carry.take_frame(&FRAME, |rest| {
            if std::mem::take(&mut first) {
                rustix::net::send(sock, &rest[..100], super::super::unix::SEND_FLAGS)
            } else {
                Err(rustix::io::Errno::AGAIN)
            }
        });
        assert!(matches!(taken, Ok(true)));
        assert!(sender.__carrying());
        (sender, peer)
    }

    const FRAME: [u8; 2048] = [5; 2048];

    /// A later send writes what a no-wait send left first, so no frame is cut.
    #[test]
    fn a_later_send_writes_what_a_no_wait_send_left_first() {
        let (sender, peer) = pair_carrying_part_of_a_frame();
        sender.send_raw(b"tail").expect("the send");
        let read = read_all(&peer, FRAME.len() + 4);
        assert_eq!(read[..FRAME.len()], FRAME);
        assert_eq!(&read[FRAME.len()..], b"tail");
        assert!(!sender.__carrying());
    }

    /// `__flush_carried` alone finishes the frame, for a stream with nothing to write next.
    #[test]
    fn a_flush_finishes_what_a_no_wait_send_left() {
        let (sender, peer) = pair_carrying_part_of_a_frame();
        sender.__flush_carried().expect("the flush");
        assert_eq!(read_all(&peer, FRAME.len()), FRAME);
        assert!(!sender.__carrying());
    }

    /// While part of a frame is carried, a no-wait send takes no other frame.
    #[test]
    fn a_no_wait_send_takes_no_frame_while_part_of_one_is_carried() {
        let (sender, peer) = pair_carrying_part_of_a_frame();
        assert!(matches!(sender.__send_no_wait(b"x", &[]), Some(Ok(false))));
        sender.__flush_carried().expect("the flush");
        assert!(matches!(sender.__send_no_wait(b"x", &[]), Some(Ok(true))));
        let read = read_all(&peer, FRAME.len() + 1);
        assert_eq!(read[..FRAME.len()], FRAME);
        assert_eq!(read[FRAME.len()], b'x');
    }

    /// A pair whose sender's socket takes no more and whose `Carry` holds bytes.
    fn full_socket_with_part_of_a_frame() -> (TcpDebugTransport, TcpDebugTransport, usize) {
        let (sender, peer) = TcpDebugTransport::pair_loopback().expect("loopback pair");
        peer.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("peer timeout");
        let total = sender.fill_and_carry();
        (sender, peer, total)
    }

    /// Input for a read with no room to write: a peer writing with no read of its own goes on.
    #[test]
    fn a_read_with_input_waits_for_no_room() {
        let (sender, peer, _) = full_socket_with_part_of_a_frame();
        sender
            .set_write_timeout(Some(std::time::Duration::from_secs(1)))
            .expect("send deadline");
        sender
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("read deadline");
        peer.send_raw(b"x").expect("the peer's send");
        let mut byte = [0u8; 8];
        assert!(matches!(sender.recv_raw(&mut byte), Ok(1)));
        assert!(sender.__carrying(), "nothing read, so no room");
    }

    /// A peer in a drain reads for the rest of this end's frame: a read goes on to write it.
    #[test]
    fn a_read_writes_the_rest_of_a_frame_with_part_of_the_peers_read() {
        let (sender, peer, total) = full_socket_with_part_of_a_frame();
        sender
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("read deadline");
        let peer = Arc::new(peer);
        let draining = {
            let peer = peer.clone();
            std::thread::spawn(move || {
                peer.send_raw(b"head").expect("the peer's send");
                read_all(&peer, total);
                peer.send_raw(b"tail").expect("the peer's send");
            })
        };
        let mut input = [0u8; 8];
        let mut read = 0;
        while read < input.len() {
            read += sender.recv_raw(&mut input[read..]).expect("the read");
        }
        assert_eq!(&input, b"headtail");
        assert!(!sender.__carrying());
        draining.join().expect("the peer");
    }

    /// The send deadline holds in a read too, and it reports the frame it cut.
    #[test]
    fn a_read_past_the_send_deadline_is_a_cut_frame() {
        let (sender, _peer, _) = full_socket_with_part_of_a_frame();
        sender
            .set_write_timeout(Some(std::time::Duration::from_millis(100)))
            .expect("send deadline");
        sender
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("read deadline");
        let mut byte = [0u8; 8];
        let read = sender.recv_raw(&mut byte);
        assert!(matches!(read, Err(RpcError::DeadlineMidFrame)), "{read:?}");
    }

    /// A flush past the send deadline stops a frame whose head went out: not a boundary `Timeout`.
    #[test]
    fn a_flush_past_the_send_deadline_is_not_a_boundary_timeout() {
        let (sender, _peer, _) = full_socket_with_part_of_a_frame();
        sender
            .set_write_timeout(Some(std::time::Duration::from_millis(100)))
            .expect("send deadline");
        let flushed = sender.__flush_carried();
        assert!(
            matches!(&flushed, Err(RpcError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock),
            "{flushed:?}"
        );
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
