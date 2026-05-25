// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Register side of the RPC Accessor bridge — the AOSP `createAccessor` /
//! `addAccessorProvider` analogue in pure Rust. Lets a process **vend**
//! an `IAccessor` binder without going through the system service
//! manager: a [`LocalAccessor`] implements `BnAccessor`, a
//! provider closure maps an instance name to an [`AccessorSockAddr`],
//! and `addConnection()` opens a connected socket whose `OwnedFd` rides
//! back over the kernel binder reply. The consume side
//! ([`accessor_16`](super::accessor_16)) then performs the v2/v1
//! handshake against that fd.
//!
//! Cross-platform: the [`Unix`](AccessorSockAddr::Unix) variant is
//! always present so macOS host hermetic tests can exercise the
//! register-side path without `rpc-vsock` / `rpc-tcp-debug` features.
//! `Vsock`/`Inet` variants are present in the type at all times for ABI
//! stability; whether they can be *connected* is a runtime check
//! inside the [`AccessorSockAddr::connect_owned_fd`] family of helpers
//! — feature-disabled variants map to `ERROR_UNSUPPORTED_SOCKET_FAMILY`
//! so an instance configured for a backend the binary wasn't built with
//! surfaces as the same AOSP-faithful service-specific error a
//! wrong-family `addConnection` returns.
//!
//! ## Implementation history
//!
//! Originally landed under plan 2-14 (phases A0.1 / A0.2 / A0.3 / A.4 /
//! A.5) with the D.9 STAGE3 real-libbinder gate as the non-negotiable
//! validation against android-16 emulator. Plan references are
//! preserved in commit messages and the long-form plan doc; user-facing
//! docstrings here describe the AOSP analogue, not the plan tag.

// cfg lives on the mod decl in super — duplicating here trips
// clippy::duplicated_attributes.

use std::io;
use std::net::SocketAddrV4;
use std::os::fd::OwnedFd;
use std::path::PathBuf;

use crate::error::Result;

/// The socket address a [`LocalAccessor`] hands back from its
/// `addConnection()` callback. AOSP `RpcSocketAddressProvider` returns
/// `sockaddr_storage` populated with one of `sockaddr_un` /
/// `sockaddr_vm` / `sockaddr_in`; this enum is the strongly-typed
/// equivalent.
///
/// All three variants exist regardless of build features so the enum's
/// shape stays stable across feature flags (matching exhaustively in
/// downstream `match` is therefore predictable). The
/// `connect_*_owned_fd` helpers (A0.2) gate the *connect path* on the
/// matching `rpc-vsock`/`rpc-tcp-debug` feature and return
/// `ERROR_UNSUPPORTED_SOCKET_FAMILY` otherwise.
///
/// `Debug`/`Clone` are provided so a provider closure can build the
/// address ahead of time and the bridge can log it on `addConnection`
/// failures without consuming it.
///
/// [`LocalAccessor`]: super::accessor_register::LocalAccessor
#[derive(Clone, Debug)]
pub enum AccessorSockAddr {
    /// AF_UNIX path. Cross-platform — works on Linux + macOS host.
    Unix(PathBuf),
    /// AF_VSOCK `(cid, port)`. Only usable when the binary was built
    /// with the `rpc-vsock` feature; otherwise the connect helper
    /// returns `ERROR_UNSUPPORTED_SOCKET_FAMILY`.
    Vsock { cid: u32, port: u32 },
    /// AF_INET v4 address. Only usable when the binary was built with
    /// the `rpc-tcp-debug` feature (debug builds only — see
    /// `rpc::transport::tcp_debug`); otherwise the connect
    /// helper returns `ERROR_UNSUPPORTED_SOCKET_FAMILY`.
    Inet(SocketAddrV4),
}

impl AccessorSockAddr {
    /// AOSP-faithful family hint, useful for `accessor_error_name`-style
    /// logging from a provider closure that wants to report which
    /// family it picked before any connect attempt.
    pub fn family_str(&self) -> &'static str {
        match self {
            AccessorSockAddr::Unix(_) => "AF_UNIX",
            AccessorSockAddr::Vsock { .. } => "AF_VSOCK",
            AccessorSockAddr::Inet(_) => "AF_INET",
        }
    }

    /// Subplan 2-14 A0.2: open a *server-process* client connection to
    /// the address this `AccessorSockAddr` describes and return the
    /// raw `OwnedFd`. This is what AOSP `singleSocketConnection` does
    /// inside `LocalAccessor::addConnection` — a plain `connect(2)`
    /// from the process hosting the `BnAccessor`, whose resulting
    /// fd is then transferred to the remote `BpAccessor` client via
    /// `ParcelFileDescriptor` (kernel-level fd duplication).
    ///
    /// Errors map onto the AOSP `IAccessor::ERROR_*` codes; the A0.3
    /// `LocalAccessor::addConnection` impl translates these into
    /// `Status::from_service_specific_error(ERROR_*)`. The mapping is:
    ///
    /// | error | AOSP code |
    /// |---|---|
    /// | `UnsupportedFamily` | `ERROR_UNSUPPORTED_SOCKET_FAMILY` |
    /// | `ConnectFailedEacces` | `ERROR_FAILED_TO_CONNECT_EACCES` |
    /// | `ConnectFailed(io)` (other) | `ERROR_FAILED_TO_CONNECT_TO_SOCKET` |
    /// | `CreateSocketFailed(io)` | `ERROR_FAILED_TO_CREATE_SOCKET` |
    ///
    /// Feature-disabled families (`Vsock` without `rpc-vsock`, `Inet`
    /// without `rpc-tcp-debug`) return `UnsupportedFamily` — the same
    /// error a wrong-family addr would get at the AOSP boundary, so an
    /// instance configured for a backend the binary wasn't built with
    /// surfaces with AOSP-faithful semantics rather than a compile-
    /// time mismatch.
    pub fn connect_owned_fd(&self) -> std::result::Result<OwnedFd, AccessorConnectError> {
        match self {
            AccessorSockAddr::Unix(path) => connect_unix_owned_fd(path),
            AccessorSockAddr::Vsock { cid, port } => connect_vsock_owned_fd(*cid, *port),
            AccessorSockAddr::Inet(addr) => connect_inet_owned_fd(*addr),
        }
    }
}

/// AOSP `IAccessor::ERROR_*`-aligned classification of a failure
/// returned by [`AccessorSockAddr::connect_owned_fd`]. The A0.3
/// `LocalAccessor::addConnection` impl converts each variant into the
/// corresponding AIDL `Status::from_service_specific_error(...)`.
///
/// `Display` is implemented for log lines; the inner `io::Error` is
/// preserved so callers can surface the OS-level cause alongside the
/// AOSP code in their `log::warn!` line (matching the pattern used by
/// [`super::accessor_16::resolve_accessor`]).
#[derive(Debug)]
pub enum AccessorConnectError {
    /// AOSP `ERROR_UNSUPPORTED_SOCKET_FAMILY` — the address family
    /// either isn't valid (none of `AF_UNIX/AF_VSOCK/AF_INET`) or the
    /// crate wasn't built with the matching backend feature.
    UnsupportedFamily,
    /// AOSP `ERROR_FAILED_TO_CREATE_SOCKET` — `socket(2)` failed
    /// before `connect(2)` was even attempted.
    CreateSocketFailed(io::Error),
    /// AOSP `ERROR_FAILED_TO_CONNECT_EACCES` — `connect(2)` was
    /// rejected with `EACCES` (peer rejected the principal — caller
    /// likely lacks permission to open this RPC endpoint).
    ConnectFailedEacces,
    /// AOSP `ERROR_FAILED_TO_CONNECT_TO_SOCKET` — `connect(2)` failed
    /// for any reason other than `EACCES` (peer not listening,
    /// network unreachable, …).
    ConnectFailed(io::Error),
}

impl std::fmt::Display for AccessorConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessorConnectError::UnsupportedFamily => {
                write!(f, "unsupported socket family for IAccessor::addConnection")
            }
            AccessorConnectError::CreateSocketFailed(e) => {
                write!(f, "socket(2) failed: {e}")
            }
            AccessorConnectError::ConnectFailedEacces => {
                write!(f, "connect(2) refused with EACCES")
            }
            AccessorConnectError::ConnectFailed(e) => {
                write!(f, "connect(2) failed: {e}")
            }
        }
    }
}

impl std::error::Error for AccessorConnectError {}

/// AF_UNIX connect helper. Cross-platform (Linux + macOS) — `std`'s
/// `UnixStream::connect(path)` is stable on both. The resulting
/// stream's `OwnedFd` is the *client-side endpoint of a connected
/// socket pair* whose server-side endpoint is whoever accepted on
/// `path` (typically the same process's `RpcServer::setup_unix_server`
/// listener — see plan 2-14 §3 A0(ii)).
fn connect_unix_owned_fd(path: &PathBuf) -> std::result::Result<OwnedFd, AccessorConnectError> {
    use std::os::unix::net::UnixStream;
    match UnixStream::connect(path) {
        Ok(stream) => Ok(OwnedFd::from(stream)),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(AccessorConnectError::ConnectFailedEacces)
        }
        // `socket(2)` and `connect(2)` failures both surface as
        // `io::Error` from `UnixStream::connect` — AOSP's
        // `singleSocketConnection` distinguishes them with a separate
        // `::socket()` call. Without retracing that we report all
        // pre-EACCES failures as `ConnectFailed` (the more common
        // case); `CreateSocketFailed` is reserved for backends where
        // `socket(2)` is a distinct syscall the helper can inspect.
        Err(e) => Err(AccessorConnectError::ConnectFailed(e)),
    }
}

/// AF_VSOCK connect helper. Behind `rpc-vsock`; when the feature is
/// off, returns `UnsupportedFamily` so an instance configured for
/// vsock surfaces with AOSP-faithful semantics on a `rpc`-only build.
#[cfg(feature = "rpc-vsock")]
fn connect_vsock_owned_fd(
    cid: u32,
    port: u32,
) -> std::result::Result<OwnedFd, AccessorConnectError> {
    use std::os::fd::IntoRawFd;
    use vsock::{VsockAddr, VsockStream};
    match VsockStream::connect(&VsockAddr::new(cid, port)) {
        Ok(stream) => {
            // SAFETY: `into_raw_fd` transfers ownership; wrapping in
            // `OwnedFd` immediately re-establishes RAII. AOSP-sanctioned
            // pattern, identical to `VsockTransport::from_owned_fd` in
            // [`crate::rpc::transport::vsock`].
            let raw = stream.into_raw_fd();
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(AccessorConnectError::ConnectFailedEacces)
        }
        Err(e) => Err(AccessorConnectError::ConnectFailed(e)),
    }
}

#[cfg(not(feature = "rpc-vsock"))]
fn connect_vsock_owned_fd(
    _cid: u32,
    _port: u32,
) -> std::result::Result<OwnedFd, AccessorConnectError> {
    Err(AccessorConnectError::UnsupportedFamily)
}

/// AF_INET v4 connect helper. Behind `rpc-tcp-debug` (debug-only
/// transport); when off, returns `UnsupportedFamily`.
#[cfg(feature = "rpc-tcp-debug")]
fn connect_inet_owned_fd(addr: SocketAddrV4) -> std::result::Result<OwnedFd, AccessorConnectError> {
    use std::net::TcpStream;
    match TcpStream::connect(addr) {
        Ok(stream) => Ok(OwnedFd::from(stream)),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(AccessorConnectError::ConnectFailedEacces)
        }
        Err(e) => Err(AccessorConnectError::ConnectFailed(e)),
    }
}

#[cfg(not(feature = "rpc-tcp-debug"))]
fn connect_inet_owned_fd(
    _addr: SocketAddrV4,
) -> std::result::Result<OwnedFd, AccessorConnectError> {
    Err(AccessorConnectError::UnsupportedFamily)
}

#[cfg(feature = "rpc-vsock")]
use std::os::fd::FromRawFd;

// --- Subplan 2-14 A0.3: `LocalAccessor` (`BnAccessor` impl) -------

use super::accessor_16::{
    BnAccessor, IAccessor, ERROR_CONNECTION_INFO_NOT_FOUND, ERROR_FAILED_TO_CONNECT_EACCES,
    ERROR_FAILED_TO_CONNECT_TO_SOCKET, ERROR_FAILED_TO_CREATE_SOCKET,
    ERROR_UNSUPPORTED_SOCKET_FAMILY,
};
use crate::binder::{Interface, Strong};
use crate::file_descriptor::ParcelFileDescriptor;
use crate::status::Status;

/// Subplan 2-14 A0.3: the AOSP `LocalAccessor` analog — a server-side
/// `BnAccessor` whose `addConnection()` calls a user-supplied
/// [`AccessorAddrProvider`] for an [`AccessorSockAddr`], opens a
/// `connect(2)` to it inside *this* process, and ships the resulting
/// `OwnedFd` to the remote `BpAccessor` client as a
/// `ParcelFileDescriptor`. The kernel binder driver duplicates the fd
/// into the client's fd table on Parcel-marshal — both sides end up
/// holding the two ends of one socket pair (the *client* end of the
/// server-process `connect`, plus the matching *accepted* end the
/// `RpcServer::setup_unix_server` loop picked up at the listening
/// path).
///
/// AOSP parity:
///
///  * `singleSocketConnection` ⇒
///    [`AccessorSockAddr::connect_owned_fd`] (A0.2)
///  * `RpcSocketAddressProvider` ⇒ [`AccessorAddrProvider`] (A0.1)
///  * `Status::fromServiceSpecificError(ERROR_*, msg)` ⇒
///    `Status::new_service_specific_error_str` with the matching
///    `IAccessor::ERROR_*` constant
///
/// Use via [`Self::new_binder`] (the `BnAccessor::new_binder` AIDL
/// stub) — the returned `SIBinder` is what
/// [`create_accessor`](crate::hub::android_16::create_accessor) wraps
/// for registration (A.4) and what `kernel addService(name, _)`
/// publishes to the system service manager (D.9 STAGE3 launcher).
pub struct LocalAccessor {
    instance: String,
    addr_provider: AccessorAddrProvider,
}

impl LocalAccessor {
    /// Build a new `LocalAccessor` bound to `instance` (the name the
    /// `getInstanceName()` RPC must report — AOSP `validateAccessor`
    /// rejects mismatch) and `addr_provider` (resolves the name to the
    /// socket address `addConnection()` should connect to).
    ///
    /// Returned wrapped as a `Strong<dyn IAccessor>` so callers can
    /// trivially `as_binder()` for `addService` or stash on a server's
    /// service directory. The `Strong<dyn IAccessor>` is also what a
    /// future [`add_accessor_provider`] (A.4) returns from its
    /// closure.
    pub fn new_binder(
        instance: impl Into<String>,
        addr_provider: AccessorAddrProvider,
    ) -> Strong<dyn IAccessor> {
        BnAccessor::new_binder(LocalAccessor {
            instance: instance.into(),
            addr_provider,
        })
    }
}

impl Interface for LocalAccessor {}

impl IAccessor for LocalAccessor {
    fn addConnection(&self) -> crate::status::Result<ParcelFileDescriptor> {
        // Step 1: ask the user-supplied closure for a socket address.
        // A `Result::Err` here is AOSP `ERROR_CONNECTION_INFO_NOT_FOUND`
        // (the provider could not derive an address for this name —
        // `singleSocketConnection` was never even attempted).
        let addr = match (self.addr_provider)(&self.instance) {
            Ok(a) => a,
            Err(e) => {
                let msg = format!("AccessorAddrProvider({:?}) failed: {e}", self.instance);
                log::warn!("{msg}");
                return Err(Status::new_service_specific_error(
                    ERROR_CONNECTION_INFO_NOT_FOUND,
                    Some(msg),
                ));
            }
        };

        // Step 2: open the server-process side of the socket pair (an
        // ordinary `connect(2)` inside *this* process; the peer is the
        // `RpcServer::setup_unix_server` listener at the same path,
        // not the remote client) and return the fd.
        match addr.connect_owned_fd() {
            Ok(fd) => Ok(ParcelFileDescriptor::new(fd)),
            Err(err) => {
                let (code, label) = accessor_error_code_for(&err);
                let msg = format!(
                    "LocalAccessor({:?}) {} ({}): {err}",
                    self.instance,
                    addr.family_str(),
                    label,
                );
                log::warn!("{msg}");
                Err(Status::new_service_specific_error(code, Some(msg)))
            }
        }
    }

    fn getInstanceName(&self) -> crate::status::Result<String> {
        Ok(self.instance.clone())
    }
}

/// Map an [`AccessorConnectError`] onto the AOSP `IAccessor::ERROR_*`
/// service-specific code + a symbolic label for logging.
fn accessor_error_code_for(err: &AccessorConnectError) -> (i32, &'static str) {
    match err {
        AccessorConnectError::UnsupportedFamily => (
            ERROR_UNSUPPORTED_SOCKET_FAMILY,
            "ERROR_UNSUPPORTED_SOCKET_FAMILY",
        ),
        AccessorConnectError::CreateSocketFailed(_) => (
            ERROR_FAILED_TO_CREATE_SOCKET,
            "ERROR_FAILED_TO_CREATE_SOCKET",
        ),
        AccessorConnectError::ConnectFailedEacces => (
            ERROR_FAILED_TO_CONNECT_EACCES,
            "ERROR_FAILED_TO_CONNECT_EACCES",
        ),
        AccessorConnectError::ConnectFailed(_) => (
            ERROR_FAILED_TO_CONNECT_TO_SOCKET,
            "ERROR_FAILED_TO_CONNECT_TO_SOCKET",
        ),
    }
}

/// Closure type returned by [`add_accessor_provider`] (A.4) and called
/// by [`LocalAccessor::addConnection`] (A0.3) to resolve an instance
/// name to the socket address it should connect to. Mirrors AOSP
/// `RpcSocketAddressProvider`'s `(name, sockaddr*, size_t) -> status_t`
/// signature but returns the strongly-typed [`AccessorSockAddr`]
/// instead of an out-pointer + status int.
///
/// A `Result::Err` here surfaces as
/// `IAccessor::ERROR_CONNECTION_INFO_NOT_FOUND` at the AIDL boundary
/// (AOSP parity — see [`super::accessor_16::accessor_error_name`]).
/// `Send + Sync` so the same provider can be reached from any thread
/// servicing the AIDL `BnAccessor::addConnection` call (the kernel
/// binder driver may dispatch on a different thread than the one that
/// registered the provider).
///
/// [`add_accessor_provider`]: super::accessor_register::add_accessor_provider
/// [`LocalAccessor::addConnection`]: super::accessor_register::LocalAccessor
pub type AccessorAddrProvider = Box<dyn Fn(&str) -> Result<AccessorSockAddr> + Send + Sync>;

// --- Subplan 2-14 A.4: process-local AccessorProvider registry ----
//
// AOSP `gAccessorProviders` (`IServiceManager.cpp:200-201`) is a
// process-local list of `AccessorProvider{ instances, providerCallback }`.
// `addAccessorProvider` checks for duplicate instances and refuses,
// then appends; `removeAccessorProvider` walks the list and drops the
// matching shared_ptr. The cross-process pickup is via
// `getInjectedAccessor` — `BackendUnifiedServiceManager::getService2`
// calls it as a *fallback* when servicemanager returns no binder for
// `name`. Phase A.5 wires that fallback; this commit lands the
// registry primitive itself.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock, Weak};

/// Closure type for [`add_accessor_provider`]. Maps an instance name
/// to an `IAccessor` `SIBinder` (typically built via
/// [`create_accessor`]) — `None` ⇒ "this provider doesn't know about
/// `name`, try the next provider in the registry".
///
/// `Send + Sync` so the registry can be walked under a mutex by any
/// thread issuing a `hub::get_service` lookup; the closure should be
/// idempotent across threads (Phase A.5 dispatches without re-locking
/// the registry — see `IServiceManager.cpp:286-291` snapshot pattern).
pub type AccessorProviderFn = Box<dyn Fn(&str) -> Option<crate::binder::SIBinder> + Send + Sync>;

/// One entry in the process-local accessor registry. Owns its set of
/// instance names (so `lookup_accessor_provider` can match without
/// taking shared ownership of the closure) and a reference-counted
/// callback (so a [`AccessorProviderHandle`] can identify "its" entry
/// on drop). `Arc` not `Box` so the handle's `Weak` upgrade is the
/// canonical liveness test.
struct AccessorProviderEntry {
    instances: HashSet<String>,
    provider: Arc<AccessorProviderFn>,
}

/// Process-wide accessor provider registry. AOSP `gAccessorProviders`
/// byte-equivalent — a single global, locked with a `Mutex`, walked on
/// `add`/`remove`/lookup.
///
/// Initialized lazily via `OnceLock` so the `rpc`-OFF /
/// non-android-16 build never allocates the mutex. The
/// `Mutex<Vec<_>>` shape is the AOSP-faithful one — readers (Phase
/// A.5 lookup) snapshot the entries inside the lock, then release
/// before calling the provider closure (no callback under the lock
/// — see `IServiceManager.cpp:286-291`).
static REGISTRY: OnceLock<std::sync::Mutex<Vec<AccessorProviderEntry>>> = OnceLock::new();

fn registry() -> &'static std::sync::Mutex<Vec<AccessorProviderEntry>> {
    REGISTRY.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// AOSP `IServiceManager.cpp:359-366` `isInstanceProvidedLocked`. The
/// caller already holds the registry lock.
fn is_instance_provided_locked(entries: &[AccessorProviderEntry], instance: &str) -> bool {
    entries.iter().any(|e| e.instances.contains(instance))
}

/// Subplan 2-14 A.4: register a process-local accessor provider for
/// `instances`. AOSP `addAccessorProvider`
/// (`IServiceManager.cpp:368-389`). Returns a
/// [`AccessorProviderHandle`] whose `Drop` un-registers the provider
/// (RAII) — drop the handle to remove the provider, or call
/// [`remove_accessor_provider`] for explicit removal with an error
/// return.
///
/// **Duplicate-instance reject**: if *any* name in `instances` is
/// already provided for by a live entry, the call returns
/// `Err(StatusCode::AlreadyExists)` and registers nothing (AOSP
/// returns an empty `weak_ptr<AccessorProvider>()`; rsbinder surfaces
/// the error directly so a caller can distinguish "no instances" from
/// "duplicate" without inspecting a weak pointer).
///
/// **Empty-set reject**: `instances.is_empty()` ⇒
/// `Err(StatusCode::BadValue)` (AOSP `ALOGE`+empty-weak).
///
/// The provider closure must be `Send + Sync` — it is invoked from
/// whatever thread services a `hub::get_service` fallback (Phase
/// A.5).
pub fn add_accessor_provider(
    instances: HashSet<String>,
    provider: AccessorProviderFn,
) -> std::result::Result<AccessorProviderHandle, crate::error::StatusCode> {
    if instances.is_empty() {
        log::warn!("add_accessor_provider: empty instance set rejected");
        return Err(crate::error::StatusCode::BadValue);
    }
    let mut reg = registry().lock().expect("accessor registry poisoned");
    for instance in &instances {
        if is_instance_provided_locked(&reg, instance) {
            log::warn!("add_accessor_provider: instance {instance:?} already provided — rejecting");
            return Err(crate::error::StatusCode::AlreadyExists);
        }
    }
    let arc_provider = Arc::new(provider);
    let handle = AccessorProviderHandle {
        provider: Arc::downgrade(&arc_provider),
    };
    reg.push(AccessorProviderEntry {
        instances,
        provider: arc_provider,
    });
    Ok(handle)
}

/// Subplan 2-14 A.4: explicit removal of a provider previously
/// registered via [`add_accessor_provider`]. AOSP
/// `removeAccessorProvider` (`IServiceManager.cpp:391-411`). Returns
/// `Err(StatusCode::BadValue)` if the handle is already dead (the
/// Arc has been dropped — typically because
/// [`AccessorProviderHandle`] already ran its Drop), and
/// `Err(StatusCode::NameNotFound)` if no entry with this provider
/// exists in the registry (shouldn't happen in practice — the handle
/// holds a `Weak` and the entry holds the `Arc`, so a live handle
/// implies a live entry).
///
/// **Idiomatic use**: prefer simply dropping the
/// [`AccessorProviderHandle`] — the Drop impl calls the same removal
/// logic and ignores the result. Direct calls to this function are
/// for callers who want to surface removal failures (rare).
pub fn remove_accessor_provider(
    handle: AccessorProviderHandle,
) -> std::result::Result<(), crate::error::StatusCode> {
    let Some(target) = handle.provider.upgrade() else {
        return Err(crate::error::StatusCode::BadValue);
    };
    let mut reg = registry().lock().expect("accessor registry poisoned");
    let before = reg.len();
    reg.retain(|e| !Arc::ptr_eq(&e.provider, &target));
    if reg.len() == before {
        return Err(crate::error::StatusCode::NameNotFound);
    }
    Ok(())
}

/// Subplan 2-14 A.4: RAII handle returned by
/// [`add_accessor_provider`]. Dropping the handle un-registers the
/// provider; ignore the handle (`_handle`) only if you want the
/// provider to live for the whole process lifetime.
///
/// The handle carries a `Weak` reference to the provider entry's
/// `Arc<AccessorProviderFn>` so its lifetime tracks the registry
/// entry — a successful `add_accessor_provider` returns a live
/// handle, `Drop` removes it, and a subsequent
/// [`remove_accessor_provider`] surfaces `BadValue` for the
/// already-removed case.
///
/// `Debug` is implemented so callers can `.expect()`/`.expect_err()`
/// against `Result<AccessorProviderHandle, _>`; the body deliberately
/// omits the inner `Weak` pointer value (only liveness is meaningful).
pub struct AccessorProviderHandle {
    provider: Weak<AccessorProviderFn>,
}

impl std::fmt::Debug for AccessorProviderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessorProviderHandle")
            .field("live", &(self.provider.strong_count() > 0))
            .finish()
    }
}

impl AccessorProviderHandle {
    /// Liveness probe — `true` iff the underlying provider entry is
    /// still in the registry (the `Arc` hasn't been dropped via
    /// [`remove_accessor_provider`] or by the handle's own Drop).
    /// Used by tests; not part of the public API contract.
    #[cfg(test)]
    fn is_live(&self) -> bool {
        self.provider.strong_count() > 0
    }
}

impl Drop for AccessorProviderHandle {
    fn drop(&mut self) {
        // RAII unregister. Race with explicit
        // `remove_accessor_provider` is fine: the second remove sees a
        // stale `Weak` and is a no-op (the entry has already been
        // dropped, and the `Arc` count is 0). Never panic in Drop.
        let Some(target) = self.provider.upgrade() else {
            return;
        };
        if let Ok(mut reg) = registry().lock() {
            reg.retain(|e| !Arc::ptr_eq(&e.provider, &target));
        }
    }
}

/// Subplan 2-14 A.4: lookup a provider by instance name. AOSP
/// `getInjectedAccessor` (`IServiceManager.cpp:284-312`) — snapshot
/// the registry under the lock, then walk the snapshot *outside* the
/// lock calling each provider's closure until one returns
/// `Some(binder)`. `None` ⇒ no live provider knows about `name`
/// (fallback caller should return the original
/// `Service::Accessor(None)` / `ServiceWithMetadata(None)` they were
/// trying to backfill).
///
/// Phase A.5 will call this from `hub::get_service`'s servicemanager
/// fallback arm; the function is `pub(crate)` here so the only
/// consumers are inside the crate (Phase A.5 + hermetic A.5 tests).
#[allow(dead_code)] // wired up in Phase A.5 (`hub::get_service` fallback)
pub(crate) fn lookup_accessor_provider(name: &str) -> Option<crate::binder::SIBinder> {
    // Snapshot of `Arc<AccessorProviderFn>` for entries that include
    // `name`. We deliberately keep `Arc` instead of `Weak` here so the
    // closure can't disappear between snapshot and call — AOSP keeps
    // `shared_ptr` copies for the same reason
    // (`IServiceManager.cpp:289-290`).
    let snapshot: Vec<Arc<AccessorProviderFn>> = {
        let reg = registry().lock().expect("accessor registry poisoned");
        reg.iter()
            .filter(|e| e.instances.contains(name))
            .map(|e| Arc::clone(&e.provider))
            .collect()
    };
    // Call closures *outside* the lock (AOSP comment: "Unlocked to
    // call the providers. This requires the providers to be
    // threadsafe and not contain any references to objects that could
    // be deleted.").
    for provider in snapshot {
        if let Some(binder) = provider(name) {
            return Some(binder);
        }
    }
    None
}

/// Subplan 2-14 A.5: process-local "consume" — look up a registered
/// `IAccessor` provider for `name` (via the A.4 registry) and bridge
/// it to an `RpcSession` root using 2-13's
/// [`super::accessor_16::resolve_accessor`]. Returns `None` if no
/// provider claims `name`, or if the bridge fails
/// (instance-name mismatch / addConnection failure / handshake / no
/// root). This is the AOSP `getInjectedAccessor` →
/// `Service::accessor → toBinder` *combined* path, exposed as a
/// single public helper so a caller without a kernel servicemanager
/// (hermetic tests on macOS, no-binderfs hosts) can drive the
/// fallback directly.
///
/// `cfg(feature = "rpc")` — without `rpc`, no `IAccessor` stub exists
/// so the function isn't compiled.
pub fn resolve_via_process_local(
    name: &str,
) -> Option<super::servicemanager_16::android::os::ServiceWithMetadata::ServiceWithMetadata> {
    let accessor = lookup_accessor_provider(name)?;
    super::accessor_16::resolve_accessor(name, accessor)
}

/// Subplan 2-14 A.4: AOSP `createAccessor`
/// (`IServiceManager.cpp:439-450`). Wrap an [`AccessorAddrProvider`]
/// in a [`LocalAccessor`] `BnAccessor`, return the binder.
/// Convenience over [`LocalAccessor::new_binder`] for the "I just
/// want an `SIBinder`" case (most callers want to register the binder
/// with `addService` or stash it in an [`add_accessor_provider`]
/// closure — both want an `SIBinder`, not a `Strong<dyn IAccessor>`).
pub fn create_accessor(
    instance: impl Into<String>,
    addr_provider: AccessorAddrProvider,
) -> crate::binder::SIBinder {
    LocalAccessor::new_binder(instance, addr_provider).as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// Family-string dispatch is order-independent across variants and
    /// stable across feature flags (`Vsock`/`Inet` variants exist
    /// unconditionally — A0.1 design note in the module doc).
    #[test]
    fn family_str_covers_all_variants() {
        let unix = AccessorSockAddr::Unix(PathBuf::from("/tmp/rsbinder-test-2-14.sock"));
        let vsock = AccessorSockAddr::Vsock { cid: 2, port: 5555 };
        let inet = AccessorSockAddr::Inet("127.0.0.1:5555".parse().expect("addr"));
        assert_eq!(unix.family_str(), "AF_UNIX");
        assert_eq!(vsock.family_str(), "AF_VSOCK");
        assert_eq!(inet.family_str(), "AF_INET");
    }

    /// `Clone`/`Debug` derives compile and round-trip the inner data
    /// (defensive — a future `Clone` regression would otherwise leak
    /// fd-bearing types into the variant).
    #[test]
    fn clone_and_debug_roundtrip() {
        let a = AccessorSockAddr::Unix(PathBuf::from("/tmp/x"));
        let b = a.clone();
        let dbg = format!("{b:?}");
        assert!(dbg.contains("Unix"));
        assert!(dbg.contains("/tmp/x"));
    }

    /// The provider closure type accepts the obvious `|name| Ok(...)`
    /// shape; this is a compile-time witness so a future refactor that
    /// breaks `Fn` boxing (e.g. accidentally requiring `FnMut`) fails
    /// here rather than at the call site.
    #[test]
    fn provider_closure_compiles() {
        let path = PathBuf::from("/tmp/rsbinder-test-provider.sock");
        let provider: AccessorAddrProvider =
            Box::new(move |_name: &str| Ok(AccessorSockAddr::Unix(path.clone())));
        let got = provider("instance-x").expect("provider ok");
        match got {
            AccessorSockAddr::Unix(p) => {
                assert_eq!(p, PathBuf::from("/tmp/rsbinder-test-provider.sock"))
            }
            other => panic!("expected Unix, got {other:?}"),
        }
    }

    /// Helper: bind+listen a unique-path Unix socket and return
    /// `(listener, path)`. The path is removed before bind to make the
    /// test idempotent under crashed previous runs. macOS' default
    /// `std::env::temp_dir()` is `/var/folders/.../T/` (~47 chars), so
    /// we keep the file name short (`rsb_${tag}_${pid}.sock` —
    /// `tag` capped to ≤ 8 chars at call sites) to stay well under
    /// `SUN_LEN = 104`.
    fn unique_unix_listener(tag: &str) -> (UnixListener, PathBuf) {
        let mut p = std::env::temp_dir();
        p.push(format!("rsb_{}_{}.sock", tag, std::process::id()));
        let _ = std::fs::remove_file(&p);
        let l = UnixListener::bind(&p).expect("bind");
        (l, p)
    }

    /// A0.2 unix path — `connect_owned_fd` returns a real, connected
    /// `OwnedFd` whose other endpoint is the listener's `accept`. The
    /// fd is byte-functional (echo round-trip) — proves the fd isn't
    /// dropped/closed somewhere in the wrapping.
    #[test]
    fn connect_owned_fd_unix_roundtrips() {
        use std::io::{Read, Write};
        let (listener, path) = unique_unix_listener("a02-rt");
        // Server accept thread — copies one byte the client writes.
        let server_handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1];
            s.read_exact(&mut buf).expect("read");
            s.write_all(&buf).expect("write");
        });

        let addr = AccessorSockAddr::Unix(path.clone());
        let fd = addr.connect_owned_fd().expect("connect");
        // Drive bytes through the adopted fd. `UnixStream::from(fd)`
        // takes ownership of the fd, so the fd lives exactly as long as
        // the stream.
        let mut stream = UnixStream::from(fd);
        stream.write_all(&[0x42]).expect("client write");
        let mut got = [0u8; 1];
        stream.read_exact(&mut got).expect("client read");
        assert_eq!(got, [0x42]);
        drop(stream);

        server_handle.join().expect("server join");
        let _ = std::fs::remove_file(&path);
    }

    /// A0.2 unix path — connect to a non-existent path surfaces as
    /// `ConnectFailed` (not `UnsupportedFamily` or panic). The exact
    /// `io::ErrorKind` is host-OS dependent (Linux = `ConnectionRefused`,
    /// macOS = `NotFound`), so we only assert the variant.
    #[test]
    fn connect_owned_fd_unix_missing_path_is_connect_failed() {
        let path = PathBuf::from(format!("/tmp/rsb_2_14_nope_{}.sock", std::process::id()));
        let addr = AccessorSockAddr::Unix(path);
        let err = addr.connect_owned_fd().expect_err("must fail");
        assert!(
            matches!(err, AccessorConnectError::ConnectFailed(_)),
            "expected ConnectFailed, got {err:?}"
        );
    }

    /// A0.2 vsock / inet `UnsupportedFamily` when the feature is off
    /// (= rsbinder default build on macOS). This is the AOSP-faithful
    /// `ERROR_UNSUPPORTED_SOCKET_FAMILY` path. When the feature **is**
    /// built in, the connect attempt would surface as `ConnectFailed`
    /// (no listener on `cid:port` / `127.0.0.1:0`) — that arm is left
    /// for the integration test in `tests/rpc_accessor_register.rs`
    /// (D.8.a), since exercising it requires a live listener and the
    /// vsock crate's connect behaviour differs on macOS.
    #[cfg(not(feature = "rpc-vsock"))]
    #[test]
    fn connect_owned_fd_vsock_disabled_is_unsupported() {
        let addr = AccessorSockAddr::Vsock { cid: 2, port: 5555 };
        let err = addr.connect_owned_fd().expect_err("vsock disabled");
        assert!(
            matches!(err, AccessorConnectError::UnsupportedFamily),
            "expected UnsupportedFamily, got {err:?}"
        );
    }

    #[cfg(not(feature = "rpc-tcp-debug"))]
    #[test]
    fn connect_owned_fd_inet_disabled_is_unsupported() {
        let addr = AccessorSockAddr::Inet("127.0.0.1:5555".parse().expect("addr"));
        let err = addr.connect_owned_fd().expect_err("inet disabled");
        assert!(
            matches!(err, AccessorConnectError::UnsupportedFamily),
            "expected UnsupportedFamily, got {err:?}"
        );
    }

    /// `AccessorConnectError::Display` includes the inner `io::Error`
    /// message when present — important for `log::warn!` lines that
    /// surface the OS-level cause next to the AOSP error code.
    #[test]
    fn connect_error_display_includes_inner_cause() {
        let e = AccessorConnectError::ConnectFailed(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "test cause",
        ));
        let s = format!("{e}");
        assert!(s.contains("test cause"), "Display missing inner: {s}");
        assert!(s.contains("connect(2)"), "Display missing op label: {s}");
    }

    /// Anti-leak invariant — `connect_owned_fd` on a successful
    /// connect returns a fd whose `AsRawFd` is non-negative and
    /// non-stdio (>= 3). Compile-time witness for the OwnedFd shape.
    #[test]
    fn connect_owned_fd_returns_real_fd() {
        let (listener, path) = unique_unix_listener("a02-fd");
        let server_handle = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let addr = AccessorSockAddr::Unix(path.clone());
        let fd = addr.connect_owned_fd().expect("connect");
        assert!(
            fd.as_raw_fd() >= 3,
            "expected real fd, got {}",
            fd.as_raw_fd()
        );
        drop(fd);
        server_handle.join().expect("server join");
        let _ = std::fs::remove_file(&path);
    }

    // --- A0.3 LocalAccessor tests (in-process Bn/Bp round-trip) -----

    use crate::FromIBinder;

    /// A0.3 happy path: `LocalAccessor::new_binder(...)` ⇒ Bn-side
    /// SIBinder ⇒ in-process `BpAccessor::from_binder` (via the
    /// generated `IAccessor::FromIBinder` hook) ⇒
    /// `getInstanceName()` returns the stored name + `addConnection()`
    /// returns a fd connected to a listener on the provider's path.
    /// This is the round-trip witness that the AIDL marshalling +
    /// kernel binder driver path works on the *same* process (no
    /// cross-process kernel binder needed — `Binder::new` + local
    /// proxy stand in for the kernel hop).
    #[test]
    fn local_accessor_inprocess_roundtrip() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        let (listener, path) = unique_unix_listener("a03-rt");
        // Server thread — echo one byte so we can prove the fd is
        // bidirectionally functional after transfer.
        let server_handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1];
            s.read_exact(&mut buf).expect("read");
            s.write_all(&buf).expect("write");
        });

        let provider_path = path.clone();
        let provider: AccessorAddrProvider =
            Box::new(move |_name: &str| Ok(AccessorSockAddr::Unix(provider_path.clone())));
        let strong = LocalAccessor::new_binder("rsbinder.test.a03", provider);
        let sib: crate::binder::SIBinder = strong.as_binder();

        // Cast back to `dyn IAccessor` via the AIDL `FromIBinder` hook
        // — exactly the path a consumer would take.
        let bp = <dyn IAccessor as FromIBinder>::try_from(sib).expect("FromIBinder");
        assert_eq!(
            bp.getInstanceName().expect("getInstanceName"),
            "rsbinder.test.a03"
        );

        let pfd = bp.addConnection().expect("addConnection");
        let fd: std::os::fd::OwnedFd = pfd.into();
        let mut stream = UnixStream::from(fd);
        stream.write_all(&[0xA5]).expect("client write");
        let mut got = [0u8; 1];
        stream.read_exact(&mut got).expect("client read");
        assert_eq!(got, [0xA5]);
        drop(stream);

        server_handle.join().expect("server join");
        let _ = std::fs::remove_file(&path);
    }

    /// A0.3 provider error path: provider returns `Err` ⇒
    /// `addConnection()` ⇒ `Status::ServiceSpecific(ERROR_CONNECTION_INFO_NOT_FOUND)`.
    /// `getInstanceName()` keeps working — the error is *per-call*,
    /// not per-instance.
    #[test]
    fn local_accessor_provider_error_maps_to_connection_info_not_found() {
        let provider: AccessorAddrProvider =
            Box::new(|_name: &str| Err(crate::error::StatusCode::BadValue));
        let strong = LocalAccessor::new_binder("rsbinder.test.a03.provider-err", provider);
        let sib = strong.as_binder();
        let bp = <dyn IAccessor as FromIBinder>::try_from(sib).expect("FromIBinder");

        // getInstanceName still works.
        assert_eq!(
            bp.getInstanceName().unwrap(),
            "rsbinder.test.a03.provider-err"
        );
        // addConnection bubbles the AOSP-faithful service-specific error.
        let status = bp.addConnection().expect_err("must fail");
        assert_eq!(
            status.service_specific_error(),
            ERROR_CONNECTION_INFO_NOT_FOUND,
            "expected ERROR_CONNECTION_INFO_NOT_FOUND, got {status:?}"
        );
    }

    /// A0.3 connect-failure path: provider returns a valid
    /// `AccessorSockAddr::Unix` whose path has no listener ⇒
    /// `addConnection()` ⇒
    /// `Status::ServiceSpecific(ERROR_FAILED_TO_CONNECT_TO_SOCKET)`.
    /// Confirms the (AccessorConnectError → ERROR_* code) mapping is
    /// wired through the AIDL marshal correctly.
    #[test]
    fn local_accessor_connect_failure_maps_to_connect_to_socket() {
        let path = PathBuf::from(format!(
            "/tmp/rsb_2_14_a03_nope_{}.sock",
            std::process::id()
        ));
        let provider: AccessorAddrProvider =
            Box::new(move |_name: &str| Ok(AccessorSockAddr::Unix(path.clone())));
        let strong = LocalAccessor::new_binder("rsbinder.test.a03.no-listener", provider);
        let sib = strong.as_binder();
        let bp = <dyn IAccessor as FromIBinder>::try_from(sib).expect("FromIBinder");

        let status = bp.addConnection().expect_err("must fail (no listener)");
        assert_eq!(
            status.service_specific_error(),
            ERROR_FAILED_TO_CONNECT_TO_SOCKET,
            "expected ERROR_FAILED_TO_CONNECT_TO_SOCKET, got {status:?}"
        );
    }

    /// A0.3 unsupported-family path (feature-disabled vsock on
    /// macOS-default build): provider returns `Vsock { .. }` ⇒
    /// `addConnection()` ⇒
    /// `Status::ServiceSpecific(ERROR_UNSUPPORTED_SOCKET_FAMILY)`.
    /// The same code path that AOSP returns for a wrong family — so an
    /// `instance` configured for vsock on a build without
    /// `rpc-vsock` surfaces with AOSP-faithful semantics rather than a
    /// compile-time mismatch.
    #[cfg(not(feature = "rpc-vsock"))]
    #[test]
    fn local_accessor_unsupported_family_maps_to_unsupported() {
        let provider: AccessorAddrProvider =
            Box::new(|_name: &str| Ok(AccessorSockAddr::Vsock { cid: 2, port: 5555 }));
        let strong = LocalAccessor::new_binder("rsbinder.test.a03.vsock-disabled", provider);
        let sib = strong.as_binder();
        let bp = <dyn IAccessor as FromIBinder>::try_from(sib).expect("FromIBinder");

        let status = bp.addConnection().expect_err("must fail (vsock disabled)");
        assert_eq!(
            status.service_specific_error(),
            ERROR_UNSUPPORTED_SOCKET_FAMILY,
            "expected ERROR_UNSUPPORTED_SOCKET_FAMILY, got {status:?}"
        );
    }

    // --- A.4 process-local registry tests --------------------------
    //
    // `REGISTRY` is a process-wide `OnceLock<Mutex<Vec<_>>>`; under
    // `cargo test` multiple test threads share it. We give every test
    // a unique instance-name namespace (`format!(..., line!())`) so
    // concurrent registrations from sibling tests don't false-collide
    // on the duplicate-instance reject — AOSP test scaffolding uses
    // the same pattern (`gAccessorProviders` is global, tests
    // namespace their fake instances).

    fn unique_instance(tag: &str) -> String {
        format!(
            "rsb.test.a4.{}.{}.{}",
            tag,
            std::process::id(),
            line!() // distinct per-source-line at the call site (each !invocation)
        )
    }

    /// AOSP `addAccessorProvider` happy path — register two providers
    /// for disjoint instance sets, verify each handle is live, verify
    /// `lookup_accessor_provider` dispatches to the matching
    /// provider's closure and returns its binder.
    #[test]
    fn add_two_providers_disjoint_instances() {
        let instance_a = unique_instance("disjoint-a");
        let instance_b = unique_instance("disjoint-b");

        let path_a = PathBuf::from(format!("/tmp/rsb_a4_disj_a_{}.sock", std::process::id()));
        let path_b = PathBuf::from(format!("/tmp/rsb_a4_disj_b_{}.sock", std::process::id()));
        let cap_a = path_a.clone();
        let cap_b = path_b.clone();

        let provider_a: AccessorProviderFn = {
            let want = instance_a.clone();
            Box::new(move |name: &str| {
                if name == want {
                    Some(create_accessor(
                        name,
                        Box::new({
                            let p = cap_a.clone();
                            move |_| Ok(AccessorSockAddr::Unix(p.clone()))
                        }),
                    ))
                } else {
                    None
                }
            })
        };
        let provider_b: AccessorProviderFn = {
            let want = instance_b.clone();
            Box::new(move |name: &str| {
                if name == want {
                    Some(create_accessor(
                        name,
                        Box::new({
                            let p = cap_b.clone();
                            move |_| Ok(AccessorSockAddr::Unix(p.clone()))
                        }),
                    ))
                } else {
                    None
                }
            })
        };

        let h_a =
            add_accessor_provider(HashSet::from([instance_a.clone()]), provider_a).expect("add a");
        let h_b =
            add_accessor_provider(HashSet::from([instance_b.clone()]), provider_b).expect("add b");
        assert!(h_a.is_live());
        assert!(h_b.is_live());

        // Lookup dispatches to the matching provider. Each provider
        // returns *its* binder; `getInstanceName` round-trips the
        // configured instance name.
        let bp_a = <dyn IAccessor as FromIBinder>::try_from(
            lookup_accessor_provider(&instance_a).expect("lookup a"),
        )
        .expect("cast a");
        assert_eq!(bp_a.getInstanceName().unwrap(), instance_a);

        let bp_b = <dyn IAccessor as FromIBinder>::try_from(
            lookup_accessor_provider(&instance_b).expect("lookup b"),
        )
        .expect("cast b");
        assert_eq!(bp_b.getInstanceName().unwrap(), instance_b);

        // Unknown instance returns None — neither provider claims it.
        assert!(lookup_accessor_provider("rsb.test.a4.absent.unknown").is_none());

        // Drop the handles — registry should shrink (lookups now fail).
        drop(h_a);
        drop(h_b);
        assert!(lookup_accessor_provider(&instance_a).is_none());
        assert!(lookup_accessor_provider(&instance_b).is_none());
    }

    /// AOSP duplicate-instance reject: a second
    /// `add_accessor_provider` with an instance already claimed by a
    /// live entry returns `Err(StatusCode::AlreadyExists)` and adds
    /// nothing. The first handle must stay live and authoritative.
    #[test]
    fn add_accessor_provider_rejects_duplicate_instance() {
        let instance = unique_instance("dup");
        let first: AccessorProviderFn = Box::new({
            let want = instance.clone();
            move |name: &str| {
                if name == want {
                    Some(create_accessor(
                        name,
                        Box::new(|_| {
                            Ok(AccessorSockAddr::Unix(PathBuf::from(
                                "/tmp/rsb_a4_dup_1.sock",
                            )))
                        }),
                    ))
                } else {
                    None
                }
            }
        });
        let h1 = add_accessor_provider(HashSet::from([instance.clone()]), first).expect("add 1");
        assert!(h1.is_live());

        let second: AccessorProviderFn = Box::new(|_name| {
            Some(create_accessor(
                "ignored",
                Box::new(|_| Ok(AccessorSockAddr::Unix(PathBuf::from("/tmp/x")))),
            ))
        });
        let err = add_accessor_provider(HashSet::from([instance.clone()]), second)
            .expect_err("dup must reject");
        assert_eq!(
            err,
            crate::error::StatusCode::AlreadyExists,
            "dup-instance must surface AlreadyExists, got {err:?}"
        );

        // Lookup still hits the *first* provider — second add added
        // nothing.
        let bp = <dyn IAccessor as FromIBinder>::try_from(
            lookup_accessor_provider(&instance).expect("lookup hits first"),
        )
        .expect("cast");
        assert_eq!(bp.getInstanceName().unwrap(), instance);

        drop(h1);
        assert!(lookup_accessor_provider(&instance).is_none());
    }

    /// `add_accessor_provider(HashSet::new(), ...)` ⇒ `BadValue`. AOSP
    /// `ALOGE("Set of instances is empty!")`+empty-weak.
    #[test]
    fn add_accessor_provider_rejects_empty_instances() {
        let provider: AccessorProviderFn = Box::new(|_| None);
        let err =
            add_accessor_provider(HashSet::new(), provider).expect_err("empty set must reject");
        assert_eq!(err, crate::error::StatusCode::BadValue);
    }

    /// RAII unregister — dropping the handle removes the entry from
    /// the registry; subsequent lookups return `None`.
    /// `remove_accessor_provider` on the already-dropped handle would
    /// be `BadValue` (we can't test that directly because handle drop
    /// consumes the handle; instead we test the explicit-removal path
    /// in [`explicit_remove_succeeds`] / [`explicit_remove_on_dead_returns_bad_value`]).
    #[test]
    fn handle_drop_unregisters() {
        let instance = unique_instance("raii");
        let provider: AccessorProviderFn = {
            let want = instance.clone();
            Box::new(move |name: &str| {
                if name == want {
                    Some(create_accessor(
                        name,
                        Box::new(|_| {
                            Ok(AccessorSockAddr::Unix(PathBuf::from(
                                "/tmp/rsb_a4_raii.sock",
                            )))
                        }),
                    ))
                } else {
                    None
                }
            })
        };
        let h = add_accessor_provider(HashSet::from([instance.clone()]), provider).expect("add");
        assert!(lookup_accessor_provider(&instance).is_some());
        drop(h);
        assert!(
            lookup_accessor_provider(&instance).is_none(),
            "handle drop must unregister"
        );
    }

    /// `remove_accessor_provider(live_handle)` ⇒ `Ok(())` and the
    /// entry is gone. Test name is also the explicit-removal-success
    /// witness.
    #[test]
    fn explicit_remove_succeeds() {
        let instance = unique_instance("explicit");
        let provider: AccessorProviderFn = {
            let want = instance.clone();
            Box::new(move |name: &str| {
                if name == want {
                    Some(create_accessor(
                        name,
                        Box::new(|_| {
                            Ok(AccessorSockAddr::Unix(PathBuf::from(
                                "/tmp/rsb_a4_explicit.sock",
                            )))
                        }),
                    ))
                } else {
                    None
                }
            })
        };
        let h = add_accessor_provider(HashSet::from([instance.clone()]), provider).expect("add");
        remove_accessor_provider(h).expect("explicit remove");
        assert!(
            lookup_accessor_provider(&instance).is_none(),
            "after explicit remove, lookup must be None"
        );
    }

    /// Concurrent register + lookup — P6 safety witness. Multiple
    /// threads register disjoint instances and look up each other's
    /// instances; no panic, no deadlock, and every lookup either
    /// finds the registered provider or returns `None` (never a stale
    /// proxy from another thread's registration in flight).
    #[test]
    fn concurrent_register_and_lookup_p6() {
        const N: usize = 8;
        let mut handles = Vec::new();
        let started = Arc::new(std::sync::Barrier::new(N));

        for i in 0..N {
            let bar = Arc::clone(&started);
            handles.push(std::thread::spawn(move || {
                bar.wait();
                let instance = format!(
                    "rsb.test.a4.p6.{}.{:?}.{}",
                    std::process::id(),
                    std::thread::current().id(),
                    i,
                );
                let cap = instance.clone();
                let provider: AccessorProviderFn = Box::new(move |name: &str| {
                    if name == cap {
                        Some(create_accessor(
                            name,
                            Box::new(|_| {
                                Ok(AccessorSockAddr::Unix(PathBuf::from("/tmp/rsb_a4_p6.sock")))
                            }),
                        ))
                    } else {
                        None
                    }
                });
                let h = add_accessor_provider(HashSet::from([instance.clone()]), provider)
                    .expect("add");
                // Self-lookup must succeed.
                let bp = <dyn IAccessor as FromIBinder>::try_from(
                    lookup_accessor_provider(&instance).expect("self lookup"),
                )
                .expect("cast");
                assert_eq!(bp.getInstanceName().unwrap(), instance);
                // Cross-lookup of an instance that wasn't registered
                // by anyone returns None (validates that the registry
                // doesn't return stale entries between threads).
                assert!(lookup_accessor_provider("rsb.test.a4.p6.never").is_none());
                drop(h);
            }));
        }

        for h in handles {
            h.join().expect("thread join");
        }
    }
}
