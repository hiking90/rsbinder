// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! In-process transport for hermetic tests.
//!
//! No sockets, no kernel: a pair of `mpsc` channels. One channel
//! message **is** one frame, so the length-prefix framing is bypassed
//! entirely. Two `MemTransport`s from [`MemTransport::pair`] are wired
//! cross-over so a write on one is a read on the other.
//!
//! `shutdown` models a **Linux** socket, deliberately: frames already
//! queued on either side are still delivered, then the end of stream;
//! later sends on either side fail. Linux is the deployment target, and
//! it is the platform where a caller that assumes a shutdown discards
//! what was queued is wrong — a hermetic backend that modelled the other
//! platform (macOS drops its queue) would certify that assumption on the
//! developer's machine and let it break on the device.
//!
//! There is no global state — every test makes its own independent
//! pair, so the RPC test suite is parallel-safe by construction.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use super::{PeerIdentity, RpcTransport};
use crate::rpc::{RpcError, RpcResult};

/// An in-process, in-memory framed transport endpoint.
///
/// `tx: Sender<…>` is not wrapped in a `Mutex`: `mpsc::Sender` is
/// `Sync + Clone` and `Sender::send` takes `&self`, so a `Mutex` would
/// only serialize unrelated senders without protecting anything.
/// `Receiver` stays under `Mutex` (it is `!Sync`).
pub struct MemTransport {
    tx: Sender<Vec<u8>>,
    rx: Mutex<Receiver<Vec<u8>>>,
    peer: PeerIdentity,
    desc: &'static str,
    timeout: Mutex<Option<std::time::Duration>>,
    /// Set by this end's [`shutdown`](RpcTransport::shutdown); shared with
    /// the peer as its `peer_closed`. Frames already queued are still
    /// delivered (the Linux model — see the module doc); once the queue is
    /// empty `recv_frame` reports `EndOfStream`, and sends on either side
    /// fail. A blocked `recv_frame` notices within one poll tick — a
    /// sender into our own `rx` would have been a cleaner wake-up, but it
    /// would also keep the channel alive past the peer's drop and hide
    /// `EndOfStream`.
    closed: Arc<AtomicBool>,
    /// The peer's `closed`: its shutdown is our end of stream and our
    /// `EPIPE`, as a socket peer's `shutdown(Both)` would be.
    peer_closed: Arc<AtomicBool>,
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
                rx: Mutex::new(b_rx),
                peer: peer.clone(),
                desc: "mem",
                timeout: Mutex::new(None),
                closed: Arc::clone(&a_closed),
                peer_closed: Arc::clone(&b_closed),
            },
            MemTransport {
                tx: b_tx,
                rx: Mutex::new(a_rx),
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
}

/// `PeerIdentity::Local` for the current process. Used by `mem` (and as
/// the non-Linux best-effort for `unix`, where `SO_PEERCRED` is
/// unavailable but a same-host/socketpair peer shares this identity).
pub(crate) fn self_identity() -> PeerIdentity {
    PeerIdentity::Local {
        uid: rustix::process::getuid().as_raw(),
        pid: std::process::id() as i32,
    }
}

impl RpcTransport for MemTransport {
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
        // Same cap the stream backends enforce in `write_frame`, so the
        // hermetic test transport cannot pass a frame every real one
        // rejects.
        if buf.len() > super::MAX_FRAME_LEN {
            return Err(RpcError::FrameTooLarge {
                declared: buf.len(),
                max: super::MAX_FRAME_LEN,
            });
        }
        // A shutdown on either end makes a later send fail, as the socket
        // backends' `shutdown(Both)` does on both sides (EPIPE) — callers
        // rely on that to retire a slot whose handshake failed. Without it
        // the frame would queue on an unbounded channel nobody reads.
        if self.either_end_shut() {
            return Err(RpcError::EndOfStream);
        }
        // A channel send only fails once the peer's receiver is
        // dropped — i.e. the peer is gone. Lock-free (`Sender: Sync`).
        self.tx
            .send(buf.to_vec())
            .map_err(|_| RpcError::EndOfStream)
    }

    fn recv_frame(&self) -> RpcResult<Vec<u8>> {
        let timeout = *self.timeout.lock().expect("mem timeout poisoned");
        let rx = self.rx.lock().expect("mem rx poisoned");
        // Block in short `recv_timeout` ticks so a local `shutdown` is
        // noticed; a frame or the peer's drop (every sender gone) returns
        // at once, never spinning.
        const TICK: std::time::Duration = std::time::Duration::from_millis(20);
        // `checked_add`, not `+`: `Instant + Duration` panics on overflow,
        // and `timeout` is caller-supplied. A duration that cannot be added
        // to `now` is effectively infinite, which is what `None` already
        // means here.
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
                // Queued before a shutdown on either end: still delivered,
                // as a Linux socket delivers what it has queued.
                Ok(frame) => return Ok(frame),
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

    /// `RpcTransport::shutdown` on a Linux socket: what either side had
    /// queued is still delivered, then the end of stream; later sends on
    /// either side fail. `mem` models exactly that (module doc), so a
    /// teardown bug that a real transport would surface on the device is
    /// visible to every hermetic test.
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
        // The peer: drains what we sent, then sees our shutdown as its
        // end of stream, and its sends fail (EPIPE on a socket).
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
    }

    /// Bidirectional simultaneous traffic must not deadlock or
    /// lose/reorder frames. Two threads cross-fire 10k frames each.
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

    /// A read timeout too large to add to `Instant::now()` must read as
    /// "no deadline", not panic — `set_read_timeout` is public API.
    #[test]
    fn unaddable_read_timeout_does_not_panic() {
        let (a, b) = MemTransport::pair();
        a.set_read_timeout(Some(std::time::Duration::MAX))
            .expect("set_read_timeout");
        b.send_frame(b"hi").expect("send");
        assert_eq!(a.recv_frame().expect("recv"), b"hi");
    }
}
