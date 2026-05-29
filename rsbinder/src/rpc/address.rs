// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcAddress` — RPC object identity.
//!
//! Mirrors android-12 r34 `RpcWireAddress` (`u8 address[32]`, opaque —
//! `RpcAddress.h:34` "hide the ABI ... potentially change the size").
//! This is **not** a u32 kernel handle: the RPC stack has its own
//! identity space, decoupled from `proxy::ProxyHandle`.
//!
//! Allocation is a per-session monotonic counter (not a CSPRNG —
//! uniqueness *within a session* is sufficient; android fills
//! it from `/dev/urandom` but treats it as opaque, so a counter is
//! wire-compatible). `zero()` is the reserved address used for the
//! special server-channel transactions.

use std::fmt;

/// On-wire length of an `RpcWireAddress` (android-12 r34: `u8[32]`).
pub(crate) const RPC_ADDR_LEN: usize = 32;

/// New-session sentinel for the raw `int32` session-id preamble
/// (`RPC_SESSION_ID_NEW = -1` in android-12 r34).
pub const RPC_SESSION_ID_NEW: i32 = -1;

/// Opaque RPC object address. ABI is hidden: no public field, no public
/// length constant, `Debug` shows only a short fingerprint.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RpcAddress {
    bytes: [u8; RPC_ADDR_LEN],
}

impl RpcAddress {
    /// The reserved all-zero address. In r34 this is the special
    /// server channel target for `GET_ROOT`/`GET_MAX_THREADS`/
    /// `GET_SESSION_ID` (see [`SpecialTransaction`]).
    pub fn zero() -> Self {
        RpcAddress {
            bytes: [0u8; RPC_ADDR_LEN],
        }
    }

    /// `true` iff this is [`RpcAddress::zero`].
    pub fn is_zero(&self) -> bool {
        self.bytes == [0u8; RPC_ADDR_LEN]
    }

    /// Allocate a fresh address from a per-session monotonic counter,
    /// **namespaced by connection role**.
    ///
    /// Both endpoints of a connection allocate into the *same on-wire
    /// address space* (a callback the initiator sends to the acceptor
    /// gets an address the acceptor must not confuse with one it minted
    /// itself). A bare per-session counter collides across the two
    /// peers (both start at 1). android fills the 32 bytes from
    /// `/dev/urandom`; rsbinder instead tags byte 8 with the
    /// allocating role so the initiator's and acceptor's subspaces are
    /// disjoint — uniqueness then holds across the *whole connection*,
    /// not just one endpoint. The counter is session-owned (no global);
    /// the value is never `zero()` (counter ≥ 1).
    pub fn unique(counter: &mut u64, space: AddressSpace) -> Self {
        *counter = counter.wrapping_add(1);
        let mut bytes = [0u8; RPC_ADDR_LEN];
        bytes[..8].copy_from_slice(&counter.to_le_bytes());
        bytes[8] = space.tag();
        RpcAddress { bytes }
    }

    /// Borrow the raw 32 wire bytes (crate-internal — wire codec only).
    pub(crate) fn as_wire_bytes(&self) -> &[u8; RPC_ADDR_LEN] {
        &self.bytes
    }

    /// Reconstruct from exactly 32 wire bytes (crate-internal).
    pub(crate) fn from_wire_bytes(bytes: [u8; RPC_ADDR_LEN]) -> Self {
        RpcAddress { bytes }
    }
}

impl fmt::Debug for RpcAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Fingerprint only — never expose the full ABI representation.
        if self.is_zero() {
            return write!(f, "RpcAddress(zero)");
        }
        write!(
            f,
            "RpcAddress({:02x}{:02x}{:02x}{:02x}…)",
            self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3]
        )
    }
}

/// Which endpoint of a connection is allocating an [`RpcAddress`].
/// Tags the address so the connection initiator's and acceptor's
/// monotonic subspaces never collide on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressSpace {
    /// The side that `connect`ed (RPC client).
    Initiator,
    /// The side that `accept`ed (RPC server).
    Acceptor,
}

impl AddressSpace {
    fn tag(self) -> u8 {
        match self {
            AddressSpace::Initiator => 1,
            AddressSpace::Acceptor => 2,
        }
    }
}

/// Special transactions targeting the reserved zero address
/// (android-12 r34 `RpcWireFormat.h`). These travel as a normal
/// `TRANSACT` command whose `RpcWireTransaction.address` is
/// [`RpcAddress::zero`] and whose `code` is one of these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SpecialTransaction {
    /// Fetch the server's root object.
    GetRoot = 0,
    /// Negotiate the server's max thread count.
    GetMaxThreads = 1,
    /// Obtain the server-assigned session id.
    GetSessionId = 2,
    /// Negotiate the FD-over-RPC mode. **Not r34** — an
    /// rsbinder/android-13+ extension sent *only* when a client opts
    /// into FD passing, so the default (`None`) path stays r34-faithful.
    GetFdMode = 3,
}

impl SpecialTransaction {
    /// The raw `code` value carried on the wire.
    pub fn code(self) -> u32 {
        self as u32
    }

    /// Decode a zero-address transaction `code`.
    pub fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(SpecialTransaction::GetRoot),
            1 => Some(SpecialTransaction::GetMaxThreads),
            2 => Some(SpecialTransaction::GetSessionId),
            3 => Some(SpecialTransaction::GetFdMode),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_distinct_and_stable() {
        assert!(RpcAddress::zero().is_zero());
        assert_eq!(RpcAddress::zero(), RpcAddress::zero());
        assert_eq!(RpcAddress::zero().as_wire_bytes(), &[0u8; RPC_ADDR_LEN]);
    }

    /// 1e6 `unique()` calls in one session collide 0 times and never
    /// equal `zero()`.
    #[test]
    fn unique_is_collision_free_and_nonzero() {
        let mut ctr = 0u64;
        let mut seen = std::collections::HashSet::new();
        let zero = RpcAddress::zero();
        for _ in 0..1_000_000 {
            let a = RpcAddress::unique(&mut ctr, AddressSpace::Initiator);
            assert!(!a.is_zero(), "unique() must never be zero");
            assert_ne!(a, zero);
            assert!(seen.insert(a), "unique() collision");
        }
        assert_eq!(seen.len(), 1_000_000);
    }

    #[test]
    fn debug_is_fingerprint_only() {
        // Debug must not dump the full ABI bytes.
        let mut ctr = 0u64;
        let a = RpcAddress::unique(&mut ctr, AddressSpace::Initiator);
        let s = format!("{a:?}");
        assert!(s.starts_with("RpcAddress("));
        assert!(s.contains('…'), "Debug must be a truncated fingerprint");
        assert_eq!(format!("{:?}", RpcAddress::zero()), "RpcAddress(zero)");
    }

    #[test]
    fn wire_bytes_roundtrip() {
        let mut ctr = 41u64;
        let a = RpcAddress::unique(&mut ctr, AddressSpace::Initiator);
        let b = RpcAddress::from_wire_bytes(*a.as_wire_bytes());
        assert_eq!(a, b);
    }

    #[test]
    fn special_transaction_codes_match_r34() {
        assert_eq!(SpecialTransaction::GetRoot.code(), 0);
        assert_eq!(SpecialTransaction::GetMaxThreads.code(), 1);
        assert_eq!(SpecialTransaction::GetSessionId.code(), 2);
        assert_eq!(
            SpecialTransaction::from_code(2),
            Some(SpecialTransaction::GetSessionId)
        );
        // 0..=2 are the android-12 r34 special codes. Code 3
        // (`GetFdMode`) is the rsbinder FD-mode extension — explicitly
        // NOT r34, sent only when a client opts into FD passing.
        assert_eq!(
            SpecialTransaction::from_code(3),
            Some(SpecialTransaction::GetFdMode)
        );
        assert_eq!(SpecialTransaction::from_code(4), None);
    }
}
