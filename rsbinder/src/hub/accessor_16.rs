// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The `IAccessor` bridge that turns the
//! `Service::Accessor` arm of [`hub::android_16::get_service`](crate::hub::android_16::get_service)
//! /[`check_service`](super::servicemanager_16) into a usable RPC root
//! binder.
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
//! must keep the underlying [`RpcSession`] alive — once the session drops
//! the connection drops and the proxy's next call is `DeadObject`.
//! [`AccessorRoot`] is a tiny `IBinder` wrapper that holds the proxy
//! `SIBinder` and a strong [`RpcSession`] side-by-side; dropping the
//! wrapper drops the inner proxy first (best-effort `DEC_STRONG`) and
//! then the session (tears down the socket — peer-side cleanup is the
//! normal "peer closed" `serve_blocking` exit, AOSP-faithful).
//!
//! # `AccessorRoot`
//!
//! The wrapper pins the [`RpcSession`] for as long as the user holds the binder the accessor
//! bridge returned; it is an implementation detail of [`try_resolve_accessor`]. `as_any`
//! deliberately forwards to the inner `RpcProxy` — that is what the AIDL-generated `Bp*`
//! stubs and `<dyn IBinder>::as_remote()`/`as_proxy()` downcast against, so the wrapper is
//! transparent to consumers. The inner proxy is the only concrete `IBinder` anyone outside
//! this module needs to recognise.
//!
//! Its `inner` field is the RPC root `SIBinder` returned by `RpcSession::get_root`. Through its
//! `Arc<RpcProxy>` it is what keeps the proxy alive: the session-side cache in
//! `RpcSessionInner.state` holds only a `Weak` (it dedups without keeping proxies alive), so
//! dropping `inner` is what fires `RpcProxy::drop`.
//!
//! Its `_session` field is belt and braces. `RpcProxy` holds a *strong*
//! `Arc<RpcSessionInner>` (as AOSP `sp<RpcSession>`), so `inner` alone already keeps the
//! session — and its connection — alive until the proxy's `DEC_STRONG` has gone out; the
//! handle only makes that visible in the type. Field order is not load-bearing.
//!
//! All of this is `cfg(feature = "rpc")` — the `rpc`-OFF build maps
//! `Service::Accessor` to a log line and `None`
//! ([`super::servicemanager_16`] cfg guards the whole resolve path).
//!
//! Process-wide state: one switch, `rpc-tcp-debug` only (`set_accessor_plaintext_inet`, plan 17
//! R-23). There is no global registry, no `OnceLock<Mutex<Vec<RpcSession>>>`; the session
//! lives exactly as long as the wrapper the caller holds.

// cfg lives on the mod decl in super; repeating it trips clippy::duplicated_attributes.

use std::any::Any;
use std::sync::{Arc, Weak};

use crate::binder::{
    DeathRecipient, IBinder, Interface, SIBinder, Stability, Transactable, TransactionCode,
};
use crate::error::{Result, StatusCode};
use crate::parcel::Parcel;
use crate::rpc::RpcSession;
use crate::status::{BinderResult, Status};

// A separate AIDL output keeps `IServiceManager.aidl`'s transitive resolution unchanged.
include!(concat!(env!("OUT_DIR"), "/accessor_16.rs"));

pub use android::os::IAccessor::{
    BnAccessor, BpAccessor, IAccessor, IAccessorDefault, IAccessorDefaultRef,
    ERROR_CONNECTION_INFO_NOT_FOUND, ERROR_FAILED_TO_CONNECT_EACCES,
    ERROR_FAILED_TO_CONNECT_TO_SOCKET, ERROR_FAILED_TO_CREATE_SOCKET,
    ERROR_UNSUPPORTED_SOCKET_FAMILY,
};
// Codegen emits the async traits only with `async` (`rsbinder/build.rs` `set_async_support`).
#[cfg(feature = "async")]
pub use android::os::IAccessor::{IAccessorAsync, IAccessorAsyncService};

/// Transparent `IBinder` wrapper pinning the session to the root; see module doc "`AccessorRoot`".
pub(crate) struct AccessorRoot {
    /// The `get_root` binder; it keeps the proxy alive (the session cache holds only a `Weak`).
    inner: SIBinder,
    /// Redundant with `inner`'s strong session ref; it states the lifetime in the type.
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

    /// The inner `RpcProxy`, so `as_remote`/`as_proxy` and generated `from_binder` see its type.
    fn as_any(&self) -> &dyn Any {
        self.inner_binder().as_any()
    }

    fn as_transactable(&self) -> Option<&dyn Transactable> {
        self.inner_binder().as_transactable()
    }

    fn descriptor(&self) -> &str {
        self.inner_binder().descriptor()
    }

    fn try_descriptor(&self) -> Result<&str> {
        self.inner_binder().try_descriptor()
    }

    fn cached_descriptor(&self) -> Option<&str> {
        self.inner_binder().cached_descriptor()
    }

    fn is_remote(&self) -> bool {
        self.inner_binder().is_remote()
    }

    fn stability(&self) -> Stability {
        self.inner_binder().stability()
    }

    fn rpc_transact(
        &self,
        token: crate::binder::sealed::Token,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        self.inner_binder().rpc_transact(token, code, reader, reply)
    }
}

/// AOSP `BackendUnifiedServiceManager::toBinderService` for the
/// `Service::Accessor` arm (android-16). Resolve `accessor` (a kernel
/// remote `IAccessor`) into the RPC root binder of the service it
/// fronts:
///
/// 1. Cast to `BpAccessor` (kernel proxy — `addConnection`/
///    `getInstanceName` ride the kernel binder driver).
/// 2. `validateAccessor`: `getInstanceName() == name` — AOSP requires
///    this so a misbehaving / hijacked Accessor cannot impersonate a
///    different service name.
/// 3. `addConnection()` — server-side connected socket fd.
/// 4. `RpcSession::from_preconnected_fd(fd, max_version=2)` —
///    android-16 max wire version.
/// 5. `get_root()`, wrapped so the session stays alive until the caller
///    drops the returned binder (AOSP `setSessionSpecificRoot`).
///
/// # Errors
///
/// Each step fails with its own value. Steps 1 and 2 follow AOSP
/// `validateAccessor` (`IServiceManager.cpp`, android-16.0.0_r4); steps
/// 3–5 follow `toBinderService`, which returns the `addConnection`
/// status as is and `Status::fromStatusT` of a session failure.
///
/// - `accessor` is not an `IAccessor`: [`StatusCode::BadType`](crate::StatusCode::BadType).
/// - `getInstanceName` fails: that call's `Status`, unchanged.
/// - The accessor reports another instance name:
///   [`StatusCode::NameNotFound`](crate::StatusCode::NameNotFound). No
///   connection is requested from such an accessor.
/// - `addConnection` fails: that call's `Status`, unchanged — typically a
///   service-specific error carrying one of the `IAccessor::ERROR_*`
///   codes ([`accessor_error_name`] names it). AOSP turns
///   `ERROR_FAILED_TO_CONNECT_TO_SOCKET` into a null service so the
///   caller retries; here it is this `Err`, and the retry is the caller's.
/// - The connection is an `AF_INET`/`AF_INET6` socket:
///   [`StatusCode::BadType`](crate::StatusCode::BadType), unless
///   `rpc::set_accessor_plaintext_inet(true)` was called (feature
///   `rpc-tcp-debug`). Without that feature every inet socket is `BadType`,
///   as any family with no backend is.
/// - The RPC handshake, the root request, or the wrapper fails: the
///   [`StatusCode`](crate::StatusCode) of that failure as a `Status`.
///
/// Every failure is also logged at `warn` with the instance name.
///
/// # Plaintext inet
///
/// AOSP accepts any family here and runs it in plaintext
/// (`IServiceManager.cpp`, android-16.0.0_r4). rsbinder has a plaintext
/// inet transport only with `rpc-tcp-debug`, and Cargo turns that feature
/// on for the whole build as soon as any crate in the dependency graph asks
/// for it. So the feature alone does not let an accessor hand this process
/// an unencrypted, unauthenticated TCP connection: that also needs the
/// process-wide opt-in, off by default. A direct
/// [`RpcSession::from_preconnected_fd`] call is not affected: its caller
/// chose the socket.
pub fn try_resolve_accessor(name: &str, accessor: SIBinder) -> BinderResult<SIBinder> {
    let bp = validate_accessor(name, accessor)?;
    connect_accessor(name, &bp)
}

/// AOSP `validateAccessor`: steps 1–2 of [`try_resolve_accessor`], with its error values.
pub(crate) fn validate_accessor(
    name: &str,
    accessor: SIBinder,
) -> BinderResult<crate::Strong<dyn IAccessor>> {
    let bp = <dyn IAccessor as crate::FromIBinder>::try_from(accessor).map_err(|e| {
        log::warn!("IAccessor cast failed for {name}: {e}");
        Status::from(StatusCode::BadType)
    })?;
    match bp.getInstanceName() {
        Ok(reported) if reported == name => Ok(bp),
        Ok(other) => {
            log::warn!(
                "IAccessor instance-name mismatch for {name}: reported {other:?} — rejecting"
            );
            Err(StatusCode::NameNotFound.into())
        }
        Err(status) => {
            log::warn!(
                "IAccessor::getInstanceName failed for {name}: {status} \
                 (service_specific={})",
                accessor_error_name(status.service_specific_error())
            );
            Err(status)
        }
    }
}

/// Steps 3–5 of [`try_resolve_accessor`] on an accessor `validate_accessor` accepted.
pub(crate) fn connect_accessor(
    name: &str,
    bp: &crate::Strong<dyn IAccessor>,
) -> BinderResult<SIBinder> {
    let pfd = bp.addConnection().map_err(|status| {
        log::warn!(
            "IAccessor::addConnection failed for {name}: {status} \
             (service_specific={})",
            accessor_error_name(status.service_specific_error())
        );
        status
    })?;
    let fd: std::os::fd::OwnedFd = pfd.into();
    #[cfg(feature = "rpc-tcp-debug")]
    refuse_plaintext_inet(name, std::os::fd::AsFd::as_fd(&fd))?;

    // Versioned handshake capped at android-16's v2; an older peer may pick v0/v1.
    let session = RpcSession::from_preconnected_fd(fd, 2).map_err(|e| {
        log::warn!("RpcSession::from_preconnected_fd failed for {name}: {e} (code={e:?})");
        Status::from(e)
    })?;
    let root = session.get_root().map_err(|e| {
        log::warn!("RPC get_root failed for {name}: {e}");
        Status::from(e)
    })?;
    AccessorRoot::into_sibinder(root, session).map_err(|e| {
        log::warn!("AccessorRoot wrap failed for {name}: {e}");
        Status::from(e)
    })
}

#[cfg(feature = "rpc-tcp-debug")]
static ACCESSOR_PLAINTEXT_INET: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Let an `IAccessor` connection be a plaintext `AF_INET`/`AF_INET6`
/// socket, process-wide; returns the value it replaces. Off by default.
///
/// The switch is read by the accessor path only
/// ([`try_resolve_accessor`](crate::hub::android_16::try_resolve_accessor),
/// [`try_resolve_via_process_local`](crate::hub::android_16::try_resolve_via_process_local)
/// and a service manager lookup that resolves an accessor). There the
/// accessor, not this process, picks the socket, and an inet socket runs
/// the `rpc-tcp-debug` transport: no encryption, no peer identity
/// ([`PeerIdentity::Anonymous`](crate::rpc::PeerIdentity::Anonymous)).
/// With the switch off such a socket is refused with
/// [`StatusCode::BadType`](crate::StatusCode::BadType) and an `error` log
/// that names this function.
///
/// The `rpc-tcp-debug` feature is not enough by itself because Cargo
/// unifies features: one crate in the graph that enables it enables it for
/// every crate, release builds included. This switch is the decision of the
/// program that runs.
/// [`RpcSession::from_preconnected_fd`](crate::rpc::RpcSession::from_preconnected_fd)
/// called directly does not read it.
#[cfg(feature = "rpc-tcp-debug")]
pub fn set_accessor_plaintext_inet(allow: bool) -> bool {
    ACCESSOR_PLAINTEXT_INET.swap(allow, std::sync::atomic::Ordering::SeqCst)
}

/// Whether [`set_accessor_plaintext_inet`] last allowed plaintext inet
/// accessor connections (`false` until it is called).
#[cfg(feature = "rpc-tcp-debug")]
pub fn accessor_plaintext_inet() -> bool {
    ACCESSOR_PLAINTEXT_INET.load(std::sync::atomic::Ordering::SeqCst)
}

/// `BadType` for an inet accessor socket unless the opt-in is on; other families pass through.
#[cfg(feature = "rpc-tcp-debug")]
fn refuse_plaintext_inet(name: &str, fd: std::os::fd::BorrowedFd<'_>) -> BinderResult<()> {
    if accessor_plaintext_inet() {
        return Ok(());
    }
    // A failed `getsockname` is `from_preconnected_fd`'s to report.
    let Ok(local) = rustix::net::getsockname(fd) else {
        return Ok(());
    };
    let family = local.address_family();
    if family == rustix::net::AddressFamily::INET || family == rustix::net::AddressFamily::INET6 {
        log::error!(
            "IAccessor for {name} returned a plaintext inet socket; refusing it. Call \
             rsbinder::rpc::set_accessor_plaintext_inet(true) to accept one (feature rpc-tcp-debug)"
        );
        return Err(StatusCode::BadType.into());
    }
    Ok(())
}

/// The `ServiceWithMetadata` an accessor-resolved root is returned in: never lazy.
pub(crate) fn accessor_service_with_metadata(
    root: SIBinder,
) -> super::servicemanager_16::android::os::ServiceWithMetadata::ServiceWithMetadata {
    super::servicemanager_16::android::os::ServiceWithMetadata::ServiceWithMetadata {
        r#service: Some(root),
        // AOSP `setSessionSpecificRoot` keeps an RPC root for the session's lifetime.
        r#isLazyService: false,
    }
}

/// [`try_resolve_accessor`] with every error turned into `None` (each is
/// still logged). On success `service` is always `Some` and
/// `isLazyService` is `false`.
#[deprecated(
    since = "0.13.0",
    note = "use try_resolve_accessor, which says why a resolve failed"
)]
pub fn resolve_accessor(
    name: &str,
    accessor: SIBinder,
) -> Option<super::servicemanager_16::android::os::ServiceWithMetadata::ServiceWithMetadata> {
    try_resolve_accessor(name, accessor)
        .ok()
        .map(accessor_service_with_metadata)
}

/// Map an `IAccessor::ERROR_*` service-specific code
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

/// Fuzz hook: feed an arbitrary little-endian i32
/// byte payload into [`accessor_error_name`] and assert it never
/// panics, allocates indefinitely, or returns a non-`'static str`. The
/// returned string is intentionally consumed via `std::hint::black_box`
/// so the optimiser cannot DCE the lookup on builds that inline it.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub fn __fuzz_accessor_error_decode(input: &[u8]) {
    // Pad/truncate to 4 bytes (missing bytes read as zero): the AIDL wire's `i32` range.
    let mut buf = [0u8; 4];
    let n = input.len().min(4);
    buf[..n].copy_from_slice(&input[..n]);
    let code = i32::from_le_bytes(buf);
    let _ = std::hint::black_box(accessor_error_name(code));
}

#[cfg(test)]
mod tests {
    //! Deterministic regression for the `ServiceSpecificError` decode —
    //! the enforceable adversarial-input gate; the libFuzzer target is
    //! the soak supplement.

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
        // Out-of-range values return the fallback string and never panic.
        for code in [5, 6, 1_000, -1, -1_000, i32::MAX, i32::MIN] {
            assert_eq!(accessor_error_name(code), "unknown", "code {code}");
        }
    }

    #[test]
    fn fuzz_accessor_error_decode_never_panics() {
        // Boundary input lengths plus the signed-i32 cardinal points, as a fuzz corpus would.
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
