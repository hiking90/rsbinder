// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `LazyServiceRegistrar` — the `aidl_lazy_service` pattern.
//!
//! Mirrors AOSP `LazyServiceRegistrar`
//! ([`LazyServiceRegistrar.h`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/include/binder/LazyServiceRegistrar.h),
//! [`LazyServiceRegistrar.cpp`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/LazyServiceRegistrar.cpp)):
//! register a service, let the service manager report when clients come and
//! go, and shut the process down once nothing is using it.
//!
//! [`LazyServiceRegistrar::register_service`] performs the service-manager
//! calls itself — `addService` with `FLAG_IS_LAZY_SERVICE`, then
//! `registerClientCallback` with an `IClientCallback` this module owns. A
//! caller does not register a callback or route `onClients` by hand:
//!
//! ```no_run
//! use rsbinder::lazy_service::LazyServiceRegistrar;
//! use rsbinder::ProcessState;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let binder: rsbinder::SIBinder = unimplemented!();
//! // `register_service` talks to the service manager, so the kernel binder
//! // `ProcessState` has to exist first.
//! ProcessState::init_default()?;
//!
//! let registrar = LazyServiceRegistrar::new();
//! registrar.register_service("my.Service/default", binder)?;
//!
//! // Returns only if the thread pool is torn down; the usual exit is the
//! // registrar's own, from the callback thread.
//! ProcessState::join_thread_pool()?;
//! # Ok(())
//! # }
//! ```
//!
//! **The process exits when the last client goes away.** That is AOSP's
//! contract (`tryShutdownLocked` calls `exit(EXIT_SUCCESS)`), and the reason
//! the pattern exists: the service manager restarts the process on the next
//! lookup. Two ways to change it —
//! [`force_persist`](LazyServiceRegistrar::force_persist) to suspend shutdown
//! for a while, or
//! [`set_active_services_callback`](LazyServiceRegistrar::set_active_services_callback)
//! to take over the decision entirely.
//!
//! ## Deviations from AOSP
//!
//! * **Service-manager calls happen outside the registrar's lock.** AOSP
//!   holds `mMutex` across `tryUnregisterService`, but a service manager may
//!   answer that call by dispatching `onClients` back into this process, and
//!   the binder driver delivers such a nested transaction on the very thread
//!   that is waiting for the reply — which would deadlock on a non-reentrant
//!   `Mutex`. State is snapshotted under the lock, the calls are made
//!   without it, and the results are recorded on the way back. The service
//!   manager is the authority on whether an unregister may proceed, so
//!   nothing is lost by not holding it.
//! * **Nothing aborts.** AOSP `LOG_ALWAYS_FATAL`s on an `onClients` for an
//!   unknown service, on an `onClients` that repeats the state it already
//!   believed, and on a failed re-register. Each is logged and survived here.
//! * **Re-registering a name with a *different* binder replaces the entry**
//!   and registers a fresh client callback for it. AOSP keeps the first
//!   binder in `mRegisteredServices` while handing the new one to
//!   `addService`, which leaves the callback keyed on a binder the service
//!   manager no longer has under that name.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::binder::SIBinder;
use crate::error::{Result, StatusCode};
use crate::hub::{BnClientCallback, IClientCallback};
use crate::status::Status;
use crate::{Interface, Strong};

/// Reports whether any service in the process currently has clients, and
/// answers whether the shutdown was handled.
///
/// Returning `true` means "I took care of it" and suppresses the automatic
/// process exit; returning `false` leaves the registrar to shut down as
/// usual. Called only when the answer *changes*, so a callback sees
/// `true, false, true, …`, never the same value twice in a row. AOSP
/// `setActiveServicesCallback`.
pub type ActiveServicesCallback = Arc<dyn Fn(bool) -> bool + Send + Sync>;

/// The service-manager calls the registrar makes. Behind a trait so the
/// state machine is exercised without a binder device; the shipping
/// implementation is [`HubRegistry`].
trait Registry: Send + Sync {
    fn add_lazy_service(&self, name: &str, binder: &SIBinder) -> std::result::Result<(), Status>;
    fn register_client_callback(
        &self,
        name: &str,
        binder: &SIBinder,
        callback: &Strong<dyn IClientCallback>,
    ) -> std::result::Result<(), Status>;
    fn try_unregister_service(
        &self,
        name: &str,
        binder: &SIBinder,
    ) -> std::result::Result<(), Status>;
}

/// The default [`Registry`]: the process's default service manager.
struct HubRegistry;

impl Registry for HubRegistry {
    fn add_lazy_service(&self, name: &str, binder: &SIBinder) -> std::result::Result<(), Status> {
        crate::hub::add_lazy_service(name, binder.clone())
    }

    fn register_client_callback(
        &self,
        name: &str,
        binder: &SIBinder,
        callback: &Strong<dyn IClientCallback>,
    ) -> std::result::Result<(), Status> {
        crate::hub::register_client_callback(name, binder, callback).map_err(Status::from)
    }

    fn try_unregister_service(
        &self,
        name: &str,
        binder: &SIBinder,
    ) -> std::result::Result<(), Status> {
        crate::hub::try_unregister_service(name, binder).map_err(Status::from)
    }
}

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
    /// Registered with the service manager. `try_unregister` flips this to
    /// `false` after a successful `tryUnregisterService`; `re_register`
    /// flips it back.
    registered: bool,
}

/// AOSP's `std::map` is ordered, and unregister order is observable — a
/// `BTreeMap` keeps it deterministic.
struct Inner {
    services: BTreeMap<String, RegisteredService>,
    /// AOSP `mNumConnectedServices`: how many services have clients.
    num_connected_services: usize,
    /// AOSP `mPreviousHasClients`: the last value handed to the active
    /// services callback, so it only fires on a change.
    previous_has_clients: Option<bool>,
    active_services_callback: Option<ActiveServicesCallback>,
    /// The `IClientCallback` this registrar registered with the service
    /// manager. Held for the lifetime of the registrar: the service manager
    /// keeps only a proxy, so dropping this would kill every `onClients`.
    callback: Option<Strong<dyn IClientCallback>>,
}

impl Inner {
    /// AOSP `updateCacheClientCount`.
    fn update_cache_client_count(&mut self) {
        self.num_connected_services = self.services.values().filter(|e| e.has_clients).count();
    }
}

/// The registrar's state, shared with the `IClientCallback` bridge.
///
/// AOSP's `ClientCounterCallbackImpl` *is* the `BnClientCallback`, so the
/// state and the callback are one object held by `sp<>`. Rust splits them:
/// `Shared` holds the state and (transitively) the callback binder, and the
/// bridge holds a `Weak<Shared>` so the two do not keep each other alive.
struct Shared {
    inner: Mutex<Inner>,
    force_persist: AtomicBool,
    registry: Arc<dyn Registry>,
    /// Set instead of exiting, so the shutdown path is observable in tests.
    #[cfg(test)]
    exited: AtomicBool,
}

/// The `IClientCallback` the registrar registers on a service's behalf.
/// AOSP dispatches `onClients` on `ClientCounterCallbackImpl` itself.
struct ClientCounterCallback {
    shared: Weak<Shared>,
}

impl Interface for ClientCounterCallback {}

impl IClientCallback for ClientCounterCallback {
    fn onClients(&self, registered: &SIBinder, has_clients: bool) -> crate::status::Result<()> {
        // A callback that outlives its registrar is not an error: the
        // service manager may still hold the proxy after the last
        // `LazyServiceRegistrar` handle is dropped.
        if let Some(shared) = self.shared.upgrade() {
            shared.on_clients_binder(registered, has_clients);
        }
        Ok(())
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// AOSP `assertRegisteredService` — the wire hands `onClients` a binder,
    /// not a name.
    fn name_of(&self, binder: &SIBinder) -> Option<String> {
        self.lock()
            .services
            .values()
            .find(|e| e.binder == *binder)
            .map(|e| e.name.clone())
    }

    fn on_clients_binder(&self, binder: &SIBinder, has_clients: bool) {
        match self.name_of(binder) {
            Some(name) => {
                self.on_clients(&name, has_clients);
            }
            // AOSP `LOG_ALWAYS_FATAL`s here.
            None => log::error!("onClients for a service this registrar did not register"),
        }
    }

    /// AOSP `ClientCounterCallbackImpl::onClients`.
    fn on_clients(&self, name: &str, has_clients: bool) -> bool {
        {
            let mut inner = self.lock();
            let Some(entry) = inner.services.get_mut(name) else {
                return false;
            };
            if entry.has_clients == has_clients {
                // AOSP `LOG_ALWAYS_FATAL`s on a repeated state.
                log::warn!(
                    "{name}: onClients repeated has_clients={has_clients}; already believed that"
                );
            }
            entry.has_clients = has_clients;
            inner.update_cache_client_count();
            log::info!(
                "Process has {} (of {} available) client(s) in use after notification {name} has clients: {has_clients}",
                inner.num_connected_services,
                inner.services.len(),
            );
        }
        self.maybe_try_shutdown();
        true
    }

    /// AOSP `maybeTryShutdownLocked`. The active-services callback runs
    /// without the registrar lock held, so it may call back into the
    /// registrar (`force_persist`, `try_unregister`) without deadlocking.
    fn maybe_try_shutdown(&self) {
        if self.force_persist.load(Ordering::Acquire) {
            log::info!("Shutdown prevented by force_persist override flag.");
            return;
        }

        // Latch `previous_has_clients` under the lock so two threads
        // reporting the same transition cannot both fire the callback.
        let to_fire = {
            let mut inner = self.lock();
            let has_clients = inner.num_connected_services != 0;
            match &inner.active_services_callback {
                Some(cb) if inner.previous_has_clients != Some(has_clients) => {
                    let cb = cb.clone();
                    inner.previous_has_clients = Some(has_clients);
                    Some((cb, has_clients))
                }
                _ => None,
            }
        };

        let handled = match to_fire {
            Some((cb, has_clients)) => cb(has_clients),
            None => false,
        };

        // Re-read rather than reuse the value from above: the lock was
        // released for the callback. This is advisory either way — the
        // service manager refuses `tryUnregisterService` for a service that
        // has clients, so a stale `true` here costs a failed round trip and
        // a re-register, never a wrongly shut-down service.
        if !handled && self.lock().num_connected_services == 0 {
            self.try_shutdown();
        }
    }

    /// AOSP `tryShutdownLocked`.
    fn try_shutdown(&self) {
        log::info!(
            "Trying to shut down the service. No clients in use for any service in process."
        );
        if self.try_unregister() {
            log::info!("Unregistered all clients and exiting");
            self.exit_process();
            return;
        }
        self.re_register();
    }

    #[cfg(not(test))]
    fn exit_process(&self) {
        std::process::exit(0);
    }

    #[cfg(test)]
    fn exit_process(&self) {
        self.exited.store(true, Ordering::Release);
    }

    /// AOSP `tryUnregisterLocked`, with the round trips outside the lock.
    fn try_unregister(&self) -> bool {
        if self.force_persist.load(Ordering::Acquire) {
            return false;
        }

        let candidates: Vec<(String, SIBinder)> = {
            let inner = self.lock();
            inner
                .services
                .values()
                .filter(|e| e.registered)
                .map(|e| (e.name.clone(), e.binder.clone()))
                .collect()
        };

        // AOSP cannot reach an empty map here (its shutdown check only runs
        // from a registered service's `onClients`), but `try_unregister` is
        // public: a registrar with nothing in it must not report "every
        // service is down, safe to exit".
        if candidates.is_empty() {
            return false;
        }

        for (name, binder) in candidates {
            match self.registry.try_unregister_service(&name, &binder) {
                Ok(()) => {
                    if let Some(entry) = self.lock().services.get_mut(&name) {
                        entry.registered = false;
                    }
                }
                Err(e) => {
                    log::info!("Failed to unregister service {name} ({e:?})");
                    return false;
                }
            }
        }
        true
    }

    /// AOSP `reRegisterLocked`. Note what it does *not* do: `has_clients` is
    /// left alone, because the service manager's view of the clients did not
    /// change just because we failed to unregister.
    fn re_register(&self) {
        let pending: Vec<(String, SIBinder)> = {
            let inner = self.lock();
            inner
                .services
                .values()
                .filter(|e| !e.registered)
                .map(|e| (e.name.clone(), e.binder.clone()))
                .collect()
        };

        for (name, binder) in pending {
            // A re-register is `addService` only — the client callback is
            // registered once per (name, binder) and the service manager
            // still holds it.
            match self.registry.add_lazy_service(&name, &binder) {
                Ok(()) => {
                    if let Some(entry) = self.lock().services.get_mut(&name) {
                        entry.registered = true;
                    }
                }
                // AOSP `LOG_ALWAYS_FATAL`s: "clients will never be able to
                // get a hold of this service". Left `registered = false` so
                // a later `re_register` can try again instead of the process
                // dying with the state it had.
                Err(e) => log::error!("Bad state: could not re-register {name} ({e:?})"),
            }
        }
    }
}

/// Registers services that the process should be shut down for when nobody
/// is using them.
///
/// A cheap handle: cloning one shares the same registrations, as AOSP's
/// `LazyServiceRegistrar` shares its `ClientCounterCallback`. AOSP exposes a
/// process singleton (`getInstance`); construct one here and share it.
#[derive(Clone)]
pub struct LazyServiceRegistrar {
    shared: Arc<Shared>,
}

impl LazyServiceRegistrar {
    /// A registrar backed by the process's default service manager.
    pub fn new() -> Self {
        Self::with_registry(Arc::new(HubRegistry))
    }

    fn with_registry(registry: Arc<dyn Registry>) -> Self {
        LazyServiceRegistrar {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    services: BTreeMap::new(),
                    num_connected_services: 0,
                    previous_has_clients: None,
                    active_services_callback: None,
                    callback: None,
                }),
                force_persist: AtomicBool::new(false),
                registry,
                #[cfg(test)]
                exited: AtomicBool::new(false),
            }),
        }
    }

    /// The `IClientCallback` for this registrar, created on first use.
    fn callback(&self) -> Strong<dyn IClientCallback> {
        let mut inner = self.shared.lock();
        inner
            .callback
            .get_or_insert_with(|| {
                BnClientCallback::new_binder(ClientCounterCallback {
                    shared: Arc::downgrade(&self.shared),
                })
            })
            .clone()
    }

    /// Register `binder` under `name` and start tracking its clients.
    ///
    /// `addService` with `FLAG_IS_LAZY_SERVICE`, then — for a name not
    /// already tracked, or one tracked under a different binder —
    /// `registerClientCallback`. AOSP
    /// [`registerServiceLocked`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/LazyServiceRegistrar.cpp;l=129).
    ///
    /// The service starts out assumed to have clients, so an idle process
    /// cannot shut down before the service manager has said anything about
    /// it.
    pub fn register_service(&self, name: &str, binder: SIBinder) -> Result<()> {
        let tracked_same_binder = {
            let inner = self.shared.lock();
            inner.services.get(name).map(|e| e.binder == binder) == Some(true)
        };

        log::info!(
            "{} service {name}",
            if tracked_same_binder {
                "Re-registering"
            } else {
                "Registering"
            }
        );

        self.shared
            .registry
            .add_lazy_service(name, &binder)
            .map_err(|e| {
                log::error!("Failed to register service {name} ({e:?})");
                StatusCode::from(e)
            })?;

        if !tracked_same_binder {
            let callback = self.callback();
            self.shared
                .registry
                .register_client_callback(name, &binder, &callback)
                .map_err(|e| {
                    log::error!("Failed to add client callback for service {name} ({e:?})");
                    StatusCode::from(e)
                })?;
        }

        let mut inner = self.shared.lock();
        inner.services.insert(
            name.to_string(),
            RegisteredService {
                name: name.to_string(),
                binder,
                has_clients: true,
                registered: true,
            },
        );
        inner.update_cache_client_count();
        Ok(())
    }

    /// Suspend automatic shutdown. AOSP `forcePersist`.
    ///
    /// Turning it back off re-checks immediately, so a process that lost its
    /// last client while persisted shuts down as soon as it is released.
    pub fn force_persist(&self, persist: bool) {
        self.shared.force_persist.store(persist, Ordering::Release);
        if !persist {
            self.shared.maybe_try_shutdown();
        }
    }

    /// `true` if [`Self::force_persist`] was last called with `true`.
    pub fn is_force_persisted(&self) -> bool {
        self.shared.force_persist.load(Ordering::Acquire)
    }

    /// Decide for yourself what an idle process should do. AOSP
    /// `setActiveServicesCallback`.
    ///
    /// Set this **before** [`Self::register_service`] — otherwise a client
    /// that arrives and leaves in between is decided without it. See
    /// [`ActiveServicesCallback`] for what the argument and return value
    /// mean.
    pub fn set_active_services_callback(&self, callback: ActiveServicesCallback) {
        self.shared.lock().active_services_callback = Some(callback);
    }

    /// Record an `onClients` transition by service name.
    ///
    /// The registrar routes the service manager's own callbacks here; call
    /// it directly only to drive the state machine by hand. It runs the full
    /// AOSP dispatch, **process exit included**. Returns `false` for a name
    /// this registrar did not register (AOSP `LOG_ALWAYS_FATAL`s).
    pub fn on_clients(&self, name: &str, has_clients: bool) -> bool {
        self.shared.on_clients(name, has_clients)
    }

    /// Unregister every service, all-or-nothing. AOSP
    /// [`tryUnregisterLocked`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/LazyServiceRegistrar.cpp;l=193).
    ///
    /// `true` means nothing is registered any more and the process is safe to
    /// exit. `false` means the service manager refused at least one — some
    /// service still has clients — and [`Self::re_register`] is owed for the
    /// ones that did come down. AOSP calls this only from the active services
    /// callback.
    ///
    /// A registrar with no services returns `false`: there is nothing to take
    /// down, so it never signals "safe to exit".
    pub fn try_unregister(&self) -> bool {
        self.shared.try_unregister()
    }

    /// Put back every service a [`Self::try_unregister`] took down. AOSP
    /// [`reRegisterLocked`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/LazyServiceRegistrar.cpp;l=209).
    pub fn re_register(&self) {
        self.shared.re_register()
    }

    /// `(name, has_clients, registered)` for every tracked service.
    pub fn snapshot(&self) -> Vec<(String, bool, bool)> {
        let inner = self.shared.lock();
        inner
            .services
            .values()
            .map(|e| (e.name.clone(), e.has_clients, e.registered))
            .collect()
    }

    /// How many services are currently registered.
    pub fn registered_count(&self) -> usize {
        let inner = self.shared.lock();
        inner.services.values().filter(|e| e.registered).count()
    }

    /// The binder registered under `name`.
    pub fn binder_for(&self, name: &str) -> Option<SIBinder> {
        let inner = self.shared.lock();
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
    use crate::{Parcel, Remotable, Result as RsResult, TransactionCode};

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

    /// A service manager that records what it was asked and can be told to
    /// refuse an unregister — which is how a real one reports "this service
    /// still has clients".
    #[derive(Default)]
    struct FakeRegistry {
        calls: Mutex<Vec<String>>,
        refuse_unregister: Mutex<Vec<String>>,
    }

    impl FakeRegistry {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn refuse(&self, name: &str) {
            self.refuse_unregister
                .lock()
                .unwrap()
                .push(name.to_string());
        }
    }

    impl Registry for FakeRegistry {
        fn add_lazy_service(
            &self,
            name: &str,
            _binder: &SIBinder,
        ) -> std::result::Result<(), Status> {
            self.calls.lock().unwrap().push(format!("add:{name}"));
            Ok(())
        }
        fn register_client_callback(
            &self,
            name: &str,
            _binder: &SIBinder,
            _callback: &Strong<dyn IClientCallback>,
        ) -> std::result::Result<(), Status> {
            self.calls.lock().unwrap().push(format!("cb:{name}"));
            Ok(())
        }
        fn try_unregister_service(
            &self,
            name: &str,
            _binder: &SIBinder,
        ) -> std::result::Result<(), Status> {
            self.calls.lock().unwrap().push(format!("unreg:{name}"));
            if self
                .refuse_unregister
                .lock()
                .unwrap()
                .iter()
                .any(|n| n == name)
            {
                return Err(Status::new_service_specific_error(-1, None));
            }
            Ok(())
        }
    }

    fn fixture() -> (LazyServiceRegistrar, Arc<FakeRegistry>) {
        let registry = Arc::new(FakeRegistry::default());
        (
            LazyServiceRegistrar::with_registry(registry.clone()),
            registry,
        )
    }

    /// `register_service` makes both service-manager calls, in AOSP's order.
    #[test]
    fn register_calls_add_service_then_client_callback() {
        let (reg, registry) = fixture();
        reg.register_service("foo", fresh_binder()).unwrap();
        assert_eq!(registry.calls(), vec!["add:foo", "cb:foo"]);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "foo");
        assert!(snap[0].2, "registered=true after register_service");
        assert_eq!(reg.registered_count(), 1);
    }

    /// A re-register of the *same* binder is `addService` only — the service
    /// manager still holds the client callback. AOSP `registerServiceLocked`
    /// guards `registerClientCallback` on `!reRegister`.
    #[test]
    fn re_registering_same_binder_does_not_re_register_the_callback() {
        let (reg, registry) = fixture();
        let binder = fresh_binder();
        reg.register_service("foo", binder.clone()).unwrap();
        reg.register_service("foo", binder).unwrap();
        assert_eq!(registry.calls(), vec!["add:foo", "cb:foo", "add:foo"]);
    }

    /// A different binder under the same name is a fresh registration: the
    /// callback the service manager holds is keyed on the old binder.
    #[test]
    fn re_registering_different_binder_registers_a_new_callback() {
        let (reg, registry) = fixture();
        let second = fresh_binder();
        reg.register_service("dup", fresh_binder()).unwrap();
        reg.register_service("dup", second.clone()).unwrap();
        assert_eq!(
            registry.calls(),
            vec!["add:dup", "cb:dup", "add:dup", "cb:dup"]
        );
        let got = reg.binder_for("dup").unwrap();
        assert!(
            std::sync::Arc::ptr_eq(got.as_arc(), second.as_arc()),
            "second register_service wins"
        );
    }

    /// The last client leaving takes the process down with it — AOSP
    /// `tryShutdownLocked` → `exit(EXIT_SUCCESS)`.
    #[test]
    fn last_client_leaving_unregisters_and_exits() {
        let (reg, registry) = fixture();
        reg.register_service("solo", fresh_binder()).unwrap();
        reg.on_clients("solo", false);
        assert!(registry.calls().contains(&"unreg:solo".to_string()));
        assert!(reg.shared.exited.load(Ordering::Acquire), "process exits");
        assert_eq!(reg.registered_count(), 0);
    }

    /// One service losing its clients while another still has them is not a
    /// shutdown. AOSP gates on `mNumConnectedServices == 0`.
    #[test]
    fn shutdown_waits_for_every_service() {
        let (reg, registry) = fixture();
        reg.register_service("a", fresh_binder()).unwrap();
        reg.register_service("b", fresh_binder()).unwrap();

        reg.on_clients("b", false);
        assert!(
            !registry.calls().iter().any(|c| c.starts_with("unreg:")),
            "`a` still has clients -> nothing unregistered"
        );
        assert!(!reg.shared.exited.load(Ordering::Acquire));
        assert_eq!(reg.registered_count(), 2);

        reg.on_clients("a", false);
        assert!(reg.shared.exited.load(Ordering::Acquire));
        assert_eq!(reg.registered_count(), 0);
    }

    /// A service manager that refuses an unregister — a client appeared
    /// between the callback and the round trip — puts everything back.
    #[test]
    fn a_refused_unregister_re_registers_and_does_not_exit() {
        let (reg, registry) = fixture();
        reg.register_service("a", fresh_binder()).unwrap();
        reg.register_service("b", fresh_binder()).unwrap();
        registry.refuse("b");

        reg.on_clients("a", false);
        reg.on_clients("b", false);

        assert!(!reg.shared.exited.load(Ordering::Acquire), "no exit");
        assert_eq!(reg.registered_count(), 2, "`a` is put back");
        assert!(registry.calls().contains(&"unreg:a".to_string()));
        // `a` came down, `b` refused, so `a` is re-added — and without a
        // second client callback.
        assert_eq!(
            registry.calls().iter().filter(|c| *c == "add:a").count(),
            2,
            "`a` re-registered"
        );
        assert_eq!(
            registry.calls().iter().filter(|c| *c == "cb:a").count(),
            1,
            "callback registered once"
        );
    }

    /// `re_register` leaves `has_clients` alone — AOSP `reRegisterLocked`
    /// only touches `registered`. Resetting it would hide the very state the
    /// service manager just reported.
    #[test]
    fn re_register_does_not_invent_clients() {
        let (reg, _registry) = fixture();
        reg.register_service("z", fresh_binder()).unwrap();
        reg.force_persist(true);
        reg.on_clients("z", false);
        assert!(!reg.try_unregister(), "force_persist blocks");

        reg.force_persist(false);
        // Releasing force_persist re-checks and shuts down.
        assert!(reg.shared.exited.load(Ordering::Acquire));
        reg.re_register();
        assert_eq!(reg.registered_count(), 1);
        let snap = reg.snapshot();
        assert!(!snap[0].1, "has_clients stays false after re_register");
    }

    /// `force_persist(true)` blocks the shutdown entirely.
    #[test]
    fn force_persist_blocks_unregister() {
        let (reg, registry) = fixture();
        reg.register_service("x", fresh_binder()).unwrap();
        reg.force_persist(true);
        assert!(reg.is_force_persisted());
        reg.on_clients("x", false);
        assert!(!reg.try_unregister(), "force_persist blocks");
        assert!(!registry.calls().iter().any(|c| c.starts_with("unreg:")));
        assert!(!reg.shared.exited.load(Ordering::Acquire));
        assert!(reg.snapshot()[0].2, "still registered");
    }

    /// An active-services callback that returns `true` owns the decision;
    /// the registrar does not unregister or exit behind its back.
    #[test]
    fn active_services_callback_can_take_over() {
        let (reg, registry) = fixture();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        reg.set_active_services_callback(Arc::new(move |has_clients| {
            sink.lock().unwrap().push(has_clients);
            true
        }));
        reg.register_service("x", fresh_binder()).unwrap();
        reg.on_clients("x", false);

        assert_eq!(*seen.lock().unwrap(), vec![false]);
        assert!(
            !reg.shared.exited.load(Ordering::Acquire),
            "callback owns it"
        );
        assert!(!registry.calls().iter().any(|c| c.starts_with("unreg:")));
        assert_eq!(reg.registered_count(), 1);
    }

    /// The callback fires only when the answer changes. AOSP
    /// `mPreviousHasClients`.
    #[test]
    fn active_services_callback_fires_only_on_change() {
        let (reg, _registry) = fixture();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        reg.set_active_services_callback(Arc::new(move |has_clients| {
            sink.lock().unwrap().push(has_clients);
            true
        }));
        reg.register_service("x", fresh_binder()).unwrap();

        reg.on_clients("x", false);
        reg.on_clients("x", true);
        reg.on_clients("x", true); // repeated — no transition
        reg.on_clients("x", false);

        assert_eq!(*seen.lock().unwrap(), vec![false, true, false]);
    }

    /// `on_clients` for an unknown service name returns `false` instead of
    /// aborting. AOSP `LOG_ALWAYS_FATAL_IF`.
    #[test]
    fn on_clients_for_unknown_service_is_silent() {
        let (reg, _registry) = fixture();
        assert!(!reg.on_clients("nope", true));
    }

    /// A registrar with nothing in it never reports "safe to exit".
    #[test]
    fn empty_registrar_never_reports_safe_to_exit() {
        let (reg, _registry) = fixture();
        assert!(!reg.try_unregister());
    }

    /// The `IClientCallback` bridge resolves the binder the wire gives it
    /// back to the name it was registered under.
    #[test]
    fn client_callback_routes_by_binder_identity() {
        let (reg, registry) = fixture();
        let a = fresh_binder();
        let b = fresh_binder();
        reg.register_service("a", a.clone()).unwrap();
        reg.register_service("b", b.clone()).unwrap();

        reg.shared.on_clients_binder(&b, false);
        assert!(!reg.snapshot().iter().find(|s| s.0 == "b").unwrap().1);
        assert!(reg.snapshot().iter().find(|s| s.0 == "a").unwrap().1);
        assert!(!registry.calls().iter().any(|c| c.starts_with("unreg:")));

        // A binder this registrar never registered is logged, not fatal.
        reg.shared.on_clients_binder(&fresh_binder(), false);
        assert!(!reg.shared.exited.load(Ordering::Acquire));
    }

    /// Clones share one set of registrations, as AOSP's handle shares its
    /// `ClientCounterCallback`.
    #[test]
    fn clones_share_state() {
        let (reg, _registry) = fixture();
        let other = reg.clone();
        reg.register_service("shared", fresh_binder()).unwrap();
        assert_eq!(other.registered_count(), 1);
        assert!(other.binder_for("shared").is_some());
    }
}
