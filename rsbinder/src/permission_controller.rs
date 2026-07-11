// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Client stub for Android's `PermissionManagerService`
//! (`android.os.IPermissionController`).
//!
//! rsbinder provides the **client side only** — the server lives in
//! Android's `system_server`. Consumers acquire a proxy via
//! [`crate::hub::get_service`]`("permission")` then cast through
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
//! still compile but [`crate::hub::get_service`] returns `None` and the
//! [`crate::permission_controller::check_permission`] convenience
//! returns `false` (fail-closed).

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
/// as the key for [`crate::hub::get_service`].
pub const SERVICE_NAME: &str = "permission";

/// An injectable, **transport-aware** authorization policy (Plan 2-16
/// Phase C). When one is installed via [`set_permission_authority`], it
/// **owns the whole decision** for every generated `@EnforcePermission`
/// check ([`check_permission`]) — across kernel binder *and* RPC — and
/// receives the transport-tagged [`Caller`] so it can apply the right rule
/// per transport (Android permission for [`Caller::Kernel`]; a uid ACL,
/// TLS-certificate allowlist, or token scope for `Caller::Rpc`).
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
/// `system_server` is not running. Other errors propagate from the
/// `FromIBinder` cast (descriptor mismatch).
///
/// This is a thin wrapper; consumers needing custom error mapping or
/// caching should call [`crate::hub::get_service`] directly.
#[allow(deprecated)] // single-shot lookup of the permission service is intended
pub fn default() -> Result<Strong<dyn IPermissionController>> {
    let binder = hub::get_service(SERVICE_NAME).ok_or(crate::StatusCode::NameNotFound)?;
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
/// RPC stack carries no uid/permission concept, and on the RPC dispatch
/// path [`crate::get_calling_uid`] is not populated, so it would read `0`
/// (= root) — and `PermissionManagerService` *unconditionally grants
/// root*. That would turn every guarded method into a **silent grant to
/// any anonymous RPC peer**. To prevent this, when `reader` is an RPC
/// parcel ([`Parcel::is_for_rpc`]) this returns `false` **before any uid
/// read or PMS lookup**, regardless of process shape, and emits a
/// one-time `warn`. The deny is therefore independent of whether uid is
/// later wired over Unix RPC (Plan 2-16 Phase B): `is_for_rpc()` stays
/// `true` no matter what uid is populated.
///
/// RPC services needing authorization must use transport-native means
/// (`PeerIdentity` + `RpcServer::set_authorizer`, or hand-rolled uid ACLs
/// via [`crate::get_calling_uid`] over Unix RPC).
///
/// # Fail-closed semantics
///
/// Returns `false` when:
/// - The transaction arrived over RPC (see above).
/// - The current thread is not handling a binder transaction
///   (`get_calling_uid()` / `get_calling_pid()` both `0`).
/// - The `permission` service is unreachable (`system_server` absent or
///   the binder driver is not initialized).
/// - The remote `checkPermission` call returns an error.
///
/// This matches AOSP's "if in doubt, deny" posture for missing
/// PermissionManagerService — see Android `checkPermission` callers in
/// `frameworks/native/services/` for the same pattern.
///
/// # Injected authority (Plan 2-16 Phase C)
///
/// If a [`PermissionAuthority`] is installed via
/// [`set_permission_authority`], it **replaces** the default decision for
/// every transport and receives the transport-tagged [`Caller`]. With no
/// authority installed (the default), the kernel→PMS / RPC→deny behavior
/// below applies unchanged.
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

    // Default (no authority). RPC fail-closed: deny before reading uid or
    // reaching PMS — uid 0 on the RPC path would otherwise read as root and
    // PMS grants root. The deny is transport-driven (the reader knows it is
    // an RPC parcel), so it is independent of Plan 2-16 Phase B uid wiring.
    if reader.is_for_rpc() {
        warn_enforce_permission_over_rpc();
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
    /// an RPC parcel **before** consulting PMS. A kernel parcel falls
    /// through to the PMS path (which returns `false` here only because
    /// `system_server` is unreachable in this hermetic build) — the RPC
    /// arm is the new transport gate this asserts.
    //
    // `serial(authority)`: `AUTHORITY` is a process global, so this default
    // test must not run concurrently with the authority-delegation test.
    #[cfg(feature = "rpc")]
    #[serial_test::serial(authority)]
    #[test]
    fn check_permission_denies_rpc_parcel() {
        let mut rpc_parcel = Parcel::new();
        rpc_parcel.set_for_rpc(true);
        assert!(
            !check_permission(&rpc_parcel, "android.permission.INTERNET"),
            "RPC parcel must fail-closed regardless of uid/PMS"
        );

        // Sanity: a kernel parcel takes the non-RPC branch (it does not
        // short-circuit on `is_for_rpc`).
        let kernel_parcel = Parcel::new();
        assert!(!kernel_parcel.is_for_rpc());
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
