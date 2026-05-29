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
//! reject. Non-UDS transports (`mem`/`vsock`/`tls`) can never select
//! `Unix` — enforced by type (the transport trait's default
//! fd methods reject) and by negotiation.

/// How (if at all) file descriptors may cross an RPC session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileDescriptorTransportMode {
    /// No FDs (android-12 fidelity / android-13 default). The
    /// categorical reject path, unchanged.
    #[default]
    None,
    /// FDs via UDS `SCM_RIGHTS`, **iff** both peers opted in and the
    /// transport is a Unix domain socket.
    Unix,
}

// Negotiation is a one-shot `GET_FD_MODE` exchange driven by
// `RpcSession::negotiate_fd_transport` / `RpcSessionInner::serve_special`:
// the client sends "want Unix? 1/0", the server replies the agreed
// mode (1=Unix iff both opted in, else 0). `Unix` requires *both*
// peers to opt in — otherwise a safe `None` fallback, never an error.
