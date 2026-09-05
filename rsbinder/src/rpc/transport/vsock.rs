// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! vsock transport — **Linux / Android** (the `vsock` crate's
//! `AF_VSOCK` impl covers both `target_os = "linux"` and `"android"`;
//! Android is the Virtualization Framework / Microdroid pVM host↔guest
//! target).
//!
//! Trust boundary: hypervisor VM isolation. Plaintext is *correct*
//! here, exactly as for `unix` on a single host. The peer identity is
//! [`PeerIdentity::Vsock`] carrying the context id — a **routing
//! address, not an ACL basis**: the hypervisor, not the cid value, is
//! the trust boundary; the cid is logged for diagnostics.
//!
//! Additive: this file + the feature + the `PeerIdentity::Vsock`
//! variant are the only change — the core is untouched and runs
//! unmodified with the transport swapped.
//!
//! Tests are Linux+VM and `#[ignore]` by default (need a peer VM or
//! `VMADDR_CID_LOCAL`).

use std::os::fd::OwnedFd;

use vsock::{VsockAddr, VsockStream};

use super::{read_frame, write_frame, PeerIdentity, RpcTransport};
use crate::rpc::RpcResult;

/// A framed transport over a connected vsock stream (Linux).
///
/// Lock-free, full-duplex. `vsock` 0.5+ implements `impl Read for
/// &VsockStream` and `impl Write for &VsockStream`, mirroring
/// `UnixStream`, so a shared `&self` can `send_frame` while another
/// thread `recv_frame`s without serializing through a `Mutex` — as the
/// [`RpcTransport`] trait contract requires.
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

    /// Wrap a preconnected vsock `OwnedFd` (the
    /// `IAccessor::addConnection()` fd-adopt path, `AF_VSOCK` family).
    /// `vsock` provides `From<OwnedFd> for VsockStream` (safe ownership
    /// transfer, mirroring std's `UnixStream::from(OwnedFd)`); the caller
    /// is responsible for asserting the fd's address family.
    pub fn from_owned_fd(fd: OwnedFd) -> RpcResult<Self> {
        Self::from_stream(VsockStream::from(fd))
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

    /// Raw, unframed write for the android-13+ profile (the real android
    /// RPC wire has no length prefix). Mirrors `UnixTransport::send_raw`;
    /// without it a `vsock://…?profile=android13plus` connection failed at
    /// its first handshake byte — on the Microdroid/AVF target this module
    /// exists for, where the peer is real libbinder and speaks only this.
    fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
        use std::io::Write;
        let mut w = &self.stream;
        w.write_all(buf)?;
        w.flush()?;
        Ok(())
    }

    /// Raw, unframed read (one `read`; `Ok(0)` = peer closed). Mirrors
    /// `UnixTransport::recv_raw`, including the `Interrupted` retry and the
    /// deadline → `Timeout` mapping.
    fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
        use std::io::Read;
        let mut r = &self.stream;
        loop {
            return match r.read(buf) {
                Ok(n) => Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if super::is_timeout(&e) => Err(crate::rpc::RpcError::Timeout),
                Err(e) => Err(e.into()),
            };
        }
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
        super::absorb_already_shut(self.stream.shutdown(std::net::Shutdown::Both))
    }
}
