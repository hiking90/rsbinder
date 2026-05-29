// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! In-process transport for hermetic tests.
//!
//! No sockets, no kernel: a pair of `mpsc` channels. One channel
//! message **is** one frame, so the length-prefix framing is bypassed
//! entirely. Two `MemTransport`s from [`MemTransport::pair`] are wired
//! cross-over so a write on one is a read on the other.
//!
//! There is no global state — every test makes its own independent
//! pair, so the RPC test suite is parallel-safe by construction.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;

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
}

impl MemTransport {
    /// Create a connected pair. Anything sent on `.0` is received on
    /// `.1` and vice-versa. Peer identity is this process (the only
    /// possible peer for an in-process channel).
    pub fn pair() -> (Self, Self) {
        let (a_tx, a_rx) = std::sync::mpsc::channel();
        let (b_tx, b_rx) = std::sync::mpsc::channel();
        let peer = self_identity();
        (
            MemTransport {
                tx: a_tx,
                rx: Mutex::new(b_rx),
                peer: peer.clone(),
                desc: "mem",
                timeout: Mutex::new(None),
            },
            MemTransport {
                tx: b_tx,
                rx: Mutex::new(a_rx),
                peer,
                desc: "mem",
                timeout: Mutex::new(None),
            },
        )
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
        // A channel send only fails once the peer's receiver is
        // dropped — i.e. the peer is gone. Lock-free (`Sender: Sync`).
        self.tx.send(buf.to_vec()).map_err(|_| RpcError::PeerClosed)
    }

    fn recv_frame(&self) -> RpcResult<Vec<u8>> {
        let timeout = *self.timeout.lock().expect("mem timeout poisoned");
        let rx = self.rx.lock().expect("mem rx poisoned");
        match timeout {
            // `recv`/`recv_timeout` block until a frame arrives, the
            // deadline elapses, or every sender drops (peer closed) —
            // never spin, never panic.
            None => rx.recv().map_err(|_| RpcError::PeerClosed),
            Some(d) => rx.recv_timeout(d).map_err(|e| match e {
                std::sync::mpsc::RecvTimeoutError::Timeout => RpcError::Timeout,
                std::sync::mpsc::RecvTimeoutError::Disconnected => RpcError::PeerClosed,
            }),
        }
    }

    fn peer_identity(&self) -> PeerIdentity {
        self.peer.clone()
    }

    fn describe(&self) -> &str {
        self.desc
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
        assert!(matches!(a.recv_frame(), Err(RpcError::PeerClosed)));
        assert!(matches!(a.send_frame(b"x"), Err(RpcError::PeerClosed)));
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
}
