// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! RPC transport (binder-over-socket) — a **separate stack** from the
//! kernel binder path.
//!
//! This module is the rsbinder equivalent of Android's `Rpc*` code
//! (`RpcServer`/`RpcSession`/`RpcState`). It shares only the high-level
//! data model (`IBinder`/`Parcel`/AIDL stubs) with the kernel path; it
//! never touches `ProcessState`, `ThreadState`, `/dev/binder`, ioctl or
//! mmap. See `plan/2-rpc-transport.md` for the architecture and the
//! per-workstream subplans `plan/2-1`…`plan/2-7`.
//!
//! # Security
//!
//! **RPC is _not_ a drop-in for kernel binder's security model.** The
//! kernel gives `getCallingUid()`/SELinux for free; RPC does not. Each
//! [`RpcTransport`](crate::rpc::transport::RpcTransport)
//! implementation *defines its own trust boundary* and reports a
//! [`PeerIdentity`](crate::rpc::transport::PeerIdentity). A transport
//! that returns
//! [`PeerIdentity::Anonymous`](crate::rpc::transport::PeerIdentity::Anonymous)
//! gives the RPC layer **no basis for access control** — this is
//! logged explicitly and must be treated as untrusted. Plaintext
//! network transport is never appropriate for production (use the
//! `tls` backend, added by subplan 2-4).
//!
//! # Example (Unix-domain server + client)
//!
//! ```no_run
//! # #[cfg(feature = "rpc")] {
//! use rsbinder::rpc::{RpcServer, RpcSession};
//!
//! // Server: bind, publish a root binder, accept in the background.
//! let server = RpcServer::setup_unix_server("/tmp/demo.sock").unwrap();
//! # let root: rsbinder::SIBinder = unimplemented!();
//! server.set_root(root);
//! let _bg = server.run_background();
//!
//! // Client: connect, (optionally) negotiate, fetch the root object.
//! let client = RpcSession::setup_unix_client("/tmp/demo.sock").unwrap();
//! let _negotiated = client.negotiate(4).unwrap();
//! let _root = client.get_root().unwrap();
//! // Drive `_root` with a typed stub (subplan 2-6 makes the AIDL
//! // generator emit RPC-capable stubs; until then, hand-written).
//! # }
//! ```
//!
//! A full client/server pair (incl. nested callbacks, oneway, timeout)
//! is exercised by `rsbinder/tests/rpc_server.rs`.

pub mod address;
pub mod fd_mode;
pub mod proxy;
pub mod server;
pub mod session;
pub mod state;
pub mod transport;
pub mod wire;

pub use address::{AddressSpace, RpcAddress, SpecialTransaction, RPC_SESSION_ID_NEW};
pub use fd_mode::FileDescriptorTransportMode;
pub use proxy::RpcProxy;
pub use server::RpcServer;
pub use session::RpcSession;
pub use state::RpcState;
pub use transport::{CertId, PeerIdentity, RpcTransport};

/// Re-export of the exact `rustls` the `tls` backend links, so callers
/// build `ClientConfig`/`ServerConfig` against a matching version
/// (subplan 2-4 track T — key/cert management stays caller-side).
#[cfg(feature = "rpc-tls")]
pub use rustls;
pub use wire::{R34Codec, WireCodec, WireMessage, WireReply, WireTransaction};

use std::fmt;

/// Result type for the RPC transport / protocol layer.
///
/// The transport layer surfaces a *rich* [`RpcError`] (so callers can
/// distinguish a clean peer close from a truncated frame, etc.). Public
/// RPC APIs (added by later subplans) project this onto
/// `rsbinder::Result` (`StatusCode`) or AIDL-facing `Status` at the
/// boundary — see [`StatusCode::RpcError`](crate::StatusCode::RpcError)
/// and `From<RpcError> for StatusCode`.
pub type RpcResult<T> = std::result::Result<T, RpcError>;

/// A transport-/protocol-level RPC error.
///
/// Kept separate from [`StatusCode`](crate::StatusCode) because
/// `StatusCode` is `Copy`/`Ord`/`Hash` and cannot carry a rich payload.
/// `#[non_exhaustive]` so later subplans (wire decode in 2-2, session
/// handshake in 2-3, TLS in 2-4) can add variants without a breaking
/// change.
#[non_exhaustive]
#[derive(Debug)]
pub enum RpcError {
    /// The peer closed the connection cleanly with no frame pending
    /// (EOF / `BrokenPipe` / `ConnectionReset` before any header bytes).
    PeerClosed,
    /// A frame length header was fully received but the body was
    /// truncated (peer closed mid-body, or declared more than it sent).
    Truncated,
    /// A declared frame length exceeds [`transport::MAX_FRAME_LEN`].
    /// Rejected *before* any allocation (anti-OOM, V4).
    FrameTooLarge {
        /// The length the peer (or caller) declared.
        declared: usize,
        /// The configured maximum ([`transport::MAX_FRAME_LEN`]).
        max: usize,
    },
    /// An underlying transport I/O error that is not a clean close.
    Io(std::io::Error),
    /// A protocol-level violation (used by the wire codec in 2-2+).
    Protocol(&'static str),
    /// A configured wait deadline elapsed with no frame boundary
    /// reached (subplan 2-3 — reply / negotiation timeout). Reported
    /// only when nothing partial was consumed, so the stream stays
    /// frame-synchronized.
    Timeout,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::PeerClosed => write!(f, "RPC peer closed the connection"),
            RpcError::Truncated => write!(f, "RPC frame truncated (incomplete body)"),
            RpcError::FrameTooLarge { declared, max } => {
                write!(
                    f,
                    "RPC frame too large: declared {declared} bytes, max {max}"
                )
            }
            RpcError::Io(e) => write!(f, "RPC transport I/O error: {e}"),
            RpcError::Protocol(why) => write!(f, "RPC protocol violation: {why}"),
            RpcError::Timeout => write!(f, "RPC wait deadline elapsed"),
        }
    }
}

impl std::error::Error for RpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RpcError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for RpcError {
    /// Map a clean disconnect to [`RpcError::PeerClosed`]; everything
    /// else stays [`RpcError::Io`]. A truncated *body* is classified by
    /// the framing reader, not here.
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        match e.kind() {
            UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted => {
                RpcError::PeerClosed
            }
            _ => RpcError::Io(e),
        }
    }
}

impl From<RpcError> for crate::StatusCode {
    /// Boundary projection used when an RPC failure must surface through
    /// `rsbinder::Result`. Specific, actionable mappings where they
    /// help a caller; the catch-all [`StatusCode::RpcError`] otherwise.
    ///
    /// [`StatusCode::RpcError`]: crate::StatusCode::RpcError
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::PeerClosed => crate::StatusCode::DeadObject,
            RpcError::Truncated => crate::StatusCode::NotEnoughData,
            RpcError::FrameTooLarge { .. } => crate::StatusCode::BadValue,
            RpcError::Io(io) => crate::StatusCode::from(io),
            RpcError::Protocol(_) => crate::StatusCode::RpcError,
            RpcError::Timeout => crate::StatusCode::TimedOut,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatusCode;

    #[test]
    fn io_clean_close_maps_to_peer_closed() {
        for kind in [
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
        ] {
            let e: RpcError = std::io::Error::from(kind).into();
            assert!(matches!(e, RpcError::PeerClosed), "{kind:?} -> {e:?}");
        }
        // A non-disconnect I/O error stays Io(..).
        let other: RpcError = std::io::Error::from(std::io::ErrorKind::PermissionDenied).into();
        assert!(matches!(other, RpcError::Io(_)));
    }

    #[test]
    fn rpc_error_projects_onto_status_code() {
        assert_eq!(
            StatusCode::from(RpcError::PeerClosed),
            StatusCode::DeadObject
        );
        assert_eq!(
            StatusCode::from(RpcError::Truncated),
            StatusCode::NotEnoughData
        );
        assert_eq!(
            StatusCode::from(RpcError::FrameTooLarge {
                declared: 1 << 30,
                max: 1
            }),
            StatusCode::BadValue
        );
        assert_eq!(
            StatusCode::from(RpcError::Protocol("bad")),
            StatusCode::RpcError
        );
    }

    /// The cfg-gated `StatusCode::RpcError` must round-trip through the
    /// three hand-written exhaustive matches (Display / i32 both ways)
    /// without colliding with another code.
    #[test]
    fn status_code_rpc_error_roundtrips() {
        let v: i32 = StatusCode::RpcError.into();
        assert_eq!(StatusCode::from(v), StatusCode::RpcError);
        assert_eq!(format!("{}", StatusCode::RpcError), "RpcError");
        // Distinct from its neighbours in the UNKNOWN_ERROR + n block.
        assert_ne!(v, StatusCode::UnexpectedNull.into());
        assert_ne!(v, StatusCode::FailedTransaction.into());
    }
}
