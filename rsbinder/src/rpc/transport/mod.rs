// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Transport abstraction for the RPC stack (subplan 2-1).
//!
//! A [`RpcTransport`] carries **length-framed byte messages** for one
//! RPC connection and reports the [`PeerIdentity`] of the other end.
//! The implementation *defines the trust boundary*: a `unix` socket
//! trusts filesystem permissions + `SO_PEERCRED`; `vsock` (2-4) trusts
//! hypervisor VM isolation; `tls` (2-4) trusts a certificate; the gated
//! `tcp_debug` backend trusts **nothing** and is debug/interop only.
//!
//! Framing is the transport's responsibility (not the wire codec's), so
//! the 2-2 wire layer can think purely in whole messages. Stream
//! backends (`unix`, `tcp_debug`) share the length-prefix helpers in
//! this module; the in-process `mem` backend frames implicitly (one
//! channel message == one frame).
//!
//! The trait is **synchronous / blocking** (matches android-12 r34's
//! blocking-thread model). An `async` adapter can be layered *on top*
//! without changing this trait — see subplan 2-3 §7-2.

use std::fmt;
use std::io::{ErrorKind, Read, Write};

use super::{RpcError, RpcResult};

mod mem;
#[cfg(feature = "rpc-tcp-debug")]
mod tcp_debug;
mod unix;

pub use mem::MemTransport;
#[cfg(feature = "rpc-tcp-debug")]
pub use tcp_debug::{insecure_warning_emitted, TcpDebugTransport};
pub use unix::UnixTransport;

/// Hard cap on a single decoded frame.
///
/// A length header declaring more than this is rejected **before any
/// allocation** — an adversarial peer cannot trigger an OOM by claiming
/// a huge body (V4 / AC-1.8 / plan 2-1 §6.3). 64 MiB is far above any
/// legitimate binder transaction yet bounded.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

/// One RPC connection: framed byte transport + peer identity.
///
/// Synchronous and blocking. `&self` (not `&mut self`) so a session can
/// hold one transport and use it from a sender thread and a receiver
/// thread concurrently — full-duplex sockets and the `mem` channel pair
/// both support that without a deadlock (AC-1.4). Implementations must
/// keep `send_frame`/`recv_frame` independently callable from two
/// threads.
pub trait RpcTransport: Send + Sync {
    /// Send exactly one logical frame. The implementation guarantees
    /// framing (length prefix or channel message boundary).
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()>;

    /// Receive exactly one logical frame.
    ///
    /// A clean peer close with nothing pending is
    /// [`RpcError::PeerClosed`]; a header received but body short is
    /// [`RpcError::Truncated`]. Never panics or loops forever on a
    /// hostile peer.
    fn recv_frame(&self) -> RpcResult<Vec<u8>>;

    /// The other end's identity, as established by this transport.
    ///
    /// This is the RPC equivalent of kernel binder's
    /// `getCallingUid()`/SELinux context — **but only as strong as the
    /// transport's trust boundary**. [`PeerIdentity::Anonymous`] means
    /// no identity at all (no ACL possible); callers must log it as
    /// such and not grant trust.
    fn peer_identity(&self) -> PeerIdentity;

    /// Short human-readable description for diagnostics/logging
    /// (e.g. socket path, `"mem"`, vsock cid). Never carries secrets.
    fn describe(&self) -> &str;
}

/// Identity of the peer on the other end of a [`RpcTransport`].
///
/// `#[non_exhaustive]`: subplan 2-4 adds `Certificate(..)` (TLS) and
/// `Vsock { cid }` variants. Matching code must keep a wildcard arm.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerIdentity {
    /// A local peer whose credentials the kernel vouches for
    /// (`SO_PEERCRED` over a Unix domain socket, or the current
    /// process for the in-memory test transport).
    Local {
        /// Peer process effective UID.
        uid: u32,
        /// Peer process PID (`-1` if unavailable on this platform).
        pid: i32,
    },
    /// No identity is available. **ACL is not possible** against an
    /// anonymous peer; this must be surfaced in logs and never treated
    /// as trusted. Used by the debug-only plaintext TCP backend.
    Anonymous,
}

impl PeerIdentity {
    /// `true` for [`PeerIdentity::Local`].
    pub fn is_local(&self) -> bool {
        matches!(self, PeerIdentity::Local { .. })
    }

    /// Peer UID, if this identity carries one.
    pub fn uid(&self) -> Option<u32> {
        match self {
            PeerIdentity::Local { uid, .. } => Some(*uid),
            _ => None,
        }
    }

    /// Peer PID, if this identity carries a meaningful one
    /// (`Some(-1)` is filtered to `None`).
    pub fn pid(&self) -> Option<i32> {
        match self {
            PeerIdentity::Local { pid, .. } if *pid >= 0 => Some(*pid),
            _ => None,
        }
    }
}

impl fmt::Display for PeerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeerIdentity::Local { uid, pid } => write!(f, "local(uid={uid}, pid={pid})"),
            // Make the security-relevant "no identity" state loud.
            PeerIdentity::Anonymous => {
                write!(
                    f,
                    "anonymous(NO peer identity — access control NOT possible)"
                )
            }
        }
    }
}

// --- Length-prefix framing shared by stream backends -----------------
//
// Wire shape: `u32 little-endian length | <length> body bytes`. No
// magic / self-sync — the length is authoritative and bounded by
// MAX_FRAME_LEN before allocation. The `mem` backend does not use this
// (a channel message is already a frame).

/// Write one length-prefixed frame to a blocking stream.
pub(crate) fn write_frame<W: Write>(w: &mut W, buf: &[u8]) -> RpcResult<()> {
    if buf.len() > MAX_FRAME_LEN {
        return Err(RpcError::FrameTooLarge {
            declared: buf.len(),
            max: MAX_FRAME_LEN,
        });
    }
    let len = (buf.len() as u32).to_le_bytes();
    w.write_all(&len)?;
    w.write_all(buf)?;
    w.flush()?;
    Ok(())
}

/// Read exactly `buf.len()` bytes for a *frame header*. Zero bytes
/// before any progress is a clean [`RpcError::PeerClosed`]; a partial
/// header then EOF is [`RpcError::Truncated`].
fn read_header<R: Read>(r: &mut R, buf: &mut [u8]) -> RpcResult<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(if filled == 0 {
                    RpcError::PeerClosed
                } else {
                    RpcError::Truncated
                });
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Read exactly `buf.len()` body bytes. The header was already
/// committed, so *any* short read is [`RpcError::Truncated`].
fn read_body<R: Read>(r: &mut R, buf: &mut [u8]) -> RpcResult<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Err(RpcError::Truncated),
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Read one length-prefixed frame from a blocking stream.
pub(crate) fn read_frame<R: Read>(r: &mut R) -> RpcResult<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    read_header(r, &mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_LEN {
        // Reject *before* allocating `len` bytes.
        return Err(RpcError::FrameTooLarge {
            declared: len,
            max: MAX_FRAME_LEN,
        });
    }
    let mut body = vec![0u8; len];
    read_body(r, &mut body)?;
    Ok(body)
}

/// Decode-only entrypoint for the `rpc_frame_decode` fuzz target and
/// the deterministic adversarial-input regression tests (plan 2-1
/// §6.3). Feeds arbitrary bytes through the same deframing path
/// `recv_frame` uses. `#[doc(hidden)]`: not part of the supported API
/// surface (and absent entirely without the `rpc` feature).
#[doc(hidden)]
pub fn __fuzz_decode_frame(input: &[u8]) -> RpcResult<Vec<u8>> {
    read_frame(&mut std::io::Cursor::new(input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_over_cursor() {
        for size in [0usize, 1, 4, 64, 4096, 1 << 20] {
            let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut buf = Vec::new();
            write_frame(&mut buf, &payload).expect("write");
            let got = read_frame(&mut std::io::Cursor::new(&buf)).expect("read");
            assert_eq!(got, payload, "roundtrip mismatch at size {size}");
        }
    }

    #[test]
    fn two_frames_back_to_back_preserve_order() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"first").unwrap();
        write_frame(&mut buf, b"second").unwrap();
        let mut cur = std::io::Cursor::new(&buf);
        assert_eq!(read_frame(&mut cur).unwrap(), b"first");
        assert_eq!(read_frame(&mut cur).unwrap(), b"second");
    }

    /// T1.7 deterministic adversarial cases — must reject without
    /// allocating, panicking, or looping (mirrors the fuzz target).
    #[test]
    fn hostile_frame_headers_are_rejected_safely() {
        // Declared u32::MAX, no body: rejected pre-allocation.
        let huge = u32::MAX.to_le_bytes();
        assert!(matches!(
            __fuzz_decode_frame(&huge),
            Err(RpcError::FrameTooLarge { .. })
        ));

        // Declared MAX_FRAME_LEN + 1.
        let over = ((MAX_FRAME_LEN + 1) as u32).to_le_bytes();
        assert!(matches!(
            __fuzz_decode_frame(&over),
            Err(RpcError::FrameTooLarge { .. })
        ));

        // Empty input: clean peer-closed, no header at all.
        assert!(matches!(
            __fuzz_decode_frame(&[]),
            Err(RpcError::PeerClosed)
        ));

        // Partial header (2 of 4 bytes): truncated, not a panic.
        assert!(matches!(
            __fuzz_decode_frame(&[1, 0]),
            Err(RpcError::Truncated)
        ));

        // Header says 8 bytes, only 3 present: truncated body.
        let mut framed = 8u32.to_le_bytes().to_vec();
        framed.extend_from_slice(&[1, 2, 3]);
        assert!(matches!(
            __fuzz_decode_frame(&framed),
            Err(RpcError::Truncated)
        ));

        // A run of zero-length frames must not spin or panic.
        let zeros = vec![0u8; 4 * 1000];
        let mut cur = std::io::Cursor::new(&zeros[..]);
        for _ in 0..1000 {
            assert_eq!(read_frame(&mut cur).unwrap(), Vec::<u8>::new());
        }
    }

    #[test]
    fn write_frame_rejects_oversize_payload() {
        // We don't actually allocate MAX+1; just check the guard via a
        // fake writer that would error if written to.
        struct Trap;
        impl Write for Trap {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                panic!("oversize payload must be rejected before any write");
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // Build a slice header claiming oversize without allocating it:
        // use a zero-filled Vec of MAX+1 only conceptually — instead
        // assert the boundary with a borrowed empty slice and a forged
        // length check by calling the guard logic directly.
        let big = vec![0u8; MAX_FRAME_LEN + 1];
        assert!(matches!(
            write_frame(&mut Trap, &big),
            Err(RpcError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn peer_identity_display_and_accessors() {
        let local = PeerIdentity::Local { uid: 1000, pid: 42 };
        assert!(local.is_local());
        assert_eq!(local.uid(), Some(1000));
        assert_eq!(local.pid(), Some(42));
        assert_eq!(format!("{local}"), "local(uid=1000, pid=42)");

        let no_pid = PeerIdentity::Local { uid: 0, pid: -1 };
        assert_eq!(no_pid.pid(), None, "-1 pid is reported as unavailable");

        let anon = PeerIdentity::Anonymous;
        assert!(!anon.is_local());
        assert_eq!(anon.uid(), None);
        assert!(
            format!("{anon}").contains("NO peer identity"),
            "Anonymous Display must make the missing-identity state loud"
        );
    }
}
