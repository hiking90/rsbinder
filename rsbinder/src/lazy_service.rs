// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `LazyServiceRegistrar` skeleton.
//!
//! Mirrors AOSP `LazyServiceRegistrar`
//! ([`LazyServiceRegistrar.h`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/LazyServiceRegistrar.h),
//! [`LazyServiceRegistrar.cpp`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/LazyServiceRegistrar.cpp)):
//! a `register_service` helper that owns the bookkeeping for the
//! `aidl_lazy_service` pattern (idle unregister, force-persist, re-register
//! after onClients).
//!
//! **Scope note.** The state machine, AOSP-faithful `IClientCallback`
//! dispatch, and `force_persist`/`try_unregister`/`re_register` all live
//! in this module and are hermetically testable. The integration that
//! wires up the *actual* `registerClientCallback` / `tryUnregisterService`
//! IServiceManager calls is not yet wired and must be supplied by the
//! caller — the hub::ServiceManager wrapper does not yet surface those
//! AIDL methods despite the underlying generated bindings supporting them
//! since AOSP API 30 (Android 11). The present module exposes the
//! registrar surface as an in-process state machine; once hub plumbing
//! lands, `register_service` will additionally call
//! `service_manager.add_service(name, binder) +
//! service_manager.register_client_callback(name, binder, &self.as_callback())`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::binder::SIBinder;
use crate::error::Result;

/// One registered (name → binder) pair plus the most recent client-side
/// presence signal from `IClientCallback::onClients`.
#[derive(Clone)]
struct RegisteredService {
    /// Service name as registered with the service manager.
    name: String,
    /// The binder being lazily managed.
    binder: SIBinder,
    /// Last-known client-side presence: `true` once we have seen at
    /// least one `onClients(name, true)`, `false` after the matching
    /// `onClients(name, false)`. AOSP `ServiceInfo::clients`
    /// ([LazyServiceRegistrar.cpp:53](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/LazyServiceRegistrar.cpp;l=53)).
    has_clients: bool,
    /// Locked = registered with the service manager. `tryUnregisterLocked`
    /// flips this to `false` after a successful `tryUnregisterService`;
    /// `reRegisterLocked` flips it back to `true`.
    registered: bool,
}

struct Inner {
    /// Service name → tracking entry. AOSP's `mRegisteredServices` map.
    services: HashMap<String, RegisteredService>,
}

/// The lazy-service bookkeeping struct. Construct one per process via
/// [`LazyServiceRegistrar::new`] (or share via `Arc` across threads —
/// the struct is `Send + Sync`).
///
/// Threading: the `services` map is protected by a `Mutex`; the
/// `force_persist` flag is a lock-free `AtomicBool` (AOSP equivalent is
/// `mForcePersist` guarded by `mMutex` — rsbinder avoids the lock on
/// the hot path because the flag is consulted from inside `onClients`
/// where holding the registrar mutex is already required).
pub struct LazyServiceRegistrar {
    inner: Mutex<Inner>,
    /// When `true`, [`Self::try_unregister`] short-circuits to a no-op
    /// even if all services report `has_clients == false`. AOSP
    /// `forcePersist`.
    force_persist: AtomicBool,
}

impl LazyServiceRegistrar {
    /// Construct a fresh registrar. Caller normally wraps in `Arc` for
    /// sharing with the `IClientCallback` bridge.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                services: HashMap::new(),
            }),
            force_persist: AtomicBool::new(false),
        }
    }

    /// Record a (name, binder) pair as lazily managed. AOSP
    /// [`LazyServiceRegistrar::registerService`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/LazyServiceRegistrar.cpp;l=123),
    /// minus the IServiceManager `addService` + `registerClientCallback`
    /// IPC calls — those are owed by the **caller** until the hub
    /// integration lands (see the module-level scope note). Re-registering
    /// the same name replaces the prior entry (AOSP `mRegisteredServices`
    /// is keyed by name).
    pub fn register_service(&self, name: &str, binder: SIBinder) -> Result<()> {
        let mut inner = self.inner.lock().expect("lazy_service inner poisoned");
        inner.services.insert(
            name.to_string(),
            RegisteredService {
                name: name.to_string(),
                binder,
                has_clients: true, // AOSP initializes assuming a client may already be holding
                registered: true,
            },
        );
        Ok(())
    }

    /// Toggle the force-persist guard. AOSP
    /// `LazyServiceRegistrar::forcePersist`. When `true`,
    /// [`Self::try_unregister`] returns `false` without attempting an
    /// IServiceManager round trip.
    pub fn force_persist(&self, persist: bool) {
        self.force_persist.store(persist, Ordering::Release);
    }

    /// `true` if [`Self::force_persist`] was last called with `true`.
    pub fn is_force_persisted(&self) -> bool {
        self.force_persist.load(Ordering::Acquire)
    }

    /// AOSP-faithful `IClientCallback::onClients` dispatch. Call this
    /// from the (future-wired-up) Bn callback to update the registrar's
    /// internal `has_clients` state for a given service. Returns `true`
    /// if the named service was tracked, `false` if unknown (which AOSP
    /// would `LOG_ALWAYS_FATAL` on — rsbinder returns silently to avoid
    /// aborting the dispatch thread).
    pub fn on_clients(&self, name: &str, has_clients: bool) -> bool {
        let mut inner = self.inner.lock().expect("lazy_service inner poisoned");
        if let Some(entry) = inner.services.get_mut(name) {
            entry.has_clients = has_clients;
            true
        } else {
            false
        }
    }

    /// Attempt to unregister **every** service, all-or-nothing. AOSP
    /// [`LazyServiceRegistrar::tryUnregisterLocked`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/LazyServiceRegistrar.cpp;l=193).
    ///
    /// AOSP's caller (`maybeTryShutdownLocked`) only reaches this when
    /// `mNumConnectedServices == 0`, and `tryUnregisterLocked` then
    /// unregisters *all* services (rolling back on the first failure). A
    /// `true` return means "no service is registered any more → the process is
    /// safe to exit". A per-entry flip (leaving client-holding services
    /// registered while others go down) would let a caller following the
    /// AOSP idiom `if try_unregister() { exit }` terminate with live-client
    /// services still registered.
    ///
    /// **Returns** `true` iff every service was flipped to
    /// `registered = false` (there is no service with clients, and
    /// `force_persist` is not engaged); `false` otherwise — in which case
    /// nothing is changed. A registrar with no services returns `false`:
    /// there is nothing to unregister, so it never signals "safe to exit"
    /// (AOSP cannot reach this state — its shutdown check only runs from an
    /// `onClients` callback of a registered service). The hub-integrated
    /// version owed by the caller (see module docs) is where the actual
    /// `tryUnregisterService` IPC happens; this skeleton only mutates
    /// internal state.
    pub fn try_unregister(&self) -> bool {
        if self.force_persist.load(Ordering::Acquire) {
            return false;
        }
        let mut inner = self.inner.lock().expect("lazy_service inner poisoned");
        if inner.services.is_empty() {
            return false;
        }
        // Global gate: if any service still has clients, unregister nothing and
        // report "not safe to shut down" (AOSP all-or-nothing).
        if inner.services.values().any(|e| e.has_clients) {
            return false;
        }
        for entry in inner.services.values_mut() {
            entry.registered = false;
        }
        true
    }

    /// Re-register every previously-unregistered service. AOSP
    /// [`LazyServiceRegistrar::reRegisterLocked`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/LazyServiceRegistrar.cpp;l=209).
    /// The hub-integrated version replays `addService` +
    /// `registerClientCallback`; this skeleton flips state only.
    pub fn re_register(&self) {
        let mut inner = self.inner.lock().expect("lazy_service inner poisoned");
        for entry in inner.services.values_mut() {
            if !entry.registered {
                entry.registered = true;
                // AOSP also resets `has_clients` to `true` defensively
                // so the next `try_unregister` can't immediately unwind
                // the re-registration without an explicit `onClients`
                // notification.
                entry.has_clients = true;
            }
        }
    }

    /// Snapshot helper: returns `(name, has_clients, registered)` for
    /// every tracked service. Primarily for tests and introspection.
    pub fn snapshot(&self) -> Vec<(String, bool, bool)> {
        let inner = self.inner.lock().expect("lazy_service inner poisoned");
        inner
            .services
            .values()
            .map(|e| (e.name.clone(), e.has_clients, e.registered))
            .collect()
    }

    /// Snapshot helper: how many services are currently `registered`.
    pub fn registered_count(&self) -> usize {
        let inner = self.inner.lock().expect("lazy_service inner poisoned");
        inner.services.values().filter(|e| e.registered).count()
    }

    /// Snapshot helper: borrow the binder previously registered under
    /// `name`. Used by the hub-integrated re-register path (future) to
    /// pass the original `SIBinder` back into `addService`.
    pub fn binder_for(&self, name: &str) -> Option<SIBinder> {
        let inner = self.inner.lock().expect("lazy_service inner poisoned");
        inner.services.get(name).map(|e| e.binder.clone())
    }
}

impl Default for LazyServiceRegistrar {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::Stability;
    use crate::native::Binder;
    use crate::{Interface, Parcel, Remotable, Result as RsResult, TransactionCode};

    struct Dummy;
    impl Remotable for Dummy {
        fn descriptor() -> &'static str {
            "test.lazy_service.Dummy"
        }
        fn on_transact(&self, _: TransactionCode, _: &mut Parcel, _: &mut Parcel) -> RsResult<()> {
            Ok(())
        }
        fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> RsResult<()> {
            Ok(())
        }
    }

    fn fresh_binder() -> SIBinder {
        let b = Binder::new_with_stability(Dummy, Stability::Local);
        Interface::as_binder(&b)
    }

    /// `register_service` records the (name, binder) entry and marks it
    /// registered.
    #[test]
    fn register_records_entry() {
        let reg = LazyServiceRegistrar::new();
        reg.register_service("foo", fresh_binder()).unwrap();
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "foo");
        assert!(snap[0].2, "registered=true after register_service");
        assert_eq!(reg.registered_count(), 1);
    }

    /// `try_unregister` is all-or-nothing: if ANY service still has clients it
    /// unregisters nothing and returns `false`; only once every service is
    /// clientless does it flip them all and return `true`. AOSP
    /// `tryUnregisterLocked` (gated by the caller on `mNumConnectedServices ==
    /// 0`).
    #[test]
    fn try_unregister_is_all_or_nothing() {
        let reg = LazyServiceRegistrar::new();
        reg.register_service("a", fresh_binder()).unwrap();
        reg.register_service("b", fresh_binder()).unwrap();
        // `a` still has clients; `b` lost theirs → not safe to unregister
        // anything, and nothing is changed.
        reg.on_clients("b", false);
        assert!(!reg.try_unregister(), "a still has clients -> false, no-op");
        let snap = reg.snapshot();
        assert!(
            snap.iter().find(|(n, _, _)| n == "a").unwrap().2,
            "`a` stays registered"
        );
        assert!(
            snap.iter().find(|(n, _, _)| n == "b").unwrap().2,
            "`b` also stays registered"
        );
        assert_eq!(reg.registered_count(), 2);

        // Now `a` also loses its clients → all clientless → unregister all.
        reg.on_clients("a", false);
        assert!(reg.try_unregister(), "all clientless -> true");
        let snap = reg.snapshot();
        assert!(
            !snap.iter().find(|(n, _, _)| n == "a").unwrap().2,
            "`a` unregistered"
        );
        assert!(
            !snap.iter().find(|(n, _, _)| n == "b").unwrap().2,
            "`b` unregistered"
        );
        assert_eq!(reg.registered_count(), 0);
    }

    /// `force_persist(true)` short-circuits `try_unregister`
    /// even when eligible candidates exist. AOSP `mForcePersist`.
    #[test]
    fn force_persist_blocks_unregister() {
        let reg = LazyServiceRegistrar::new();
        reg.register_service("x", fresh_binder()).unwrap();
        reg.on_clients("x", false);
        reg.force_persist(true);
        assert!(reg.is_force_persisted());
        assert!(!reg.try_unregister(), "force_persist blocks");
        let snap = reg.snapshot();
        assert!(snap[0].2, "still registered");
    }

    /// `re_register` restores `registered = true`
    /// and defensively resets `has_clients = true` so the next
    /// `try_unregister` cannot immediately undo the re-registration
    /// without a fresh `onClients(false)`.
    #[test]
    fn re_register_restores_state_and_resets_clients() {
        let reg = LazyServiceRegistrar::new();
        reg.register_service("z", fresh_binder()).unwrap();
        reg.on_clients("z", false);
        reg.try_unregister();
        assert_eq!(reg.registered_count(), 0);
        reg.re_register();
        assert_eq!(reg.registered_count(), 1);
        // `try_unregister` immediately after re_register must be a
        // no-op (has_clients reset to true).
        assert!(!reg.try_unregister());
    }

    /// `on_clients` for an unknown service name silently returns
    /// `false` instead of aborting. AOSP `LOG_ALWAYS_FATAL_IF` on
    /// unknown service is intentionally softened for rsbinder.
    #[test]
    fn on_clients_for_unknown_service_is_silent() {
        let reg = LazyServiceRegistrar::new();
        assert!(!reg.on_clients("nope", true));
    }

    /// Re-registering a name replaces the prior entry — AOSP
    /// `mRegisteredServices` is name-keyed. The new binder's identity
    /// is what subsequent `binder_for` lookups must return.
    #[test]
    fn register_same_name_replaces() {
        let reg = LazyServiceRegistrar::new();
        let first = fresh_binder();
        let second = fresh_binder();
        reg.register_service("dup", first.clone()).unwrap();
        reg.register_service("dup", second.clone()).unwrap();
        let got = reg.binder_for("dup").unwrap();
        assert!(
            std::sync::Arc::ptr_eq(got.as_arc(), second.as_arc()),
            "second register_service wins"
        );
    }
}
