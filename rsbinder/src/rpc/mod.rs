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
//! # Endianness
//!
//! The wire is little-endian, header and body alike, on whatever host
//! either end runs — so an rsbinder peer and an AOSP libbinder peer
//! exchange byte-identical parcels, and a big-endian host is a peer like
//! any other rather than a silent corruption. See the crate docs'
//! *Wire byte order* for the three-layer split.
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
//! server.set_root(root).unwrap();
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
//! `ProcessState::is_initialized()`, so a pure-RPC process (e.g. on
//! macOS) does not panic on an uninitialized `ProcessState`.
//!
//! # Frame-boundary reads
//!
//! A read that failed with [`RpcError::EndOfStream`](crate::rpc::RpcError::EndOfStream) ("no
//! frame pending") or [`RpcError::Timeout`](crate::rpc::RpcError::Timeout) ("no frame boundary
//! crossed") is documented to have left the stream at
//! a frame boundary. Every other read failure lacks that guarantee — including ones that in
//! fact consumed nothing — and is treated as a lost position. One caller asks through the
//! crate-private `RpcError::leaves_frame_boundary_intact`: the android-13+ reader, which
//! promotes a mid-frame case to `Truncated` / `DeadlineMidFrame`. The transports'
//! length-prefix readers and the serve loop reimplement the same split inline — against the io
//! error kind and the `RpcError` variants respectively — so a change to the set has to be
//! made in all three places.

pub mod address;
pub(crate) mod deadline;
pub mod end;
pub mod fd_mode;
pub(crate) mod lifecycle;
pub mod proxy;
pub mod server;
pub mod session;
// Codec and session bookkeeping are `pub(crate)` so the protocol can change without semver breaks.
pub(crate) mod state;
pub mod transport;
pub(crate) mod wire;
pub(crate) mod wire_android13;
/// Fuzz entrypoints (`fuzz/fuzz_targets/rpc_{wire,address}_decode.rs`,
/// `rpc_session_handshake.rs`); not part of the supported API.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub use wire::{__fuzz_decode_address, __fuzz_decode_wire, __fuzz_session_handshake};

pub use address::{AddressSpace, RpcAddress, SpecialTransaction, RPC_SESSION_ID_NEW};
pub use end::{EndReason, EndedBy, SessionEnd, StreamState};
pub use fd_mode::FileDescriptorTransportMode;
pub use proxy::RpcProxy;
pub use server::RpcServer;
#[expect(
    deprecated,
    reason = "re-exports the deprecated config while it is honored"
)]
pub use session::RpcUnixClientConfig;
pub use session::{RpcClientConfig, RpcSession};
pub use transport::{CertId, PeerIdentity, RpcTransport};

/// Re-export of the exact `rustls` the `tls` backend links, so callers
/// build `ClientConfig`/`ServerConfig` against a matching version
/// (key/cert management stays caller-side).
#[cfg(feature = "rpc-tls")]
pub use rustls;

use std::fmt;

/// Refuses a proxy at registration, not late at write (AOSP `RpcState::onBinderLeaving`).
pub(crate) fn refuse_remote(binder: &crate::SIBinder, what: &str) -> crate::Result<()> {
    if (**binder).is_remote() {
        log::error!("{what}: refusing a remote binder; wrap it in a local Bn* (gateway) instead");
        return Err(crate::StatusCode::InvalidOperation);
    }
    Ok(())
}

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
    /// The stream ended: EOF with no frame pending, or a disconnect kind
    /// (`BrokenPipe` / `ConnectionReset` / `ConnectionAborted`) from a
    /// read or a write. It says nothing about
    /// **who** ended the stream or whether the end was **clean**: this
    /// end's own write failing after its own
    /// [`shutdown`](transport::RpcTransport::shutdown) folds here just as
    /// a peer's EOF does, and so does a reset. Whether this end decided
    /// is [`SessionEnd::by`]; whether the transport's close signal
    /// arrived is [`UncleanEndOfStream`](Self::UncleanEndOfStream). The
    /// frame-boundary guarantee is **read-side only**: a read that
    /// disconnects part-way through a frame is
    /// [`Truncated`](Self::Truncated), never this. A *send* that
    /// disconnects past its first byte is this variant with the position
    /// lost — no writer can report how much of the frame went out — which
    /// is one reason the session's send-failure rule ends the session on
    /// it.
    /// Projects to [`StatusCode::DeadObject`](crate::StatusCode).
    EndOfStream,
    /// The stream ended without the transport's own close signal — a TLS
    /// session whose TCP stream ended with no `close_notify`. It arrives at
    /// a frame boundary or part-way through one, and the position is not
    /// assumed intact either way (the length-prefix readers behind
    /// `send_frame` / `recv_frame` promote it to
    /// [`Truncated`](Self::Truncated) mid-frame; the AOSP-framing ones every
    /// session uses leave it as itself). Kept apart from [`EndOfStream`](Self::EndOfStream)
    /// because on the one backend built for untrusted networks this is
    /// exactly what a truncation attack looks like; a plain socket has no
    /// close signal to miss and never reports it. The transport's own
    /// [`shutdown`](transport::RpcTransport::shutdown) refuses every send
    /// from that point, lets the one already in flight finish, and only
    /// then sends the signal and cuts the socket — so a deliberate close
    /// on this end is `EndOfStream` on the other, and every frame a sender
    /// was told went out is one the peer reads before it. The wait for
    /// that send is bounded so teardown stays finite: a peer that has
    /// stopped reading holds its sender past it, and reads the unclean end
    /// it was heading for anyway. Projects to
    /// [`StatusCode::DeadObject`](crate::StatusCode).
    UncleanEndOfStream,
    /// A frame was cut short: the length header itself arrived incomplete,
    /// or the header arrived in full and the body did not (peer closed
    /// mid-body, or declared more than it sent).
    Truncated,
    /// A read deadline elapsed part-way through a frame. The stream
    /// position is as lost as after [`Truncated`](Self::Truncated). The
    /// deadline is one of this end's (a reply deadline, a server's idle
    /// timeout): a socket read deadline expires as `EAGAIN`, while the
    /// kernel's own `ETIMEDOUT` is a lost connection and arrives as
    /// [`Io`](Self::Io).
    /// Projects to [`StatusCode::TimedOut`](crate::StatusCode).
    DeadlineMidFrame,
    /// A declared frame length exceeds [`transport::MAX_FRAME_LEN`].
    /// Rejected *before* any allocation (anti-OOM).
    FrameTooLarge {
        /// The length the peer (or caller) declared.
        declared: usize,
        /// The configured maximum ([`transport::MAX_FRAME_LEN`]).
        max: usize,
    },
    /// An underlying transport I/O error that is not a clean close.
    /// The kernel's `ETIMEDOUT` (`io::ErrorKind::TimedOut`: a `connect` that
    /// ran out of SYN retries, or TCP keepalive or retransmission giving up on
    /// a peer whose host stopped answering) is one: a lost connection, not a
    /// deadline of this end's. It projects through its errno, as every kind
    /// does, to [`StatusCode::TimedOut`](crate::StatusCode) — the status
    /// libbinder returns for the call that hit it (AOSP `RpcState.cpp`
    /// `handleRpcError` converts only `-ECONNRESET`, and `RpcSession.cpp`
    /// `singleSocketConnection` returns `-connErrno`). A serve loop records
    /// it as a lost connection, not an idle eviction
    /// ([`EndReason::Frame`]).
    Io(std::io::Error),
    /// A protocol-level violation (used by the wire codec).
    Protocol(&'static str),
    /// A wait deadline elapsed with no frame boundary crossed: a read that
    /// consumed nothing (a reply or negotiation deadline), or a send that
    /// put nothing on the wire. The stream stays frame-synchronized either
    /// way, but a reply, negotiation or send deadline still ends the
    /// session; only a serve loop's idle expiry between frames reads on
    /// (when the session was not idle). The deadline is
    /// one of this end's (`SO_RCVTIMEO`/`SO_SNDTIMEO`, which expire as
    /// `EAGAIN`); the kernel's own `ETIMEDOUT` is a lost connection and
    /// arrives as [`Io`](Self::Io).
    /// Projects to [`StatusCode::TimedOut`](crate::StatusCode).
    Timeout,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::EndOfStream => write!(f, "RPC stream ended"),
            RpcError::UncleanEndOfStream => {
                write!(f, "RPC stream ended without a close signal (truncated?)")
            }
            RpcError::Truncated => write!(f, "RPC frame truncated (incomplete body)"),
            RpcError::DeadlineMidFrame => {
                write!(f, "RPC read deadline elapsed part-way through a frame")
            }
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

impl RpcError {
    /// `EndOfStream` or `Timeout`: a failed read left a frame boundary; see "Frame-boundary reads".
    pub(crate) fn leaves_frame_boundary_intact(&self) -> bool {
        matches!(self, RpcError::EndOfStream | RpcError::Timeout)
    }
}

impl From<std::io::Error> for RpcError {
    /// Map a clean disconnect to [`RpcError::EndOfStream`]; everything
    /// else stays [`RpcError::Io`]. A truncated *body* is classified by
    /// the framing reader, not here. An `RpcError` that travelled as the
    /// `io::Error`'s payload (how the `Read` adapters carry
    /// [`RpcError::UncleanEndOfStream`] through the framing readers)
    /// comes back as itself rather than folded by kind.
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        let e = match e.downcast::<RpcError>() {
            Ok(rpc) => return rpc,
            Err(e) => e,
        };
        match e.kind() {
            UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted => {
                RpcError::EndOfStream
            }
            _ => RpcError::Io(e),
        }
    }
}

impl From<RpcError> for std::io::Error {
    /// Boundary projection back to `std::io::Error` for callers that
    /// hold an `io::Result<_>` accumulator. Its consumers are the
    /// adapters that bridge an [`transport::RpcTransport`] to a
    /// `std::io` `Read`/`Write` — `RawTransportIo`
    /// (`rpc::wire_android13`, the AOSP raw framing of both profiles) and
    /// `RawIo` (`rpc::transport::tls`, length-prefixed frames over TLS).
    ///
    /// `RpcError::Io` hands its payload back as-is, but the reverse
    /// direction folds the disconnect kinds, so an `Io` carrying one of
    /// them returns as `EndOfStream`.
    /// [`RpcError::EndOfStream`] projects onto a single representative
    /// disconnect kind (`BrokenPipe`): the four kinds `From<io::Error>`
    /// folds into `EndOfStream` are not distinguished, and a
    /// `EndOfStream` raised from a 0-byte read never had one. That kind
    /// folds back into `EndOfStream`, so a write-side disconnect on an
    /// android-13+ session stays `StatusCode::DeadObject` rather than
    /// degrading to an unclassified `Io(Other)` (`StatusCode::Unknown`)
    /// — the status a caller's dead-peer check keys on.
    /// [`RpcError::Timeout`] projects onto `WouldBlock`, the kind an
    /// expired socket deadline (`EAGAIN`) has, so the framing readers above
    /// an adapter see this end's deadline exactly as they see it on a raw
    /// socket. It is kind-preserving rather than variant-preserving, since
    /// `From<io::Error>` folds that kind into `Io(WouldBlock)`. `TimedOut`
    /// is not used for it: that kind is the kernel's `ETIMEDOUT`, a lost
    /// connection (`transport` module doc "Short reads and writes").
    /// [`RpcError::UncleanEndOfStream`] must survive the round trip —
    /// folding it by kind would turn it back into `EndOfStream`, the very
    /// thing it exists to be told apart from — so it goes out as an
    /// `UnexpectedEof` carrying itself as the payload, which
    /// `From<io::Error>` recovers first. `RpcError::DeadlineMidFrame` goes
    /// out the same way, as `WouldBlock` carrying itself, so the variant is
    /// at least recoverable on the far side: by kind alone it would come back
    /// as a boundary `Timeout`, the opposite of what it means. No reader takes
    /// it yet (each checks `is_timeout` by kind before the downcast), and no
    /// producer sends this variant across this boundary. The remaining
    /// variants have no `io::ErrorKind` that means what they mean, so they
    /// stay `Other`.
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::Io(io) => io,
            RpcError::EndOfStream => std::io::ErrorKind::BrokenPipe.into(),
            RpcError::Timeout => std::io::ErrorKind::WouldBlock.into(),
            e @ RpcError::UncleanEndOfStream => {
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, e)
            }
            // Carries itself, or it would read back as a boundary `Timeout` (see the fn doc).
            e @ RpcError::DeadlineMidFrame => {
                std::io::Error::new(std::io::ErrorKind::WouldBlock, e)
            }
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
            RpcError::EndOfStream => crate::StatusCode::DeadObject,
            RpcError::UncleanEndOfStream => crate::StatusCode::DeadObject,
            RpcError::Truncated => crate::StatusCode::NotEnoughData,
            RpcError::DeadlineMidFrame => crate::StatusCode::TimedOut,
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
/// transaction reaches: scalars, `String` and the generic `Vec<T>` array
/// path. (The binder reads below hit a stub `RpcParcelOps` that returns
/// before touching the address bytes — `RpcAddress` decoding is covered
/// by the `rpc_address_decode` target, not here.)
/// Property: no panic / OOM / UB / unbounded pre-allocation on *any*
/// input — every array length is bounded by the bytes actually present,
/// and every out-vec length by `MAX_OUT_VEC_BYTES` (AOSP `resizeOutVector`),
/// which is a byte cap, not a presence check.
/// Not part of the supported API surface.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub fn __fuzz_decode_rpc_parcel(input: &[u8]) {
    use crate::binder::SIBinder;
    use crate::error::{Result, StatusCode};
    use crate::parcel::{Parcel, RpcParcelOps};
    use std::sync::Arc;

    // Sessionless binder hook: drives `read::<SIBinder>` without a connection or address decode.
    struct NullOps;
    impl RpcParcelOps for NullOps {
        fn write_binder(&self, _b: Option<&SIBinder>, _p: &mut Parcel) -> Result<()> {
            Err(StatusCode::DeadObject)
        }
        fn read_binder(&self, _p: &mut Parcel) -> Result<Option<SIBinder>> {
            Err(StatusCode::DeadObject)
        }
        // No session, so no node table to give anything back to.
        fn cancel_leaving(&self, _addrs: &[crate::rpc::RpcAddress]) {}
        fn session_id(&self) -> *const () {
            std::ptr::null()
        }
        fn records_binder_positions(&self) -> Result<bool> {
            Ok(false)
        }
        fn acquire_copied(&self, _objects: &[&[u8]]) -> Result<crate::parcel::CopiedBinders> {
            Err(StatusCode::DeadObject)
        }
    }

    fn fresh(input: &[u8]) -> Parcel {
        let mut p = Parcel::data_only_from_vec(input.to_vec());
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
    let _ = fresh(input).resize_out_vec::<i32>(&mut Vec::new());
    let _ = fresh(input).resize_nullable_out_vec::<i64>(&mut None);
}

#[cfg(test)]
mod strong_session_tests;

#[cfg(test)]
mod tests {
    //! # Mutation gates
    //!
    //! - `peer_closed_and_timeout_round_trip_through_io_error`: `RpcError -> io::Error` must
    //!   preserve the `io::ErrorKind` for the two variants that have one meaning the same
    //!   thing. `RawTransportIo` crosses that boundary on every android-13+ handshake and on
    //!   every read and write of a non-fd session, so a variant that degrades to `Other` there
    //!   comes back as `Io(Other)`: a peer that closes before our write has to report
    //!   `StatusCode::DeadObject`, not `Unknown`. Which side of an exchange notices a
    //!   disconnect first is a host- and timing-dependent race, so both directions have to
    //!   classify alike. `EndOfStream` returns as the same variant; `Timeout` is only
    //!   kind-preserving — it comes back as `Io(WouldBlock)`. Mutant: mapping `EndOfStream`
    //!   through `other => io::Error::other(...)` fails the round trip (and the timeout arm
    //!   guards `read_exact_into`'s `is_timeout` path the same way).
    //! - `rpc_parcel_hostile_array_len_is_bounded_not_oom`: a hostile array length in an
    //!   RPC-mode parcel must fail with bounded pre-allocation and `Err`. Mutant: removing the
    //!   `min(len, data_avail())` / `len > data_avail()` guards turns each case into a multi-GB
    //!   allocation that aborts the test process. The out-vec cases have a different guard:
    //!   `check_out_vec_size` (the `MAX_OUT_VEC_BYTES` cap); removing it makes them allocate
    //!   instead of returning `NoMemory`. The `rpc_parcel_rpc_mode` fuzz target is the soak
    //!   supplement.

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
            assert!(matches!(e, RpcError::EndOfStream), "{kind:?} -> {e:?}");
        }
        // A non-disconnect I/O error stays Io(..).
        let other: RpcError = std::io::Error::from(std::io::ErrorKind::PermissionDenied).into();
        assert!(matches!(other, RpcError::Io(_)));
    }

    #[test]
    fn rpc_error_projects_onto_status_code() {
        assert_eq!(
            StatusCode::from(RpcError::EndOfStream),
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
        // The kernel's `ETIMEDOUT` projects through its errno, as libbinder's `-ETIMEDOUT` does.
        let etimedout = rustix::io::Errno::TIMEDOUT.raw_os_error();
        assert_eq!(
            StatusCode::from(RpcError::Io(std::io::Error::from_raw_os_error(etimedout))),
            StatusCode::TimedOut
        );
        assert_eq!(StatusCode::from(RpcError::Timeout), StatusCode::TimedOut);
    }

    /// `EndOfStream`/`Timeout` keep their `io::ErrorKind` via `io::Error`; see "Mutation gates".
    #[test]
    fn peer_closed_and_timeout_round_trip_through_io_error() {
        let io = std::io::Error::from(RpcError::EndOfStream);
        assert_eq!(io.kind(), std::io::ErrorKind::BrokenPipe);
        assert!(matches!(RpcError::from(io), RpcError::EndOfStream));
        assert_eq!(
            StatusCode::from(RpcError::from(std::io::Error::from(RpcError::EndOfStream))),
            StatusCode::DeadObject,
            "a write-side disconnect must stay DeadObject, not Unknown"
        );

        let io = std::io::Error::from(RpcError::Timeout);
        assert_eq!(io.kind(), std::io::ErrorKind::WouldBlock);
        assert!(
            transport::is_timeout(&io),
            "read_exact_into's deadline arm keys on this kind"
        );
        // `From<io::Error>` folds only disconnect kinds: a timeout returns as `Io(WouldBlock)`.
        assert!(
            matches!(RpcError::from(io), RpcError::Io(ref e) if transport::is_timeout(e)),
            "a timeout stays recognizable as one across the round trip"
        );

        // An `Io` payload round-trips unchanged; a variant with no matching kind is `Other`.
        let io = std::io::Error::from(RpcError::Io(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )));
        assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::io::Error::from(RpcError::Protocol("bad")).kind(),
            std::io::ErrorKind::Other
        );
    }

    /// cfg-gated `StatusCode::RpcError` round-trips Display and i32 both ways, colliding with none.
    #[test]
    fn status_code_rpc_error_roundtrips() {
        let v: i32 = StatusCode::RpcError.into();
        assert_eq!(StatusCode::from(v), StatusCode::RpcError);
        assert_eq!(format!("{}", StatusCode::RpcError), "RpcError");
        // Distinct from its neighbours in the UNKNOWN_ERROR + n block.
        assert_ne!(v, StatusCode::UnexpectedNull.into());
        assert_ne!(v, StatusCode::FailedTransaction.into());
    }

    /// A hostile RPC-parcel array length errors with bounded pre-allocation, never multi-GB.
    #[test]
    fn rpc_parcel_hostile_array_len_is_bounded_not_oom() {
        use crate::parcel::Parcel;

        // Generic `Vec<T>` array path, body empty after the length.
        let mut p = Parcel::new();
        p.set_for_rpc(true).unwrap();
        p.write(&i32::MAX).unwrap();
        p.set_data_position(0);
        assert!(
            p.read::<Vec<i32>>().is_err(),
            "hostile Vec<i32> len must error, not OOM"
        );

        // A little data present (data_avail() > 0 but << len).
        let mut p = Parcel::new();
        p.set_for_rpc(true).unwrap();
        p.write(&i32::MAX).unwrap();
        p.write(&7i32).unwrap();
        p.write(&8i32).unwrap();
        p.set_data_position(0);
        assert!(p.read::<Vec<i64>>().is_err());

        // Out-vec lengths are capped by byte size (AOSP 1 MB), not by `data_avail()`.
        for (len, want) in [(i32::MAX, Err(StatusCode::NoMemory)), (4, Ok(()))] {
            let mut p = Parcel::new();
            p.set_for_rpc(true).unwrap();
            p.write(&len).unwrap();
            p.set_data_position(0);
            assert_eq!(p.resize_out_vec::<i32>(&mut Vec::new()), want);
            p.set_data_position(0);
            let mut out: Option<Vec<i64>> = None;
            assert_eq!(p.resize_nullable_out_vec(&mut out), want);
        }
        let mut p = Parcel::new();
        p.write(&249_999i32).unwrap();
        p.write(&250_000i32).unwrap();
        p.set_data_position(0);
        let mut out = Vec::<i32>::new();
        assert_eq!(p.resize_out_vec(&mut out), Ok(()));
        assert_eq!(out.len(), 249_999);
        assert_eq!(p.resize_out_vec(&mut out), Err(StatusCode::NoMemory));

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
