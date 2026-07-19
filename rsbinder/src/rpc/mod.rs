// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! RPC transport (binder-over-socket) — a **separate stack** from the
//! kernel binder path.
//!
//! This module is the rsbinder equivalent of Android's `Rpc*` code
//! (`RpcServer`/`RpcSession`/`RpcState`). It shares only the high-level
//! data model (`IBinder`/`Parcel`/AIDL stubs) with the kernel path; it
//! never touches `ProcessState`, `ThreadState`, `/dev/binder`, ioctl or
//! mmap.
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
//! `tls` backend).
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
//! let root = client.get_root().unwrap();
//! // Drive `root` with the **same generated stub** as the kernel path
//! // — the AIDL generator emits `as_remote().ok_or(BadType)?`, so one
//! // `Bp*` resolves either stack:
//! //
//! //     let foo: Strong<dyn IFoo> =
//! //         <dyn IFoo as FromIBinder>::try_from(root)?;
//! # let _ = root;
//! # }
//! ```
//!
//! A complete, runnable Unix-domain client/server pair driving a
//! generated AIDL stub is in
//! [`example-hello`](https://github.com/hiking90/rsbinder/tree/master/example-hello)
//! (`cargo run -p example-hello --features rpc --bin rpc_hello_service`
//! / `--bin rpc_hello_client`). A full pair incl. nested callbacks,
//! oneway, timeout and shared-session concurrency is exercised by
//! `rsbinder/tests/rpc_server.rs`.
//!
//! # Async
//!
//! The RPC stack's I/O is **blocking** (thread-per-connection, matching
//! android-12 r34's blocking-thread model). There is deliberately *no*
//! non-blocking `RpcTransport` / async reactor serve loop.
//!
//! What *is* supported (and verified — `tests/rpc_async.rs`) is the
//! same `spawn_blocking` adapter the kernel async path uses, over RPC:
//!
//! * **Async client** — the generated `…Async<P>` stub
//!   (`Strong::into_async::<rsbinder::Tokio>()`) runs each blocking
//!   `client_transact` on `tokio::task::spawn_blocking`; the reply
//!   parse is the async continuation. Concurrent calls on one shared
//!   session stay correctly serialized by the per-connection driver
//!   lock, now under genuine async concurrency.
//! * **Async service** — `Bn*::new_async_binder(impl …AsyncService,
//!   TokioRuntime(handle))` drives an `async fn` handler via
//!   `rt.block_on` from the blocking serve worker.
//!
//! Note this needs no kernel binder: the `Tokio` pool's
//! "am-I-in-a-kernel-transaction?" guard short-circuits via
//! `ProcessState::is_initialized()` so a pure-RPC process (e.g. on
//! macOS) no longer panics on an uninitialized `ProcessState`.

pub mod address;
pub mod fd_mode;
pub(crate) mod lifecycle;
pub mod proxy;
pub mod server;
pub mod session;
// Internal RPC machinery: the wire-codec layer and per-session refcount/async
// state. Not part of the public API — the codec is selected internally (no user
// injection point) and `RpcState` is private session bookkeeping. Keeping them
// `pub(crate)` lets the protocol evolve without semver-breaking releases.
pub(crate) mod state;
pub mod transport;
// The wire modules implement the complete AOSP codec surface (both directions
// of every message, all negotiated versions), fully exercised by their own
// hermetic `mod tests`. Some encode/decode entry points are validated there but
// not reached by the current live dispatch path; `dead_code` fired on them only
// after the demotion from `pub`. Keep the complete, tested surface.
#[allow(dead_code)]
pub(crate) mod wire;
#[allow(dead_code)]
pub(crate) mod wire_android13;

pub use address::{AddressSpace, RpcAddress, SpecialTransaction, RPC_SESSION_ID_NEW};
pub use fd_mode::FileDescriptorTransportMode;
pub use proxy::RpcProxy;
pub use server::RpcServer;
pub use session::{RpcSession, RpcUnixClientConfig};
pub use transport::{CertId, PeerIdentity, RpcTransport};

/// Re-export of the exact `rustls` the `tls` backend links, so callers
/// build `ClientConfig`/`ServerConfig` against a matching version
/// (key/cert management stays caller-side).
#[cfg(feature = "rpc-tls")]
pub use rustls;

use std::fmt;

/// Result type for the RPC transport / protocol layer.
///
/// The transport layer surfaces a *rich* [`RpcError`] (so callers can
/// distinguish a clean peer close from a truncated frame, etc.). Public
/// RPC APIs project this onto
/// `rsbinder::Result` (`StatusCode`) or AIDL-facing `Status` at the
/// boundary — see [`StatusCode::RpcError`](crate::StatusCode::RpcError)
/// and `From<RpcError> for StatusCode`.
pub type RpcResult<T> = std::result::Result<T, RpcError>;

/// A transport-/protocol-level RPC error.
///
/// Kept separate from [`StatusCode`](crate::StatusCode) because
/// `StatusCode` is `Copy`/`Ord`/`Hash` and cannot carry a rich payload.
/// `#[non_exhaustive]` so the wire-decode, session-handshake, and TLS
/// layers can add variants without a breaking change.
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
    /// Rejected *before* any allocation (anti-OOM).
    FrameTooLarge {
        /// The length the peer (or caller) declared.
        declared: usize,
        /// The configured maximum ([`transport::MAX_FRAME_LEN`]).
        max: usize,
    },
    /// An underlying transport I/O error that is not a clean close.
    Io(std::io::Error),
    /// A protocol-level violation (used by the wire codec).
    Protocol(&'static str),
    /// A configured wait deadline elapsed with no frame boundary
    /// reached (reply / negotiation timeout). Reported
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

impl From<RpcError> for std::io::Error {
    /// Boundary projection back to `std::io::Error` for callers that
    /// hold an `io::Result<_>` accumulator (the accept-loop dispatch in
    /// `RpcServer::run` is the motivating site — its `accept_transport`
    /// helper must surface a `from_stream`-side `RpcError` through the
    /// same `io::ErrorKind` matching as the underlying `accept()`).
    /// `RpcError::Io` round-trips the original `io::Error` unchanged;
    /// other variants are wrapped with [`std::io::ErrorKind::Other`].
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::Io(io) => io,
            other => std::io::Error::other(format!("{other}")),
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

/// Decode-only entrypoint for the `rpc_parcel_rpc_mode` fuzz target.
/// Arbitrary bytes are interpreted as an **RPC-mode**
/// `Parcel` body and run through the deserializers a real RPC
/// transaction reaches: scalars, `String`, the generic `Vec<T>` array
/// path, binder-as-`RpcAddress`, and the AIDL out-vec resizers.
/// Property: no panic / OOM / UB / unbounded pre-allocation on *any*
/// input — every length is bounded by the bytes actually present.
/// Not part of the supported API surface.
#[doc(hidden)]
pub fn __fuzz_decode_rpc_parcel(input: &[u8]) {
    use crate::binder::SIBinder;
    use crate::error::{Result, StatusCode};
    use crate::parcel::{Parcel, RpcParcelOps};
    use std::sync::Arc;

    // Binder hook with no live session: exercises the RPC `read_binder`
    // path (i32 present flag + 32-byte `RpcAddress`, all bounds-checked)
    // without needing a real connection.
    struct NullOps;
    impl RpcParcelOps for NullOps {
        fn write_binder(&self, _b: Option<&SIBinder>, _p: &mut Parcel) -> Result<()> {
            Err(StatusCode::DeadObject)
        }
        fn read_binder(&self, _p: &mut Parcel) -> Result<Option<SIBinder>> {
            Err(StatusCode::DeadObject)
        }
    }

    fn fresh(input: &[u8]) -> Parcel {
        let mut p = Parcel::from_vec(input.to_vec());
        p.set_for_rpc(true);
        p.attach_rpc_ops(Arc::new(NullOps));
        p.set_data_position(0);
        p
    }

    let _ = fresh(input).read::<i32>();
    let _ = fresh(input).read::<i64>();
    let _ = fresh(input).read::<String>();
    let _ = fresh(input).read::<Vec<i32>>();
    let _ = fresh(input).read::<Vec<i64>>();
    let _ = fresh(input).read::<Vec<String>>();
    let _ = fresh(input).read::<Option<SIBinder>>();
    let _ = fresh(input).read::<SIBinder>();
    // NOTE: `resize_out_vec`/`resize_nullable_out_vec` are intentionally
    // *not* fuzzed here — an out-vec length is not backed by parcel
    // bytes (the callee fills it), so they are unbounded by design
    // (upstream Android is identical); feeding an arbitrary length
    // would just OOM the fuzzer without modelling a real wire input.
    // The bounded `in`-array path (`deserialize_array`) above is the
    // surface this target covers.
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

    /// A hostile array length in an RPC-mode parcel must fail
    /// gracefully (bounded pre-allocation + `Err`), never pre-allocate
    /// gigabytes. Deterministic regression — reverting the
    /// `min(len, data_avail())` / `len > data_avail()` guards turns each
    /// of these into a multi-GB allocation that aborts the test
    /// process. The `rpc_parcel_rpc_mode` fuzz target is the soak
    /// supplement.
    #[test]
    fn rpc_parcel_hostile_array_len_is_bounded_not_oom() {
        use crate::parcel::Parcel;

        // Generic `Vec<T>` array path, body empty after the length.
        let mut p = Parcel::new();
        p.set_for_rpc(true);
        p.write(&i32::MAX).unwrap();
        p.set_data_position(0);
        assert!(
            p.read::<Vec<i32>>().is_err(),
            "hostile Vec<i32> len must error, not OOM"
        );

        // A little data present (data_avail() > 0 but << len).
        let mut p = Parcel::new();
        p.set_for_rpc(true);
        p.write(&i32::MAX).unwrap();
        p.write(&7i32).unwrap();
        p.write(&8i32).unwrap();
        p.set_data_position(0);
        assert!(p.read::<Vec<i64>>().is_err());

        // (`resize_out_vec`/`resize_nullable_out_vec` are deliberately
        // not asserted here: an out-vec length is not backed by parcel
        // data — the callee fills it — so they are unbounded by design,
        // exactly like Android libbinder's `resizeOutVector`. Bounding
        // them by `data_avail()` regressed the live kernel out-array
        // path; see the note in `parcel.rs`.)

        // The fuzz entrypoint must never panic on adversarial bytes.
        for pat in [
            vec![],
            vec![0xFFu8; 4],
            vec![0xFF, 0xFF, 0xFF, 0x7F, 0, 0, 0, 0],
            (0..64u8).collect::<Vec<u8>>(),
        ] {
            __fuzz_decode_rpc_parcel(&pat);
        }
    }
}
