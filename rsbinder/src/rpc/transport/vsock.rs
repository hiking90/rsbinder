// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! vsock transport (subplan 2-4 track V) — **Linux / Android** (the
//! `vsock` crate's `AF_VSOCK` impl covers both `target_os = "linux"` and
//! `"android"`; Android is the Virtualization Framework / Microdroid pVM
//! host↔guest target — subplan 2-15).
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

use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

use vsock::{VsockAddr, VsockStream};

use super::{read_frame, write_frame, PeerIdentity, RpcTransport};
use crate::rpc::RpcResult;

/// A framed transport over a connected vsock stream (Linux).
///
/// **M11 fix (review 2026-05-21)**: lock-free, full-duplex. `vsock`
/// 0.5+ implements `impl Read for &VsockStream` and `impl Write for
/// &VsockStream`, mirroring `UnixStream`, so a shared `&self` can
/// `send_frame` while another thread `recv_frame`s without serializing
/// through a `Mutex`. The trait doc on [`RpcTransport`] requires
/// concurrent send+recv, which the previous `Mutex<VsockStream>`
/// violated.
pub struct VsockTransport {
    stream: VsockStream,
    peer: PeerIdentity,
    desc: String,
}

impl VsockTransport {
    /// Connect to `(cid, port)` (client side).
    pub fn connect(cid: u32, port: u32) -> RpcResult<Self> {
        let stream = VsockStream::connect(&VsockAddr::new(cid, port))?;
        Self::from_stream(stream)
    }

    /// Wrap a preconnected vsock `OwnedFd` (subplan 2-13 A0.2 — the
    /// `IAccessor::addConnection()` fd-adopt path, `AF_VSOCK` family).
    /// The `vsock` crate has no public `From<OwnedFd>` for `VsockStream`
    /// so this goes through `FromRawFd` after taking ownership; the
    /// caller is responsible for asserting the fd's address family.
    pub fn from_owned_fd(fd: OwnedFd) -> RpcResult<Self> {
        // SAFETY: `fd` is moved (consumed) here; `VsockStream::from_raw_fd`
        // adopts exclusive ownership for the lifetime of the stream. The
        // `OwnedFd` is converted via `into_raw_fd()` which is the AOSP-
        // sanctioned "transfer ownership without closing" path.
        let raw = fd.into_raw_fd();
        let stream = unsafe { VsockStream::from_raw_fd(raw) };
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
        Ok(VsockTransport { stream, peer, desc })
    }
}

impl RpcTransport for VsockTransport {
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
        // `&VsockStream: Write` (vsock 0.5+), so a shared `&self` can
        // send while another thread receives — same lock-free duplex
        // pattern as `UnixTransport`.
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
