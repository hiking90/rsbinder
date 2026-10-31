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
//! nothing is registered under that name**: a check inside an RPC
//! transaction and a check outside any transaction are denied before the
//! lookup is reached, and a lookup that yields `Ok(None)` or `Err` denies
//! as well. The lookup is reached only inside a kernel transaction, so
//! kernel binder is initialized whenever it runs. The Linux
//! `hub` is a general-purpose service manager: a process allowed to
//! register can [`crate::hub::add_service`] a binder under the name
//! `"permission"`, and if its descriptor matches, the lookup
//! succeeds and `check_permission` returns that service's answer
//! verbatim — `true` included.
//!
//! The lookup is not repeated per check. A controller that answered once
//! is kept and asked directly until a call to it fails, as AOSP libbinder
//! keeps `gPermissionController`. Until then, re-registering `"permission"`
//! under another binder or restarting the service manager does not change
//! which process answers, and the kept strong reference counts as a client
//! of the registered service, so its `tryUnregisterService` is refused.

// Private: the re-exports below are the one public path to the generated items.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/permission_controller.rs"));
}

pub use generated::android::os::IPermissionController::{
    BnPermissionController, BpPermissionController, IPermissionController,
    IPermissionControllerDefault, IPermissionControllerDefaultRef,
};

#[cfg(feature = "async")]
pub use generated::android::os::IPermissionController::{
    IPermissionControllerAsync, IPermissionControllerAsyncService,
};

use std::sync::{Arc, Mutex, PoisonError, RwLock};

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

/// Injected authority (`None` = default); a `RwLock`, not `OnceLock`, so it can be replaced.
static AUTHORITY: RwLock<Option<Arc<dyn PermissionAuthority>>> = RwLock::new(None);

/// Install the process-wide [`PermissionAuthority`]; subsequent
/// [`check_permission`] calls delegate the whole decision to it. Replaces
/// any previously installed authority, which is dropped here after the
/// slot's lock is released.
///
/// Code that installs an authority into a process it does not own (a
/// library) should use [`replace_permission_authority`] and keep what it
/// replaced.
pub fn set_permission_authority(authority: Arc<dyn PermissionAuthority>) {
    drop(replace_permission_authority(Some(authority)));
}

/// Remove any installed [`PermissionAuthority`], restoring the built-in
/// default (kernel → PMS, RPC → deny). The removed authority is dropped
/// after the slot's lock is released.
pub fn clear_permission_authority() {
    drop(replace_permission_authority(None));
}

/// Install `authority` as the process-wide [`PermissionAuthority`], or
/// restore the built-in default with `None`, and return the authority it
/// replaced, `None` if the default was in effect.
///
/// A check already running in another thread finishes with the authority
/// it started with.
pub fn replace_permission_authority(
    authority: Option<Arc<dyn PermissionAuthority>>,
) -> Option<Arc<dyn PermissionAuthority>> {
    let mut slot = AUTHORITY.write().unwrap_or_else(PoisonError::into_inner);
    // Returned after the guard drops, so the old authority's user `Drop` never runs under the lock.
    std::mem::replace(&mut *slot, authority)
}

/// The process-wide [`PermissionAuthority`] installed now, `None` while
/// the built-in default (kernel → PMS, RPC → deny) is in effect.
pub fn permission_authority() -> Option<Arc<dyn PermissionAuthority>> {
    AUTHORITY
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
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
/// about. To prevent this, a check made while an RPC transaction is the
/// innermost one on this thread ([`crate::calling_caller`] is
/// `Caller::Rpc`) returns `false` **before any PMS lookup**, whatever uid
/// the transport carries, and emits a one-time `warn`.
///
/// The transport is decided by [`crate::calling_caller`] alone, the
/// calling context the dispatcher installed for the innermost transaction
/// on this thread. `reader` is not consulted: generated code passes the
/// inbound parcel, but a parcel's marshalling mode says nothing about who
/// is calling, and a handler can hand this function any parcel. Nesting
/// follows the innermost frame in both directions: a kernel
/// `BR_TRANSACTION` dispatched inside an RPC handler suspends the RPC
/// calling context, so PMS is asked about the kernel caller; an RPC
/// dispatch inside a kernel handler is the innermost frame, so the check
/// denies.
///
/// RPC services needing authorization must use transport-native means
/// (`PeerIdentity` + `RpcServer::set_authorizer`, or hand-rolled uid ACLs
/// via [`crate::get_calling_uid`] over Unix RPC).
///
/// # Fail-closed semantics
///
/// **With no [`PermissionAuthority`] installed**, returns `false` when:
/// - The innermost transaction on this thread is an RPC one (see above).
/// - The current thread is not handling a binder transaction (explicit
///   deny before PMS is consulted).
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
/// The controller that answered the last check is kept process-wide and
/// asked directly next time, as AOSP libbinder's `checkPermission` keeps
/// `gPermissionController`, so a check costs one IPC instead of a service
/// manager lookup plus one. When the kept controller fails (its process
/// restarted), it is dropped and the same check goes once to a fresh
/// lookup. Unlike AOSP, a missing service is not waited for: the check
/// denies at once.
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
pub fn check_permission(_reader: &Parcel, permission_name: &str) -> bool {
    // Clone the Arc out so the read lock is released before the (re-entrant) policy runs.
    let authority = permission_authority();
    // No caller (not in a transaction) ⇒ fail closed on either path.
    let caller = crate::calling_caller();
    if let Some(authority) = authority {
        return caller.is_some_and(|caller| authority.check(permission_name, &caller));
    }
    match caller {
        Some(Caller::Kernel { uid, pid, .. }) => ask_controller(&CONTROLLER, default, |pc| {
            pc.checkPermission(permission_name, pid, uid as i32)
        }),
        // An RPC uid is not a PMS uid (see rustdoc).
        #[cfg(feature = "rpc")]
        Some(Caller::Rpc(_)) => {
            warn_enforce_permission_over_rpc();
            false
        }
        None => false,
    }
}

type ControllerSlot = Mutex<Option<Arc<Strong<dyn IPermissionController>>>>;

/// AOSP `gPermissionController`; see [`check_permission`].
static CONTROLLER: ControllerSlot = Mutex::new(None);

/// The lock is never held across an IPC.
fn ask_controller(
    slot: &ControllerSlot,
    lookup: impl FnOnce() -> Result<Strong<dyn IPermissionController>>,
    ask: impl Fn(&Strong<dyn IPermissionController>) -> crate::BinderResult<bool>,
) -> bool {
    let lock = || slot.lock().unwrap_or_else(PoisonError::into_inner);
    let cached = lock().clone();
    if let Some(pc) = cached {
        if let Ok(granted) = ask(&pc) {
            return granted;
        }
        let mut slot = lock();
        // Another thread may already have installed a fresh one; keep that.
        if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &pc)) {
            *slot = None;
        }
    }
    let Ok(pc) = lookup() else {
        return false;
    };
    let Ok(granted) = ask(&pc) else {
        return false;
    };
    *lock() = Some(Arc::new(pc));
    granted
}

/// Warn once per process (not per interface) when `@EnforcePermission` first denies over RPC.
#[cfg(feature = "rpc")]
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

    /// A Unix RPC session's caps before fd-mode negotiation; incidental, but the guard takes it.
    #[cfg(feature = "rpc")]
    const UNIX_CAPS: crate::TransportCaps =
        crate::TransportCaps::TRUSTED_UID.union(crate::TransportCaps::SAME_HOST);

    /// AOSP wire descriptor; on a mismatch `system_server` rejects every interface token.
    #[test]
    fn test_descriptor_matches_aosp_wire() {
        // `descriptor()` needs `Self: Sized`; every concrete impl returns the same constant.
        assert_eq!(
            <BpPermissionController as IPermissionController>::descriptor(),
            "android.os.IPermissionController"
        );
    }

    /// system_server's `addService("permission", ...)`; drift makes `default()` `NameNotFound`.
    #[test]
    fn test_service_name_matches_system_server_registration() {
        assert_eq!(SERVICE_NAME, "permission");
    }

    /// Answers `checkPermission` with `grant`, or fails as a dead binder once `dead` is set.
    struct FakeController {
        grant: bool,
        dead: Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::Interface for FakeController {}

    impl IPermissionController for FakeController {
        fn checkPermission(&self, _: &str, _: i32, _: i32) -> crate::BinderResult<bool> {
            if self.dead.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::StatusCode::DeadObject.into());
            }
            Ok(self.grant)
        }
        fn noteOp(&self, _: &str, _: i32, _: &str) -> crate::BinderResult<i32> {
            Ok(0)
        }
        fn getPackagesForUid(&self, _: i32) -> crate::BinderResult<Vec<String>> {
            Ok(Vec::new())
        }
        fn isRuntimePermission(&self, _: &str) -> crate::BinderResult<bool> {
            Ok(false)
        }
        fn getPackageUid(&self, _: &str, _: i32) -> crate::BinderResult<i32> {
            Ok(0)
        }
    }

    fn fake(
        grant: bool,
    ) -> (
        Strong<dyn IPermissionController>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pc = BnPermissionController::new_binder(FakeController {
            grant,
            dead: dead.clone(),
        });
        (pc, dead)
    }

    /// Kept until it fails, then replaced by a lookup in the same call; failures are not cached.
    #[test]
    fn the_controller_is_cached_until_it_fails() {
        let ask = |pc: &Strong<dyn IPermissionController>| pc.checkPermission("p", 1, 2);
        let slot: ControllerSlot = Mutex::new(None);

        let (first, first_dead) = fake(true);
        assert!(ask_controller(&slot, || Ok(first), ask));
        assert!(
            ask_controller(&slot, || panic!("a cached controller needs no lookup"), ask),
            "the cached controller answers"
        );

        // `system_server` restarted: the cached proxy is dead, a lookup finds the new one.
        first_dead.store(true, std::sync::atomic::Ordering::SeqCst);
        let (second, _second_dead) = fake(false);
        let second_binder = second.as_binder();
        assert!(
            !ask_controller(&slot, || Ok(second), ask),
            "the fresh controller's answer, not a failure's deny"
        );
        let cached = slot
            .lock()
            .unwrap()
            .clone()
            .expect("the fresh controller is cached");
        assert!(cached.as_binder() == second_binder);

        // A fresh controller that fails is not cached, and the check denies.
        let slot: ControllerSlot = Mutex::new(None);
        let (broken, broken_dead) = fake(true);
        broken_dead.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(!ask_controller(&slot, || Ok(broken), ask));
        assert!(slot.lock().unwrap().is_none());
        // No controller at all denies.
        assert!(!ask_controller(
            &slot,
            || Err(crate::StatusCode::NameNotFound),
            ask
        ));
    }

    /// Plan 2-16 Phase A: inside an RPC transaction, or outside any, the check denies pre-PMS.
    // `serial(authority)`: `AUTHORITY` is process-global and the delegation test sets it.
    #[cfg(feature = "rpc")]
    #[serial_test::serial(authority)]
    #[test]
    fn check_permission_denies_rpc_parcel() {
        use crate::rpc::transport::PeerIdentity;
        use crate::thread_state::RpcCallingGuard;
        use std::sync::Arc;

        let mut rpc_parcel = Parcel::new();
        rpc_parcel.set_for_rpc(true).unwrap();
        let kernel_parcel = Parcel::new();
        assert!(kernel_parcel.is_kernel_backed());
        // A root peer: the uid PMS grants unconditionally, were it ever asked.
        let _g =
            RpcCallingGuard::install(Arc::new(PeerIdentity::Local { uid: 0, pid: 7 }), UNIX_CAPS);
        assert!(crate::is_handling_transaction());
        assert!(
            !check_permission(&rpc_parcel, "android.permission.INTERNET"),
            "RPC parcel must fail-closed regardless of uid/PMS"
        );
        // The transport comes from the calling context, not from the parcel handed in.
        assert!(
            !check_permission(&kernel_parcel, "android.permission.INTERNET"),
            "a kernel-marshalled parcel inside an RPC transaction must fail-closed too"
        );

        drop(_g);
        // Without this the denial could come from `default()` failing, not from the gate.
        assert!(!crate::is_handling_transaction());
        assert!(
            !check_permission(&kernel_parcel, "android.permission.INTERNET"),
            "outside a transaction the kernel path denies before PMS"
        );
    }

    /// Plan 17 K-23: the getter sees each install; a replaced authority's `Drop` panic is harmless.
    #[serial_test::serial(authority)]
    #[test]
    fn authority_getter_and_a_panicking_drop_of_the_replaced_one() {
        struct Fixed {
            grant: bool,
            panic_on_drop: bool,
        }
        impl PermissionAuthority for Fixed {
            fn check(&self, _: &str, _: &Caller) -> bool {
                self.grant
            }
        }
        impl Drop for Fixed {
            fn drop(&mut self) {
                if self.panic_on_drop {
                    panic!("injected authority Drop panic");
                }
            }
        }

        assert!(permission_authority().is_none());
        let first: Arc<dyn PermissionAuthority> = Arc::new(Fixed {
            grant: false,
            panic_on_drop: true,
        });
        assert!(replace_permission_authority(Some(first.clone())).is_none());
        let seen = permission_authority().expect("the getter sees the installed authority");
        assert!(Arc::ptr_eq(&seen, &first));
        // The slot now holds the last reference, so replacing it runs the panicking `Drop`.
        drop((seen, first));

        let second: Arc<dyn PermissionAuthority> = Arc::new(Fixed {
            grant: true,
            panic_on_drop: false,
        });
        let replaced = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            set_permission_authority(second.clone())
        }));
        assert!(replaced.is_err(), "the replaced authority's Drop panicked");
        assert!(
            !AUTHORITY.is_poisoned(),
            "the Drop ran after the write lock was released"
        );
        let seen = permission_authority().expect("the new authority is installed");
        assert!(Arc::ptr_eq(&seen, &second));

        // Not in a transaction: denies without consulting the authority, and without panicking.
        assert!(!check_permission(&Parcel::new(), "p"));
        #[cfg(feature = "rpc")]
        {
            let _g = crate::thread_state::RpcCallingGuard::install(
                Arc::new(crate::rpc::transport::PeerIdentity::Local { uid: 1000, pid: 7 }),
                UNIX_CAPS,
            );
            assert!(
                check_permission(&Parcel::new(), "p"),
                "the new authority decides"
            );
        }

        let last = replace_permission_authority(None).expect("the second authority");
        assert!(Arc::ptr_eq(&last, &second));
        assert!(permission_authority().is_none());
    }

    /// Plan 2-16 Phase C: an installed authority grants a Unix-RPC uid what the default denies.
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
        rpc_parcel.set_for_rpc(true).unwrap();

        // RPC uid 1000: the authority grants its one permission and denies everything else.
        {
            let _g = RpcCallingGuard::install(
                Arc::new(PeerIdentity::Local { uid: 1000, pid: 7 }),
                UNIX_CAPS,
            );
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
            let _g = RpcCallingGuard::install(
                Arc::new(PeerIdentity::Local { uid: 2000, pid: 7 }),
                UNIX_CAPS,
            );
            assert!(
                !check_permission(&rpc_parcel, "com.example.DO_THING"),
                "authority must deny a non-allowed uid"
            );
        }
        // No in-flight transaction ⇒ no caller ⇒ fail closed.
        assert!(!check_permission(&rpc_parcel, "com.example.DO_THING"));

        // Restore the default so other tests see kernel-PMS / RPC-deny.
        clear_permission_authority();
        let _g = RpcCallingGuard::install(
            Arc::new(PeerIdentity::Local { uid: 1000, pid: 7 }),
            UNIX_CAPS,
        );
        assert!(
            !check_permission(&rpc_parcel, "com.example.DO_THING"),
            "after clear, the default RPC deny is restored"
        );
    }
}
