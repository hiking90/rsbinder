// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Subplan 2-13 (A.4 + A.5): the `IAccessor` bridge that turns the
//! `Service::Accessor` arm of [`hub::android_16::get_service`]
//! /[`check_service`](super::servicemanager_16) — historically dropped
//! to `None` with a warning — into a usable RPC root binder.
//!
//! AOSP `BackendUnifiedServiceManager::toBinder` (android-16.0.0_r4):
//! 1. `getService2(name)` → `Service::accessor` (kernel-binder remote
//!    `IAccessor`),
//! 2. `validateAccessor` → `getInstanceName() == name`,
//! 3. `addConnection()` → connected `unique_fd`,
//! 4. `RpcSession::setupPreconnectedClient(fd, request)` → `getRootObject()`.
//!
//! rsbinder follows the same five-step path. The new piece this module
//! introduces is the *session lifetime owner*
//! ([`AccessorRoot`]): the user-returned `ServiceWithMetadata.service`
//! must keep the underlying [`RpcSession`] alive (the [`RpcProxy`]
//! itself only holds a `Weak<RpcSessionInner>` — once the session drops
//! the connection drops and the proxy's next call is `DeadObject`).
//! [`AccessorRoot`] is a tiny `IBinder` wrapper that holds the proxy
//! `SIBinder` and a strong [`RpcSession`] side-by-side; dropping the
//! wrapper drops the inner proxy first (best-effort `DEC_STRONG`) and
//! then the session (tears down the socket — peer-side cleanup is the
//! normal "peer closed" `serve_blocking` exit, AOSP-faithful).
//!
//! All of this is `cfg(feature = "rpc")` — the `rpc`-OFF build keeps
//! the legacy `Service::Accessor → log + None` arm byte-for-byte
//! ([`super::servicemanager_16`] cfg guards the whole resolve path).
//!
//! Process-wide state: **none**. There is no global registry, no
//! `OnceLock<Mutex<Vec<RpcSession>>>` (P6 would forbid it); the session
//! lives exactly as long as the wrapper the caller holds.

#![cfg(feature = "rpc")]

use std::any::Any;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Weak};

use crate::binder::{
    DeathRecipient, IBinder, Interface, SIBinder, Stability, Transactable, TransactionCode,
    WIBinder,
};
use crate::error::Result;
use crate::parcel::Parcel;
use crate::rpc::RpcSession;

// Generated `IAccessor` stub (build.rs A0.1). The accessor AIDL is its
// own AIDL output so the `IServiceManager.aidl` transitive resolution
// stays unchanged.
include!(concat!(env!("OUT_DIR"), "/accessor_16.rs"));

pub use android::os::IAccessor::{
    BnAccessor, BpAccessor, IAccessor, IAccessorAsync, IAccessorAsyncService, IAccessorDefault,
    IAccessorDefaultRef, ERROR_CONNECTION_INFO_NOT_FOUND, ERROR_FAILED_TO_CONNECT_EACCES,
    ERROR_FAILED_TO_CONNECT_TO_SOCKET, ERROR_FAILED_TO_CREATE_SOCKET,
    ERROR_UNSUPPORTED_SOCKET_FAMILY,
};

/// Wrapper `IBinder` that pins the underlying [`RpcSession`] alive for
/// as long as the user holds the binder returned by the accessor
/// bridge. Implementation detail of [`resolve_accessor`].
///
/// `as_any` deliberately forwards to the inner `RpcProxy` — that is
/// what the AIDL-generated `Bp*` stubs and
/// `<dyn IBinder>::as_remote()`/`as_proxy()` downcast against, so the
/// wrapper is transparent to consumers. The inner proxy is the only
/// concrete `IBinder` anyone outside this module needs to recognise.
pub(crate) struct AccessorRoot {
    /// The RPC root `SIBinder` returned by `RpcSession::get_root`. Owns
    /// an `Arc<RpcProxy>` (one of two: the other is cached inside
    /// `RpcSessionInner.state` and dies with the session).
    inner: SIBinder,
    /// Strong handle to the RPC session. Drops *after* `inner` thanks
    /// to declaration order, so the inner `RpcProxy::drop` still finds
    /// a live session to send `DEC_STRONG` through (best-effort —
    /// `RpcProxy` tolerates a dead session).
    _session: RpcSession,
}

impl AccessorRoot {
    fn into_sibinder(inner: SIBinder, session: RpcSession) -> Result<SIBinder> {
        SIBinder::new(Arc::new(AccessorRoot {
            inner,
            _session: session,
        }))
    }

    fn inner_binder(&self) -> &dyn IBinder {
        &**self.inner.as_arc()
    }
}

impl Interface for AccessorRoot {
    fn as_binder(&self) -> SIBinder {
        self.inner.clone()
    }
}

impl IBinder for AccessorRoot {
    fn link_to_death(&self, recipient: Weak<dyn DeathRecipient>) -> Result<()> {
        self.inner_binder().link_to_death(recipient)
    }

    fn unlink_to_death(&self, recipient: Weak<dyn DeathRecipient>) -> Result<()> {
        self.inner_binder().unlink_to_death(recipient)
    }

    fn ping_binder(&self) -> Result<()> {
        self.inner_binder().ping_binder()
    }

    /// Deliberate forward: `as_any` reports the **inner** `RpcProxy`
    /// rather than `AccessorRoot` itself so `<dyn IBinder>::as_remote`
    /// / `as_proxy` and `__rpc_stamp_descriptor` see the concrete
    /// proxy type they expect. `AccessorRoot` is `pub(crate)` and
    /// nothing in the codebase downcasts to it.
    fn as_any(&self) -> &dyn Any {
        self.inner_binder().as_any()
    }

    fn as_transactable(&self) -> Option<&dyn Transactable> {
        self.inner_binder().as_transactable()
    }

    fn descriptor(&self) -> &str {
        self.inner_binder().descriptor()
    }

    fn is_remote(&self) -> bool {
        self.inner_binder().is_remote()
    }

    fn stability(&self) -> Stability {
        self.inner_binder().stability()
    }

    fn rpc_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        self.inner_binder().rpc_transact(code, reader, reply)
    }

    fn inc_strong(&self, strong: &SIBinder) -> Result<()> {
        self.inner_binder().inc_strong(strong)
    }

    fn attempt_inc_strong(&self) -> bool {
        self.inner_binder().attempt_inc_strong()
    }

    fn dec_strong(&self, strong: Option<ManuallyDrop<SIBinder>>) -> Result<()> {
        self.inner_binder().dec_strong(strong)
    }

    fn inc_weak(&self, weak: &WIBinder) -> Result<()> {
        self.inner_binder().inc_weak(weak)
    }

    fn dec_weak(&self) -> Result<()> {
        self.inner_binder().dec_weak()
    }
}

/// AOSP `BackendUnifiedServiceManager::toBinder` for the
/// `Service::Accessor` arm (android-16). Resolve `accessor` (a kernel
/// remote `IAccessor`) into an RPC-root `ServiceWithMetadata`:
///
/// 1. Cast to `BpAccessor` (kernel proxy — `addConnection`/
///    `getInstanceName` ride the kernel binder driver).
/// 2. `validateAccessor`: `getInstanceName() == name` — AOSP requires
///    this so a misbehaving / hijacked Accessor cannot impersonate a
///    different service name.
/// 3. `addConnection()` — server-side connected socket fd. Failures
///    surface as `Status::service_specific_error()` with one of the
///    five `IAccessor::ERROR_*` codes (decoded by the caller via
///    [`accessor_error_name`] in B.6 logging).
/// 4. `RpcSession::from_preconnected_fd(fd, max_version=2)` —
///    android-16 max wire version.
/// 5. `get_root()` → wrap in [`AccessorRoot`] so the session stays
///    alive until the caller drops the returned binder.
///
/// Returns `None` on any failure (instance-name mismatch, transport
/// rejection, root not available). The caller logs the original
/// `Status`/`StatusCode` separately so the silent-`None` is informative
/// rather than mute.
pub fn resolve_accessor(
    name: &str,
    accessor: SIBinder,
) -> Option<super::servicemanager_16::android::os::ServiceWithMetadata::ServiceWithMetadata> {
    // Step 1: kernel `BpAccessor`. `<dyn IAccessor>::try_from(SIBinder)`
    // is the generated FromIBinder hook.
    let bp = match <dyn IAccessor as crate::FromIBinder>::try_from(accessor) {
        Ok(bp) => bp,
        Err(e) => {
            log::warn!("IAccessor cast failed for {name}: {e}");
            return None;
        }
    };

    // Step 2: instance-name validation (AOSP `validateAccessor`).
    match bp.getInstanceName() {
        Ok(reported) if reported == name => {}
        Ok(other) => {
            log::warn!(
                "IAccessor instance-name mismatch for {name}: reported {other:?} — rejecting"
            );
            return None;
        }
        Err(status) => {
            log::warn!(
                "IAccessor::getInstanceName failed for {name}: {status} \
                 (service_specific={})",
                accessor_error_name(status.service_specific_error())
            );
            return None;
        }
    }

    // Step 3: server-side connected fd.
    let pfd = match bp.addConnection() {
        Ok(pfd) => pfd,
        Err(status) => {
            log::warn!(
                "IAccessor::addConnection failed for {name}: {status} \
                 (service_specific={})",
                accessor_error_name(status.service_specific_error())
            );
            return None;
        }
    };
    let fd: std::os::fd::OwnedFd = pfd.into();

    // Step 4: adopt the fd and run the android-13+ versioned handshake
    // with the android-16 ceiling (max_version=2). Version negotiation
    // happens on the wire — the peer can downgrade to v0/v1 if it's an
    // older Accessor.
    let session = match RpcSession::from_preconnected_fd(fd, 2) {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "RpcSession::from_preconnected_fd failed for {name}: {e} (code={:?})",
                e
            );
            return None;
        }
    };

    // Step 5: get the RPC root and wrap with a session-keeper.
    let root = match session.get_root() {
        Ok(b) => b,
        Err(e) => {
            log::warn!("RPC get_root failed for {name}: {e}");
            return None;
        }
    };

    let wrapped = match AccessorRoot::into_sibinder(root, session) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("AccessorRoot wrap failed for {name}: {e}");
            return None;
        }
    };

    Some(
        super::servicemanager_16::android::os::ServiceWithMetadata::ServiceWithMetadata {
            r#service: Some(wrapped),
            // RPC roots are not `LazyService`s — they are not registered
            // through `registerLazyService` and have no shutdown hook;
            // AOSP `setSessionSpecificRoot` always keeps the binder hot
            // for the session's lifetime. Match that here.
            r#isLazyService: false,
        },
    )
}

/// Map an `IAccessor::ERROR_*` service-specific code (subplan 2-13 B.6)
/// to its symbolic name for logging. `0` is also the
/// `CONNECTION_INFO_NOT_FOUND` code (AIDL constant value) — when the
/// inbound `Status` has no `ServiceSpecific` variant the caller passes
/// `service_specific_error()`'s `0` fallback, so an unknown `0` reads
/// as `ERROR_CONNECTION_INFO_NOT_FOUND` — fine for a log hint; the
/// underlying `Status::Display` line is authoritative.
pub fn accessor_error_name(code: i32) -> &'static str {
    match code {
        ERROR_CONNECTION_INFO_NOT_FOUND => "ERROR_CONNECTION_INFO_NOT_FOUND",
        ERROR_FAILED_TO_CREATE_SOCKET => "ERROR_FAILED_TO_CREATE_SOCKET",
        ERROR_FAILED_TO_CONNECT_TO_SOCKET => "ERROR_FAILED_TO_CONNECT_TO_SOCKET",
        ERROR_FAILED_TO_CONNECT_EACCES => "ERROR_FAILED_TO_CONNECT_EACCES",
        ERROR_UNSUPPORTED_SOCKET_FAMILY => "ERROR_UNSUPPORTED_SOCKET_FAMILY",
        _ => "unknown",
    }
}

/// Fuzz hook (subplan 2-13 B.6 / V4): feed an arbitrary big-endian i32
/// byte payload into [`accessor_error_name`] and assert it never
/// panics, allocates indefinitely, or returns a non-`'static str`. The
/// returned string is intentionally consumed via `std::hint::black_box`
/// so the optimiser cannot DCE the lookup on builds that inline it.
#[doc(hidden)]
pub fn __fuzz_accessor_error_decode(input: &[u8]) {
    // Pad / truncate to exactly 4 bytes — any extra is ignored, any
    // missing reads as zero. Equivalent to the AIDL wire's `i32`
    // serialization range.
    let mut buf = [0u8; 4];
    let n = input.len().min(4);
    buf[..n].copy_from_slice(&input[..n]);
    let code = i32::from_le_bytes(buf);
    let _ = std::hint::black_box(accessor_error_name(code));
}

#[cfg(test)]
mod tests {
    //! Subplan 2-13 B.6 deterministic regression: the
    //! `ServiceSpecificError` decode is the gate that survives without
    //! a nightly fuzz infrastructure (the master plan calls these the
    //! *enforceable* adversarial-input gates; the libFuzzer target is
    //! the soak supplement).

    use super::*;

    #[test]
    fn accessor_error_name_covers_all_aosp_codes() {
        assert_eq!(
            accessor_error_name(ERROR_CONNECTION_INFO_NOT_FOUND),
            "ERROR_CONNECTION_INFO_NOT_FOUND"
        );
        assert_eq!(
            accessor_error_name(ERROR_FAILED_TO_CREATE_SOCKET),
            "ERROR_FAILED_TO_CREATE_SOCKET"
        );
        assert_eq!(
            accessor_error_name(ERROR_FAILED_TO_CONNECT_TO_SOCKET),
            "ERROR_FAILED_TO_CONNECT_TO_SOCKET"
        );
        assert_eq!(
            accessor_error_name(ERROR_FAILED_TO_CONNECT_EACCES),
            "ERROR_FAILED_TO_CONNECT_EACCES"
        );
        assert_eq!(
            accessor_error_name(ERROR_UNSUPPORTED_SOCKET_FAMILY),
            "ERROR_UNSUPPORTED_SOCKET_FAMILY"
        );
    }

    #[test]
    fn accessor_error_name_unknown_is_safe() {
        // Out-of-range values (positive overflow, negative, max/min)
        // must never panic and must return the fallback string.
        for code in [5, 6, 1_000, -1, -1_000, i32::MAX, i32::MIN] {
            assert_eq!(accessor_error_name(code), "unknown", "code {code}");
        }
    }

    #[test]
    fn fuzz_accessor_error_decode_never_panics() {
        // Mirrors what a libFuzzer corpus drive would do — exercise the
        // shrink/grow inputs at the boundaries plus a sweep across the
        // signed-i32 cardinal points.
        __fuzz_accessor_error_decode(&[]);
        __fuzz_accessor_error_decode(&[0]);
        __fuzz_accessor_error_decode(&[0, 0]);
        __fuzz_accessor_error_decode(&[0, 0, 0]);
        __fuzz_accessor_error_decode(&[0, 0, 0, 0]);
        __fuzz_accessor_error_decode(&[1, 2, 3, 4, 5, 6, 7, 8]);
        __fuzz_accessor_error_decode(&u32::MAX.to_le_bytes());
        __fuzz_accessor_error_decode(&i32::MAX.to_le_bytes());
        __fuzz_accessor_error_decode(&i32::MIN.to_le_bytes());
    }
}
