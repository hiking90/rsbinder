// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! In-process transport for hermetic tests.
//!
//! No sockets, no kernel: a pair of `mpsc` channels. Two `MemTransport`s
//! from [`MemTransport::pair`] are wired cross-over so a write on one is a
//! read on the other.
//!
//! It carries either kind of traffic the trait defines. Through
//! `send_frame`/`recv_frame` one channel message **is** one frame, so the
//! length-prefix framing is bypassed entirely. Through `send_raw`/`recv_raw`
//! it is a byte stream, as the socket backends are: each `send_raw` queues
//! its bytes as one message, and `recv_raw` hands them out across as many
//! reads as the caller's buffers take, keeping the unread rest of a message
//! for the next read. So the AOSP framing that the android-13+ profile
//! drives itself runs over `mem` byte for byte as it runs over a socket.
//! One direction carries one kind: a `recv_frame` while a raw read has left
//! part of a message unread is refused, since a socket stream at that
//! point would not be at a frame boundary either.
//!
//! `shutdown` models a **Linux** socket, deliberately: frames and bytes
//! already queued on either side are still delivered, then the end of
//! stream; later sends on either side fail. Linux is the deployment target,
//! and it is the platform where a caller that assumes a shutdown discards
//! what was queued is wrong — a hermetic backend that modelled the other
//! platform (macOS drops its queue) would certify that assumption on the
//! developer's machine and let it break on the device.
//!
//! Each end's shutdown sets a flag shared with the peer. Once the queue is
//! empty, `recv_frame` reports `EndOfStream` and `recv_raw` `Ok(0)`. A
//! blocked read notices the flag within one poll tick; a sender into its
//! own `rx` would wake it sooner, but would also keep the channel alive past
//! the peer's drop and hide the end of stream.
//! `mem_shutdown_models_a_linux_socket` pins this, so a teardown bug a real
//! transport would surface on the device is visible to every hermetic test.
//!
//! There is no global state — every test makes its own independent
//! pair, so the RPC test suite is parallel-safe by construction.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use super::{PeerIdentity, RpcTransport};
use crate::rpc::{RpcError, RpcResult};

/// An in-process, in-memory endpoint carrying frames or a byte stream.
///
/// `tx: Sender<…>` is not wrapped in a `Mutex`: `mpsc::Sender` is
/// `Sync + Clone` and `Sender::send` takes `&self`, so a `Mutex` would
/// only serialize unrelated senders without protecting anything.
/// `Receiver` stays under `Mutex` (it is `!Sync`), together with the
/// unread rest of the message a raw read started.
pub struct MemTransport {
    tx: Sender<Vec<u8>>,
    inbox: Mutex<Inbox>,
    peer: PeerIdentity,
    desc: &'static str,
    timeout: Mutex<Option<std::time::Duration>>,
    /// Set by this end's `shutdown`, shared as the peer's `peer_closed`; see module doc.
    closed: Arc<AtomicBool>,
    /// The peer's `closed`: our end of stream and `EPIPE`, as a socket peer's `shutdown(Both)`.
    peer_closed: Arc<AtomicBool>,
}

/// The receive side: the channel, and the message `recv_raw` is partway through.
struct Inbox {
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    /// Bytes of `pending` already handed out.
    taken: usize,
}

impl Inbox {
    fn new(rx: Receiver<Vec<u8>>) -> Self {
        Inbox {
            rx,
            pending: Vec::new(),
            taken: 0,
        }
    }

    fn unread(&self) -> &[u8] {
        &self.pending[self.taken..]
    }
}

impl MemTransport {
    /// Create a connected pair. Anything sent on `.0` is received on
    /// `.1` and vice-versa. Peer identity is this process (the only
    /// possible peer for an in-process channel).
    pub fn pair() -> (Self, Self) {
        let (a_tx, a_rx) = std::sync::mpsc::channel();
        let (b_tx, b_rx) = std::sync::mpsc::channel();
        let peer = self_identity();
        let a_closed = Arc::new(AtomicBool::new(false));
        let b_closed = Arc::new(AtomicBool::new(false));
        (
            MemTransport {
                tx: a_tx,
                inbox: Mutex::new(Inbox::new(b_rx)),
                peer: peer.clone(),
                desc: "mem",
                timeout: Mutex::new(None),
                closed: Arc::clone(&a_closed),
                peer_closed: Arc::clone(&b_closed),
            },
            MemTransport {
                tx: b_tx,
                inbox: Mutex::new(Inbox::new(a_rx)),
                peer,
                desc: "mem",
                timeout: Mutex::new(None),
                closed: b_closed,
                peer_closed: a_closed,
            },
        )
    }

    fn either_end_shut(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.peer_closed.load(Ordering::SeqCst)
    }

    /// Queue one message for the peer; fails after either end's shutdown (a socket's `EPIPE`).
    fn push(&self, buf: &[u8]) -> RpcResult<()> {
        // A failed-handshake un-push needs the refusal after a shutdown.
        if self.either_end_shut() {
            return Err(RpcError::EndOfStream);
        }
        // Fails only once the peer's receiver is dropped. Lock-free (`Sender: Sync`).
        self.tx
            .send(buf.to_vec())
            .map_err(|_| RpcError::EndOfStream)
    }

    /// The next queued message, under the read deadline; `EndOfStream` once nothing more can come.
    fn next_message(&self, rx: &Receiver<Vec<u8>>) -> RpcResult<Vec<u8>> {
        let timeout = *self.timeout.lock().expect("mem timeout poisoned");
        // Short ticks so a local `shutdown` is noticed; a message or peer drop returns at once.
        const TICK: std::time::Duration = std::time::Duration::from_millis(20);
        // `Instant + Duration` panics on overflow; an unaddable timeout means `None` (infinite).
        let deadline = timeout.and_then(|d| std::time::Instant::now().checked_add(d));
        loop {
            let wait = match deadline {
                Some(at) => {
                    let left = at.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return Err(RpcError::Timeout);
                    }
                    left.min(TICK)
                }
                None => TICK,
            };
            match rx.recv_timeout(wait) {
                // Queued before a shutdown: still delivered, as a Linux socket delivers it.
                Ok(message) => return Ok(message),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(RpcError::EndOfStream);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if self.either_end_shut() {
                        return Err(RpcError::EndOfStream);
                    }
                }
            }
        }
    }
}

/// `PeerIdentity::Local` for this process: `mem`'s peer and `unix`'s BSD no-remote-peer answer.
pub(crate) fn self_identity() -> PeerIdentity {
    PeerIdentity::Local {
        uid: rustix::process::getuid().as_raw(),
        pid: std::process::id() as i32,
    }
}

impl RpcTransport for MemTransport {
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
        // Same cap as `write_frame`, so this test transport passes no frame a real one rejects.
        if buf.len() > super::MAX_FRAME_LEN {
            return Err(RpcError::FrameTooLarge {
                declared: buf.len(),
                max: super::MAX_FRAME_LEN,
            });
        }
        self.push(buf)
    }

    fn recv_frame(&self) -> RpcResult<Vec<u8>> {
        let inbox = self.inbox.lock().expect("mem inbox poisoned");
        if !inbox.unread().is_empty() {
            return Err(RpcError::Protocol(
                "mem: a frame read while a raw read left a message partly unread",
            ));
        }
        self.next_message(&inbox.rx)
    }

    /// One message of the peer's, as many reads as `buf` takes; a zero-length send moves no byte.
    fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
        // An empty message would read as `Ok(0)`, the end of stream.
        if buf.is_empty() {
            return if self.either_end_shut() {
                Err(RpcError::EndOfStream)
            } else {
                Ok(())
            };
        }
        self.push(buf)
    }

    fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut inbox = self.inbox.lock().expect("mem inbox poisoned");
        // A zero-length frame from `send_frame` carries no byte for a stream reader.
        while inbox.unread().is_empty() {
            match self.next_message(&inbox.rx) {
                Ok(message) => {
                    inbox.pending = message;
                    inbox.taken = 0;
                }
                Err(RpcError::EndOfStream) => return Ok(0),
                Err(e) => return Err(e),
            }
        }
        let n = buf.len().min(inbox.unread().len());
        buf[..n].copy_from_slice(&inbox.unread()[..n]);
        inbox.taken += n;
        Ok(n)
    }

    fn peer_identity(&self) -> PeerIdentity {
        self.peer.clone()
    }

    fn describe(&self) -> &str {
        self.desc
    }

    fn shutdown(&self) -> RpcResult<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        *self.timeout.lock().expect("mem timeout poisoned") = timeout;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn mem_roundtrip_all_sizes() {
        let (a, b) = MemTransport::pair();
        for size in [0usize, 1, 64, 64 * 1024, 1 << 20, (1 << 20) + 1] {
            let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            a.send_frame(&payload).expect("send");
            assert_eq!(b.recv_frame().expect("recv"), payload, "size {size}");
        }
    }

    /// `shutdown` behaves as on a Linux socket (module doc): queued frames, then end of stream.
    #[test]
    fn mem_shutdown_models_a_linux_socket() {
        let (a, b) = MemTransport::pair();
        a.send_frame(b"a->b before").expect("send before shutdown");
        b.send_frame(b"b->a before").expect("send before shutdown");
        a.shutdown().expect("shutdown");
        a.shutdown().expect("a second shutdown is Ok");
        // Our side: the queued frame first, then the end of stream; sends fail.
        assert_eq!(
            a.recv_frame().expect("queued frame survives"),
            b"b->a before"
        );
        assert!(matches!(a.recv_frame(), Err(RpcError::EndOfStream)));
        assert!(
            matches!(a.send_frame(b"after"), Err(RpcError::EndOfStream)),
            "a send after shutdown must fail, not queue"
        );
        // The peer drains what we sent, then sees end of stream; its sends fail (socket: EPIPE).
        assert_eq!(b.recv_frame().expect("peer drains"), b"a->b before");
        assert!(matches!(b.recv_frame(), Err(RpcError::EndOfStream)));
        assert!(matches!(b.send_frame(b"x"), Err(RpcError::EndOfStream)));
    }

    #[test]
    fn mem_peer_identity_is_current_process() {
        let (a, _b) = MemTransport::pair();
        assert_eq!(
            a.peer_identity(),
            PeerIdentity::Local {
                uid: rustix::process::getuid().as_raw(),
                pid: std::process::id() as i32,
            }
        );
        assert_eq!(a.describe(), "mem");
    }

    #[test]
    fn mem_peer_closed_on_drop() {
        let (a, b) = MemTransport::pair();
        drop(b);
        assert!(matches!(a.recv_frame(), Err(RpcError::EndOfStream)));
        assert!(matches!(a.send_frame(b"x"), Err(RpcError::EndOfStream)));
        assert_eq!(a.recv_raw(&mut [0u8; 4]).expect("raw end of stream"), 0);
        assert!(matches!(a.send_raw(b"x"), Err(RpcError::EndOfStream)));
    }

    /// Two threads cross-fire 10k frames each without deadlock, loss or reordering.
    #[test]
    fn mem_bidirectional_concurrent_no_deadlock() {
        let (a, b) = MemTransport::pair();
        let a = Arc::new(a);
        let b = Arc::new(b);
        const N: usize = 10_000;

        let a_send = {
            let a = a.clone();
            std::thread::spawn(move || {
                for i in 0..N {
                    a.send_frame(&(i as u32).to_le_bytes()).unwrap();
                }
            })
        };
        let b_send = {
            let b = b.clone();
            std::thread::spawn(move || {
                for i in 0..N {
                    b.send_frame(&(i as u32).to_le_bytes()).unwrap();
                }
            })
        };

        for i in 0..N {
            let got = b.recv_frame().unwrap();
            assert_eq!(u32::from_le_bytes(got.try_into().unwrap()), i as u32);
        }
        for i in 0..N {
            let got = a.recv_frame().unwrap();
            assert_eq!(u32::from_le_bytes(got.try_into().unwrap()), i as u32);
        }
        a_send.join().unwrap();
        b_send.join().unwrap();
    }

    /// A read timeout too large to add to `Instant::now()` means no deadline, not a panic.
    #[test]
    fn unaddable_read_timeout_does_not_panic() {
        let (a, b) = MemTransport::pair();
        a.set_read_timeout(Some(std::time::Duration::MAX))
            .expect("set_read_timeout");
        b.send_frame(b"hi").expect("send");
        assert_eq!(a.recv_frame().expect("recv"), b"hi");
    }

    /// Raw reads ignore send boundaries: they split one send and join two, as a socket's do.
    #[test]
    fn mem_raw_is_a_byte_stream() {
        let (a, b) = MemTransport::pair();
        a.send_raw(b"hello").expect("send");
        a.send_raw(b"").expect("an empty send moves nothing");
        a.send_raw(b" world").expect("send");
        let mut got = Vec::new();
        let mut chunk = [0u8; 3];
        while got.len() < 11 {
            let n = b.recv_raw(&mut chunk).expect("recv");
            assert!(n > 0, "no end of stream while the peer is open");
            got.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(got, b"hello world");
    }

    /// A raw deadline that consumed nothing is `Timeout` and leaves the stream where it was.
    #[test]
    fn mem_raw_deadline_keeps_the_stream_in_sync() {
        let (a, b) = MemTransport::pair();
        b.set_read_timeout(Some(std::time::Duration::from_millis(30)))
            .expect("set_read_timeout");
        let mut buf = [0u8; 4];
        assert!(matches!(b.recv_raw(&mut buf), Err(RpcError::Timeout)));
        a.send_raw(b"abcdef").expect("send");
        assert_eq!(b.recv_raw(&mut buf).expect("recv"), 4);
        assert!(matches!(b.recv_raw(&mut [0u8; 0]), Ok(0)));
        assert_eq!(b.recv_raw(&mut buf).expect("the rest"), 2);
        assert_eq!(&buf[..2], b"ef");
        assert!(matches!(b.recv_raw(&mut buf), Err(RpcError::Timeout)));
    }

    /// Bytes queued (and partly read) before a shutdown are delivered, then `Ok(0)` (module doc).
    #[test]
    fn mem_raw_shutdown_models_a_linux_socket() {
        let (a, b) = MemTransport::pair();
        b.send_raw(b"queued").expect("send");
        let mut buf = [0u8; 2];
        assert_eq!(a.recv_raw(&mut buf).expect("first part"), 2);
        a.shutdown().expect("shutdown");
        let mut rest = [0u8; 16];
        assert_eq!(a.recv_raw(&mut rest).expect("the unread rest"), 4);
        assert_eq!(&rest[..4], b"eued");
        assert_eq!(a.recv_raw(&mut rest).expect("end of stream"), 0);
        assert!(matches!(a.send_raw(b"x"), Err(RpcError::EndOfStream)));
        assert!(matches!(a.send_raw(b""), Err(RpcError::EndOfStream)));
        assert_eq!(b.recv_raw(&mut rest).expect("peer end of stream"), 0);
        assert!(matches!(b.send_raw(b"x"), Err(RpcError::EndOfStream)));
    }

    /// A frame read in the middle of a raw message is refused: no frame boundary is there.
    #[test]
    fn mem_frame_read_after_a_partial_raw_read_is_refused() {
        let (a, b) = MemTransport::pair();
        a.send_raw(b"abcd").expect("send");
        assert_eq!(b.recv_raw(&mut [0u8; 1]).expect("recv"), 1);
        assert!(matches!(b.recv_frame(), Err(RpcError::Protocol(_))));
    }
}
