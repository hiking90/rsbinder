// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What the transport under a binder can do, as a set of bits.
//!
//! rsbinder already expresses each transport difference as a type with a
//! single enforcement point: a parcel's fd mode decides whether
//! [`ParcelFileDescriptor`](crate::ParcelFileDescriptor) can be written,
// The target only exists with `rpc`, so only link it then.
#![cfg_attr(
    feature = "rpc",
    doc = "[`PeerIdentity`](crate::rpc::PeerIdentity) decides whether"
)]
#![cfg_attr(
    not(feature = "rpc"),
    doc = "`PeerIdentity` (`rpc` feature) decides whether"
)]
//! [`get_calling_uid`](crate::get_calling_uid) is vouched for, and the
//! entry layer refuses an option the endpoint does not have. Those checks
//! all answer at the moment of use.
//!
//! [`TransportCaps`] is the read-only summary of the same facts, available
//! *before* the first transaction. It exists so a feature that needs
//! something the transport lacks can say so up front, with a message
//! stating when the missing bit holds, instead of failing on the wire later.
//! It never replaces the checks it summarizes — see the type's own docs.

use crate::error::{Result, StatusCode};

/// What the transport under a binder can do.
///
/// **A summary, not the rule.** Every bit here is derived from a fact
/// some other type already owns, and that owner still enforces it: fd
/// writes are refused by the parcel's fd mode whether or not
/// [`FD_PASSING`](Self::FD_PASSING) was consulted, and
/// [`get_calling_uid`](crate::get_calling_uid) returns its fail-closed
/// sentinel whether or not [`TRUSTED_UID`](Self::TRUSTED_UID) was. Code
/// that skips the caps check is not less safe, only less informative
/// about why it failed.
///
/// Read it from [`Client::caps`](crate::Client::caps) (what this client
// The target only exists with `rpc`, so only link it then.
#[cfg_attr(
    feature = "rpc",
    doc = "has), [`RpcSession::caps`](crate::rpc::RpcSession::caps) (what this"
)]
#[cfg_attr(
    not(feature = "rpc"),
    doc = "has), `RpcSession::caps` (`rpc` feature) (what this"
)]
/// session negotiated), [`calling_caps`](crate::thread_state::calling_caps)
/// (what the in-flight call arrived over, read inside the handler serving
/// it), or [`Endpoint::static_caps`](crate::Endpoint::static_caps)
/// (what the transport can offer at all, before any negotiation).
///
/// ```
/// use rsbinder::TransportCaps;
///
/// let caps = TransportCaps::KERNEL;
/// assert!(caps.contains(TransportCaps::FD_PASSING));
/// assert!(caps.require(TransportCaps::CALLBACKS, "a callback").is_ok());
///
/// let tls = TransportCaps::NONE;
/// assert!(tls.require(TransportCaps::FD_PASSING, "a pipe").is_err());
/// ```
#[derive(Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct TransportCaps(u32);

impl TransportCaps {
    /// File descriptors cross this transport, so a
    /// [`ParcelFileDescriptor`](crate::ParcelFileDescriptor) or a shared
    /// memory region can be sent. Kernel binder always can; an RPC
    /// session can when it negotiated
    // The target only exists with `rpc`, so only link it then.
    #[cfg_attr(
        feature = "rpc",
        doc = "[`FileDescriptorTransportMode::Unix`](crate::rpc::FileDescriptorTransportMode),"
    )]
    #[cfg_attr(
        not(feature = "rpc"),
        doc = "`FileDescriptorTransportMode::Unix` (`rpc` feature),"
    )]
    /// which only a Unix-domain socket carries (`SCM_RIGHTS`).
    ///
    /// Enforced without this bit by the fd write itself, which returns
    /// [`StatusCode::FdsNotAllowed`].
    pub const FD_PASSING: Self = Self(1 << 0);

    /// [`get_calling_uid`](crate::get_calling_uid) is vouched for by
    /// something outside the peer's control: the kernel driver, or
    /// `SO_PEERCRED` on a Unix socket. Without it, that call returns a
    /// sentinel that is never a real uid, and a uid ACL can never match.
    ///
    /// A vsock cid, a TLS certificate and an anonymous TCP peer all lack
    /// it — a certificate identifies the peer, but not by uid.
    pub const TRUSTED_UID: Self = Self(1 << 1);

    /// Calls cross this transport in **both** directions outside a
    /// handler: a binder handed to the peer can be transacted on at any
    /// time, not only while dispatching one of the peer's calls. This is
    /// what a callback, a streaming sink or an out-of-band cancellation
    /// needs.
    ///
    /// Kernel binder always has it. An RPC session has it only when the
    /// client opened incoming connections
    /// ([`ClientOptions::incoming_connections`](crate::ClientOptions::incoming_connections)),
    /// and then **both** ends report it — they are the two ends of the
    /// same connections.
    ///
    /// A default one-connection RPC session does **not** have it, on
    /// either end. The client may still call the server whenever it likes
    /// over its founding connection; what is missing is the other
    /// direction, because a connection the client reads only inside its
    /// own reply wait has nobody to read a request written into it.
    pub const CALLBACKS: Self = Self(1 << 2);

    /// Both ends share a kernel, so a memory region mapped on one side can
    /// be mapped on the other and a pid means the same thing to both.
    /// Kernel binder and a Unix socket qualify; vsock crosses a VM
    /// boundary and TCP a host one.
    pub const SAME_HOST: Self = Self(1 << 3);

    /// The kernel binder driver is underneath, so its knobs apply: the
    /// receive mapping size, `TF_UPDATE_TXN`, scheduler inheritance,
    /// SELinux contexts. Never set on an RPC session, whose transport is
    /// a socket.
    pub const KERNEL_KNOBS: Self = Self(1 << 4);

    /// No capabilities — what a vsock or TLS session has. The incoming
    /// (callback) connections *rsbinder* opens are Unix-only today, so an
    /// rsbinder client cannot reach [`CALLBACKS`](Self::CALLBACKS) there
    /// either. A server can: its accept path is transport-generic, so a
    /// peer that attaches an incoming connection over vsock or TLS — as
    /// AOSP's `RpcSession` does — makes that session report `CALLBACKS`
    /// and nothing else.
    pub const NONE: Self = Self(0);

    /// Everything: kernel binder, which is the only transport that has
    /// every bit.
    pub const KERNEL: Self = Self(0b1_1111);

    /// Whether every bit in `other` is set here. `contains(NONE)` is
    /// always true.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The union of two sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The bits of `other` removed from this set.
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Whether no bit is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw bits, for logging or a bit-exact test assertion. The
    /// numeric value of each flag is not part of the API contract; use
    /// the constants.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Check a requirement before the work starts.
    ///
    /// `Ok(())` when every bit in `needed` is present. Otherwise logs
    /// what `what` needed and which bits are missing, and returns
    /// [`StatusCode::InvalidOperation`] — AOSP's code for "this transport
    /// cannot do this", as opposed to
    /// [`StatusCode::BadValue`](crate::StatusCode::BadValue) for a
    /// malformed request.
    ///
    /// The point is the timing: a streaming sink or a cancellation that
    /// needs [`CALLBACKS`](Self::CALLBACKS) fails here, at setup, with a
    /// log line saying when the missing bit holds, rather than on a
    /// transaction minutes later.
    pub fn require(self, needed: Self, what: &str) -> Result<()> {
        if self.contains(needed) {
            return Ok(());
        }
        let missing = needed.difference(self);
        log::error!(
            "rsbinder: {what} needs {missing} (this transport has {self}); \
             {}",
            missing.remedy()
        );
        Err(StatusCode::InvalidOperation)
    }

    /// One clause per missing bit, joined by `; `, for the
    /// [`require`](Self::require) log. Each states when the bit holds, not
    /// a procedure: the reader may be on either end of any transport, and
    /// a procedure is right for only one of those positions.
    ///
    /// Every missing bit gets its own clause, because acting on only the
    /// first one leaves the call failing for the bits it did not mention.
    fn remedy(self) -> String {
        let mut out = String::new();
        for (bit, advice) in [
            (
                Self::CALLBACKS,
                "CALLBACKS holds on kernel binder, and on an RPC session whose \
                 client end opened incoming connections — see \
                 `TransportCaps::CALLBACKS`",
            ),
            (
                Self::FD_PASSING,
                "FD_PASSING holds on kernel binder, and on a Unix-socket RPC \
                 session that negotiated `FileDescriptorTransportMode::Unix`",
            ),
            (
                Self::TRUSTED_UID,
                "TRUSTED_UID holds on kernel binder and Unix-socket RPC, where \
                 the kernel vouches for the peer's uid",
            ),
            (
                Self::SAME_HOST,
                "SAME_HOST holds on kernel binder and Unix-socket RPC, where \
                 both ends share a kernel",
            ),
            (
                Self::KERNEL_KNOBS,
                "KERNEL_KNOBS holds on kernel binder only",
            ),
        ] {
            if self.contains(bit) {
                if !out.is_empty() {
                    out.push_str("; ");
                }
                out.push_str(advice);
            }
        }
        out
    }
}

impl core::ops::BitOr for TransportCaps {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl core::ops::BitOrAssign for TransportCaps {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

impl core::fmt::Display for TransportCaps {
    /// `FD_PASSING|TRUSTED_UID|SAME_HOST`, or `NONE` when empty.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_empty() {
            return f.write_str("NONE");
        }
        let mut first = true;
        for (bit, name) in [
            (Self::FD_PASSING, "FD_PASSING"),
            (Self::TRUSTED_UID, "TRUSTED_UID"),
            (Self::CALLBACKS, "CALLBACKS"),
            (Self::SAME_HOST, "SAME_HOST"),
            (Self::KERNEL_KNOBS, "KERNEL_KNOBS"),
        ] {
            if self.contains(bit) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

impl core::fmt::Debug for TransportCaps {
    /// `TransportCaps(FD_PASSING|TRUSTED_UID)` — the flag names, not the
    /// number, so a failing assertion says which bit is missing.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "TransportCaps({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_algebra() {
        let unix =
            TransportCaps::FD_PASSING | TransportCaps::TRUSTED_UID | TransportCaps::SAME_HOST;
        assert!(unix.contains(TransportCaps::FD_PASSING));
        assert!(unix.contains(TransportCaps::FD_PASSING | TransportCaps::SAME_HOST));
        assert!(!unix.contains(TransportCaps::CALLBACKS));
        assert!(!unix.contains(TransportCaps::KERNEL));
        // Every set contains the empty one, and none is empty once a bit
        // is set.
        assert!(unix.contains(TransportCaps::NONE));
        assert!(TransportCaps::NONE.contains(TransportCaps::NONE));
        assert!(TransportCaps::NONE.is_empty());
        assert!(!unix.is_empty());
        assert_eq!(
            unix.difference(TransportCaps::FD_PASSING),
            TransportCaps::TRUSTED_UID | TransportCaps::SAME_HOST
        );
        // `KERNEL` is exactly the five bits, so no bit can be added to it.
        assert_eq!(
            TransportCaps::KERNEL,
            TransportCaps::FD_PASSING
                | TransportCaps::TRUSTED_UID
                | TransportCaps::CALLBACKS
                | TransportCaps::SAME_HOST
                | TransportCaps::KERNEL_KNOBS
        );
        assert_eq!(TransportCaps::default(), TransportCaps::NONE);
        let mut acc = TransportCaps::NONE;
        acc |= TransportCaps::CALLBACKS;
        assert_eq!(acc, TransportCaps::CALLBACKS);
    }

    #[test]
    fn require_reports_only_what_is_missing() {
        let tls = TransportCaps::NONE;
        assert_eq!(
            tls.require(TransportCaps::CALLBACKS, "streaming sink"),
            Err(StatusCode::InvalidOperation)
        );
        assert_eq!(
            TransportCaps::KERNEL.require(TransportCaps::KERNEL, "x"),
            Ok(())
        );
        // A requirement that is already met on a partial set.
        let unix = TransportCaps::FD_PASSING | TransportCaps::TRUSTED_UID;
        assert_eq!(unix.require(TransportCaps::FD_PASSING, "a pipe"), Ok(()));
        assert_eq!(
            unix.require(
                TransportCaps::FD_PASSING | TransportCaps::CALLBACKS,
                "a pipe handed to a callback"
            ),
            Err(StatusCode::InvalidOperation)
        );
        // Requiring nothing always succeeds, including on an empty set.
        assert_eq!(tls.require(TransportCaps::NONE, "nothing"), Ok(()));
    }

    #[test]
    fn display_names_the_bits() {
        assert_eq!(TransportCaps::NONE.to_string(), "NONE");
        assert_eq!(
            TransportCaps::KERNEL.to_string(),
            "FD_PASSING|TRUSTED_UID|CALLBACKS|SAME_HOST|KERNEL_KNOBS"
        );
        assert_eq!(
            (TransportCaps::TRUSTED_UID | TransportCaps::SAME_HOST).to_string(),
            "TRUSTED_UID|SAME_HOST"
        );
        assert_eq!(
            format!("{:?}", TransportCaps::CALLBACKS),
            "TransportCaps(CALLBACKS)"
        );
    }
}
