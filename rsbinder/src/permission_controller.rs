// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Client stub for Android's `PermissionManagerService`
//! (`android.os.IPermissionController`).
//!
//! rsbinder provides the **client side only** — the server lives in
//! Android's `system_server`. Consumers acquire a proxy via
//! [`crate::hub::try_get_service`]`("permission")` then cast through
//! [`crate::permission_controller::IPermissionController`].
//!
//! The wire descriptor is `"android.os.IPermissionController"`, generated
//! from `aidl/permission/android/os/IPermissionController.aidl` (vendored
//! verbatim from AOSP `frameworks/base/core/java/android/os/`). The
//! descriptor is stable across Android 10–16; rsbinder ships a single
//! AIDL (unlike `IServiceManager`, which is per-version).
//!
//! On non-Android targets (Linux + binderfs without an Android
//! userspace, or macOS), `system_server` does not exist — the helpers
//! still compile but nothing registers a `"permission"` service:
//! [`crate::hub::try_get_service`] returns `Ok(None)` where a service
//! manager is reachable but holds no such entry, and `Err` (or, with no
//! kernel binder at all, a `ProcessState` panic) where no service
//! manager is reachable. With no
//! [`crate::permission_controller::PermissionAuthority`] installed, the
//! [`crate::permission_controller::check_permission`] convenience
//! returns `false` (fail-closed) on each path it can take **as long as
//! nothing is registered under that name**: a parcel that
//! does not use kernel marshalling is denied by the marshalling-mode gate
//! before the lookup is reached, a kernel-marshalling parcel outside a
//! transaction is denied by the `is_handling_transaction` gate, and a
//! lookup that yields `Ok(None)` or `Err` denies as well. The Linux
//! `hub` is a general-purpose service manager: a process allowed to
//! register can [`crate::hub::add_service`] a binder under the name
//! `"permission"`, and if its descriptor matches, the lookup
//! succeeds and `check_permission` returns that service's answer
//! verbatim — `true` included.
//!
//! With nothing registered under that name, the `ProcessState` panic is
//! the one outcome that is not a `false`.
//! Reaching it takes a kernel-marshalling parcel *and* an in-flight
//! transaction *and* a process with no kernel binder — a combination only
//! an RPC dispatch handed some parcel other than its own inbound one can
//! produce, since `is_handling_transaction` is otherwise false there.
//! Generated `@EnforcePermission` code always passes the inbound RPC
//! parcel, which stops at the first gate, so this is reachable only from
//! a hand-written handler.

include!(concat!(env!("OUT_DIR"), "/permission_controller.rs"));

pub use android::os::IPermissionController::{
    BnPermissionController, BpPermissionController, IPermissionController,
    IPermissionControllerDefault, IPermissionControllerDefaultRef,
};

#[cfg(feature = "async")]
pub use android::os::IPermissionController::{
    IPermissionControllerAsync, IPermissionControllerAsyncService,
};

use std::sync::{Arc, RwLock};

use crate::error::Result;
use crate::{hub, Caller, FromIBinder, Parcel, Strong};

/// Service name registered by Android's `PermissionManagerService`. Used
/// as the key for [`crate::hub::try_get_service`].
pub const SERVICE_NAME: &str = "permission";

/// An injectable, **transport-aware** authorization policy (Plan 2-16
/// Phase C). When one is installed via [`set_permission_authority`], it
/// **owns the decision whenever a caller is in flight** for every
/// generated `@EnforcePermission` check ([`check_permission`]) — across
/// kernel binder *and* RPC — and receives the transport-tagged [`Caller`]
/// so it can apply the right rule per transport (Android permission for
/// [`Caller::Kernel`]; a uid ACL, TLS-certificate allowlist, or token
/// scope for `Caller::Rpc`). With no transaction in flight
/// ([`crate::calling_caller`] returns `None`) the check fails closed
/// without consulting it, so an authority never sees an in-process call
/// and cannot log or allow one.
///
/// The core crate ships only this **slot**, never a policy: token/JWT
/// formats, certificate→permission tables, and uid→permission maps are
/// deployment concerns. With **no** authority installed, [`check_permission`]
/// keeps its safe default — kernel → `PermissionManagerService`, RPC →
/// deny (Plan 2-16 Phase A).
pub trait PermissionAuthority: Send + Sync {
    /// Return `true` to grant `permission` to `caller`. Implementations
    /// should fail closed for callers/transports they do not recognize.
    fn check(&self, permission: &str, caller: &Caller) -> bool;
}

/// Process-wide injected authority. `None` ⇒ the built-in default
/// ([`check_permission`] kernel-PMS / RPC-deny). Deployment policy, set
/// once at startup; a `RwLock` (not `OnceLock`) so tests and dynamic
/// reconfiguration can replace or [`clear_permission_authority`] it.
static AUTHORITY: RwLock<Option<Arc<dyn PermissionAuthority>>> = RwLock::new(None);

/// Install the process-wide [`PermissionAuthority`]; subsequent
/// [`check_permission`] calls delegate the whole decision to it. Replaces
/// any previously installed authority.
pub fn set_permission_authority(authority: Arc<dyn PermissionAuthority>) {
    *AUTHORITY.write().expect("permission authority poisoned") = Some(authority);
}

/// Remove any installed [`PermissionAuthority`], restoring the built-in
/// default (kernel → PMS, RPC → deny).
pub fn clear_permission_authority() {
    *AUTHORITY.write().expect("permission authority poisoned") = None;
}

/// Acquire a `BpPermissionController` proxy for the system-wide
/// `permission` service, mirroring AOSP
/// `defaultServiceManager()->getService(String16("permission"))` +
/// `interface_cast<IPermissionController>(binder)`.
///
/// Returns `Err(StatusCode::NameNotFound)` when the service manager has
/// no `"permission"` entry — typically on non-Android Linux where
/// `system_server` is not running. On Android 11+ a service manager that
/// cannot be reached at all surfaces as its own transport error
/// (`DeadObject`, `FailedTransaction`, …) rather than `NameNotFound`, so
/// a caller that branches on `NameNotFound` to mean "no permission
/// service" must handle those separately. **Not on Android 10**: the
/// legacy C service manager cannot tell a not-found reply from a
/// transport failure and reports both as not-registered, so an
/// unreachable service manager arrives as `NameNotFound` there — but
/// only once the service manager proxy has been built; a first lookup
/// with no reachable service manager still fails inside `hub::default()`
/// with its own transport error. A descriptor mismatch propagates from
/// the `FromIBinder` cast.
///
/// This is a thin wrapper; consumers needing custom error mapping or
/// caching should call [`crate::hub::try_get_service`] directly.
///
/// # Panics
///
/// Panics if kernel binder was never initialized in this process: the
/// lookup reaches `ProcessState::as_self`.
pub fn default() -> Result<Strong<dyn IPermissionController>> {
    // Single-shot on purpose: a permission check must not block.
    let binder = hub::try_get_service(SERVICE_NAME)?.ok_or(crate::StatusCode::NameNotFound)?;
    <dyn IPermissionController>::try_from(binder)
}

/// Convenience: ask `system_server`'s `PermissionManagerService` whether
/// the current binder *caller* (as reported by
/// [`crate::get_calling_uid`] / [`crate::get_calling_pid`]) holds
/// `permission_name`.
///
/// This is the runtime backing the generated `@EnforcePermission` deny
/// block; `reader` is the inbound transaction parcel held by
/// `on_transact`, used to detect the transport (see *RPC fail-closed*).
///
/// Intended for server-side use inside a [`crate::Transactable::transact`]
/// dispatch, mirroring AOSP `IPCThreadState::self()->getCallingUid()` +
/// `IPermissionController::checkPermission(...)`.
///
/// # RPC fail-closed (`@EnforcePermission` is kernel-only) — Plan 2-16 Phase A
///
/// `@EnforcePermission` has **no meaning over the RPC transport**: AOSP's
/// RPC stack carries no Android permission concept, and an RPC peer's uid
/// is a different namespace from the one `PermissionManagerService`
/// arbitrates. RPC dispatch does stamp the peer (Plan 2-16 Phase B), so
/// [`crate::get_calling_uid`] answers — which is the problem, not the
/// remedy: a Unix RPC peer running as root reads its real uid `0`, and
/// PMS *unconditionally grants* root, so every guarded method would
/// become a **silent grant to any RPC peer that runs as root**; a peer on
/// a transport that carries no uid (`Vsock`, TLS `Certificate`,
/// `Anonymous`) reads the `u32::MAX` sentinel, which PMS knows nothing
/// about. To prevent this, a `reader` that does not use kernel
/// marshalling ([`Parcel::is_kernel_backed`] is `false`) returns `false`
/// **before any uid read or PMS lookup**, regardless of process shape,
/// and emits a one-time `warn`. Gating on the marshalling mode rather
/// than on one named transport is what denies a mode added later by
/// default. The deny does not depend on what uid an RPC transport
/// carries — no uid makes an RPC parcel kernel-backed.
///
/// [`Parcel::is_kernel_backed`] answers only *which marshalling the
/// parcel uses* — it is **not** a statement that a live kernel
/// transaction vouched for the caller; a freshly constructed
/// [`Parcel::new`] reports `true` as well. Whether a transaction is in
/// flight is a second question, which this function asks separately via
/// [`crate::is_handling_transaction`], and the two can disagree: while an
/// RPC dispatch is on the stack, a nested kernel `BR_TRANSACTION`
/// dispatched on the same thread hands the handler a kernel-marshalled
/// `reader`, yet the outer RPC calling context is still installed, so
/// [`crate::is_handling_transaction`] and [`crate::get_calling_uid`] /
/// [`crate::get_calling_pid`] answer for the **RPC** peer — PMS is then
/// asked about the RPC peer's uid while a kernel caller is being served.
/// That residual gap is not closed here: it needs the calling context to
/// be scoped per dispatch, not a stronger parcel predicate.
///
/// RPC services needing authorization must use transport-native means
/// (`PeerIdentity` + `RpcServer::set_authorizer`, or hand-rolled uid ACLs
/// via [`crate::get_calling_uid`] over Unix RPC).
///
/// # Fail-closed semantics
///
/// **With no [`PermissionAuthority`] installed**, returns `false` when:
/// - `reader` does not use kernel marshalling (see above) — a property of
///   the parcel, not of how the transaction was dispatched.
/// - The current thread is not handling a binder transaction (explicit
///   deny before uid/pid are read or PMS is consulted).
/// - The `permission` service is unreachable (`system_server` absent) —
///   the lookup error denies.
/// - The remote `checkPermission` call returns an error.
///
/// A reachable service that answers `true` grants, so this is fail-closed
/// against a *missing* permission service, not against a hostile one; see
/// the [module docs](crate::permission_controller) for what registering
/// under that name means on Linux.
///
/// This matches AOSP's "if in doubt, deny" posture for missing
/// PermissionManagerService — see Android `checkPermission` callers in
/// `frameworks/native/services/` for the same pattern.
///
/// # Panics
///
/// Panics if the [`default()`] lookup is reached in a process where
/// kernel binder was never initialized (`ProcessState::as_self`). That
/// takes a kernel-marshalling `reader` *and* an in-flight transaction
/// *and* an uninitialized `ProcessState` — see the [module
/// docs](crate::permission_controller) for the one path that produces all
/// three. An installed [`PermissionAuthority`] returns before the lookup,
/// so it cannot panic here.
///
/// # Injected authority (Plan 2-16 Phase C)
///
/// If a [`PermissionAuthority`] is installed via
/// [`set_permission_authority`], it **replaces** the default decision for
/// every transport and receives the transport-tagged [`Caller`] — but
/// only while a transaction is in flight; with none
/// ([`crate::calling_caller`] returns `None`) the check fails closed
/// without consulting it. With no authority installed (the default), the
/// kernel→PMS / RPC→deny behavior applies unchanged.
pub fn check_permission(reader: &Parcel, permission_name: &str) -> bool {
    // Injected deployment policy owns the whole decision when present. The
    // Arc is cloned out so the read lock is released before the (possibly
    // re-entrant) policy runs.
    let authority = AUTHORITY
        .read()
        .expect("permission authority poisoned")
        .clone();
    if let Some(authority) = authority {
        // Pass the transport-tagged caller; no caller (not in a
        // transaction) ⇒ fail closed.
        return crate::calling_caller()
            .is_some_and(|caller| authority.check(permission_name, &caller));
    }

    // Deny before uid is read: an RPC uid is not a PMS uid (see rustdoc).
    if !reader.is_kernel_backed() {
        warn_enforce_permission_over_rpc();
        return false;
    }
    // Outside a transaction uid/pid read as 0 and PMS grants root.
    if !crate::is_handling_transaction() {
        return false;
    }
    let calling_pid = crate::get_calling_pid();
    let calling_uid = crate::get_calling_uid();
    let Ok(pc) = default() else {
        return false;
    };
    pc.checkPermission(permission_name, calling_pid as i32, calling_uid as i32)
        .unwrap_or(false)
}

/// One-time `warn` the first time an `@EnforcePermission` method is denied
/// because it was dispatched over RPC. Per-process, not per-interface —
/// the message states the general rule, not a specific permission.
fn warn_enforce_permission_over_rpc() {
    use std::sync::Once;
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        log::warn!(
            "@EnforcePermission is unsupported over RPC and denies every \
             guarded method (Plan 2-16 Phase A); use PeerIdentity / \
             set_authorizer or uid ACLs for RPC authorization"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated trait must expose the AOSP wire descriptor
    /// verbatim — `"android.os.IPermissionController"`.
    /// A mismatch here would silently fail every cross-process call to
    /// `system_server` because the kernel-side `check_interface` would
    /// reject the inbound `writeInterfaceToken` prefix.
    #[test]
    fn test_descriptor_matches_aosp_wire() {
        // Pick any `Sized` impl — `descriptor()` is gated by
        // `where Self: Sized`, but every concrete impl returns the
        // same constant via the AOSP-required `META_INTERFACE` macro.
        assert_eq!(
            <BpPermissionController as IPermissionController>::descriptor(),
            "android.os.IPermissionController"
        );
    }

    /// `SERVICE_NAME` matches the AOSP-registered service
    /// name (`servicemanager` `addService("permission", ...)` in
    /// system_server). Any drift makes `default()` return
    /// `NameNotFound` on every real Android device.
    #[test]
    fn test_service_name_matches_system_server_registration() {
        assert_eq!(SERVICE_NAME, "permission");
    }

    /// Plan 2-16 Phase A unit-level proof: `check_permission` denies for
    /// an RPC parcel **before** consulting PMS. A kernel parcel outside a
    /// live transaction is denied at the `is_handling_transaction` gate
    /// before PMS is ever consulted — the RPC arm is the transport gate
    /// this asserts.
    //
    // `serial(authority)`: `AUTHORITY` is a process global, so this default
    // test must not run concurrently with the authority-delegation test.
    #[cfg(feature = "rpc")]
    #[serial_test::serial(authority)]
    #[test]
    fn check_permission_denies_rpc_parcel() {
        use crate::rpc::transport::PeerIdentity;
        use crate::thread_state::RpcCallingGuard;
        use std::sync::Arc;

        let mut rpc_parcel = Parcel::new();
        rpc_parcel.set_for_rpc(true);
        // Inside a (simulated) RPC transaction, so `is_handling_transaction()`
        // is `true` and only the kernel-backing gate can produce the denial.
        let _g = RpcCallingGuard::install(Arc::new(PeerIdentity::Local { uid: 1000, pid: 7 }));
        assert!(crate::is_handling_transaction());
        assert!(
            !check_permission(&rpc_parcel, "android.permission.INTERNET"),
            "RPC parcel must fail-closed regardless of uid/PMS"
        );

        // A kernel parcel passes the kernel-backing gate and is denied one
        // step later, by `is_handling_transaction` — dropping the guard first
        // keeps `default()` (and its `ProcessState`) out of reach.
        drop(_g);
        let kernel_parcel = Parcel::new();
        assert!(kernel_parcel.is_kernel_backed());
        // Pin the precondition the gate fires on: without it the denial
        // below could equally come from `default()` failing, which stays
        // `false` even if the gate were deleted.
        assert!(!crate::is_handling_transaction());
        assert!(
            !check_permission(&kernel_parcel, "android.permission.INTERNET"),
            "outside a transaction the kernel path denies before PMS"
        );
    }

    /// Plan 2-16 Phase C: an installed [`PermissionAuthority`] owns the
    /// decision for every transport and receives the transport-tagged
    /// [`Caller`]. Here a policy grants one permission to a specific
    /// Unix-RPC uid — which the *default* path would unconditionally deny
    /// over RPC — proving the slot can implement RPC authorization.
    #[cfg(feature = "rpc")]
    #[serial_test::serial(authority)]
    #[test]
    fn injected_authority_overrides_default_rpc_deny() {
        use crate::rpc::transport::PeerIdentity;
        use crate::thread_state::RpcCallingGuard;
        use crate::Caller;
        use std::sync::Arc;

        struct UidGrant {
            allow_uid: u32,
            permission: String,
        }
        impl PermissionAuthority for UidGrant {
            fn check(&self, permission: &str, caller: &Caller) -> bool {
                matches!(caller, Caller::Rpc(PeerIdentity::Local { uid, .. })
                    if *uid == self.allow_uid && permission == self.permission)
            }
        }

        set_permission_authority(Arc::new(UidGrant {
            allow_uid: 1000,
            permission: "com.example.DO_THING".to_string(),
        }));

        let mut rpc_parcel = Parcel::new();
        rpc_parcel.set_for_rpc(true);

        // Inside an RPC transaction from uid 1000: the authority grants the
        // one permission it knows, and denies everything else.
        {
            let _g = RpcCallingGuard::install(Arc::new(PeerIdentity::Local { uid: 1000, pid: 7 }));
            assert!(
                check_permission(&rpc_parcel, "com.example.DO_THING"),
                "authority must grant the allowed uid+permission over RPC"
            );
            assert!(
                !check_permission(&rpc_parcel, "com.example.OTHER"),
                "authority must deny an unknown permission"
            );
        }
        // Different uid ⇒ deny.
        {
            let _g = RpcCallingGuard::install(Arc::new(PeerIdentity::Local { uid: 2000, pid: 7 }));
            assert!(
                !check_permission(&rpc_parcel, "com.example.DO_THING"),
                "authority must deny a non-allowed uid"
            );
        }
        // No in-flight transaction ⇒ no caller ⇒ fail closed.
        assert!(!check_permission(&rpc_parcel, "com.example.DO_THING"));

        // Restore the default so other tests see kernel-PMS / RPC-deny.
        clear_permission_authority();
        let _g = RpcCallingGuard::install(Arc::new(PeerIdentity::Local { uid: 1000, pid: 7 }));
        assert!(
            !check_permission(&rpc_parcel, "com.example.DO_THING"),
            "after clear, the default RPC deny is restored"
        );
    }
}
