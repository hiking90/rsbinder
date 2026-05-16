// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! vsock transport (subplan 2-4 track V) — **Linux only**.
//!
//! Trust boundary: hypervisor VM isolation (plan §5). Plaintext is
//! *correct* here, exactly as for `unix` on a single host. The peer
//! identity is [`PeerIdentity::Vsock`] carrying the context id — a
//! **routing address, not an ACL basis** (subplan 2-4 R1): the
//! hypervisor, not the cid value, is the trust boundary; the cid is
//! logged for diagnostics.
//!
//! Additive invariant (AC-4.1): this file + the feature + the
//! `PeerIdentity::Vsock` variant are the only change — the 2-2/2-3
//! core is untouched and runs unmodified with the transport swapped.
//!
//! Tests are Linux+VM and `#[ignore]` by default (need a peer VM or
//! `VMADDR_CID_LOCAL`), per the plan's V6 environment gate.

use std::sync::Mutex;

use vsock::{VsockAddr, VsockStream};

use super::{read_frame, write_frame, PeerIdentity, RpcTransport};
use crate::rpc::RpcResult;

/// A framed transport over a connected vsock stream (Linux).
pub struct VsockTransport {
    // One thread per connection in the RPC model — a `Mutex` keeps
    // `&self` without needing a duplex split.
    stream: Mutex<VsockStream>,
    peer: PeerIdentity,
    desc: String,
}

impl VsockTransport {
    /// Connect to `(cid, port)` (client side).
    pub fn connect(cid: u32, port: u32) -> RpcResult<Self> {
        let stream = VsockStream::connect(&VsockAddr::new(cid, port))?;
        Self::from_stream(stream)
    }

    /// Wrap an accepted/connected `VsockStream`. The peer cid is
    /// resolved once here.
    pub fn from_stream(stream: VsockStream) -> RpcResult<Self> {
        let peer = match stream.peer_addr() {
            Ok(a) => PeerIdentity::Vsock { cid: a.cid() },
            // No peer addr ⇒ no identity; never forge one.
            Err(_) => PeerIdentity::Anonymous,
        };
        let desc = match stream.peer_addr() {
            Ok(a) => format!("vsock:cid={},port={}", a.cid(), a.port()),
            Err(_) => "vsock".to_string(),
        };
        Ok(VsockTransport {
            stream: Mutex::new(stream),
            peer,
            desc,
        })
    }
}

impl RpcTransport for VsockTransport {
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
        let mut s = self.stream.lock().expect("vsock poisoned");
        write_frame(&mut *s, buf)
    }

    fn recv_frame(&self) -> RpcResult<Vec<u8>> {
        let mut s = self.stream.lock().expect("vsock poisoned");
        read_frame(&mut *s)
    }

    fn peer_identity(&self) -> PeerIdentity {
        self.peer.clone()
    }

    fn describe(&self) -> &str {
        &self.desc
    }

    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        self.stream
            .lock()
            .expect("vsock poisoned")
            .set_read_timeout(timeout)?;
        Ok(())
    }
}
