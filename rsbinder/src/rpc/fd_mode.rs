// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `FileDescriptorTransportMode`.
//!
//! android-12 r34 forbids FDs in RPC parcels **categorically**; the
//! default path implements that faithful reject. android-13+ adds an
//! **opt-in** mode where, *only if both peers agree and the transport
//! is a Unix domain socket*, file descriptors travel out-of-band via
//! `SCM_RIGHTS`. The default is permanently
//! [`FileDescriptorTransportMode::None`] — which is
//! both android-13's default and bit-identical to the categorical
//! reject. Negotiation does not check the transport: `Unix` may be agreed
//! on a non-UDS transport (`mem`/`vsock`/`tls`), and every fd send there
//! then fails (the transport trait's default fd methods reject; `caps`
//! omits `FD_PASSING`).
//!
//! android-13+ fixes the mode in the connection header. On r34,
//! negotiation is a one-shot `GET_FD_MODE` exchange driven by
//! `RpcSession::negotiate_fd_transport` / `RpcSessionInner::serve_special`:
//! the client sends "want Unix? 1/0", the server replies the agreed mode
//! (1 = Unix iff both opted in, else 0). `Unix` requires *both* peers to opt
//! in; otherwise the session falls back to `None`, never an error.
//! `GET_FD_MODE` is an rsbinder extension on the r34 wire only: an
//! android-13+ server answers it `UNKNOWN_TRANSACTION`, as AOSP libbinder
//! does, and on android-13+ `negotiate_fd_transport` sends nothing and
//! refuses a `Unix` request the header did not agree
//! (`StatusCode::InvalidOperation`).

/// How (if at all) file descriptors may cross an RPC session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileDescriptorTransportMode {
    /// No FDs (android-12 r34 / android-13 default): every FD in a parcel is refused.
    #[default]
    None,
    /// FDs via UDS `SCM_RIGHTS` once both peers opted in — in the
    /// android-13+ connection header, or by r34 `GET_FD_MODE`. On a
    /// non-UDS transport every fd send fails (see `RpcSession::caps`).
    Unix,
}
