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
//! // `onClients` is an inbound transaction, so the pool has to be running.
//! ProcessState::start_thread_pool();
//!
//! // The process-wide registrar: it has to outlive what it registers.
//! LazyServiceRegistrar::instance().register_service("my.Service/default", binder)?;
//!
//! // Returns only if the thread pool is torn down; the usual exit is the
//! // registrar's own, from the binder thread that took the `onClients`.
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
//!   holds `mMutex` across `tryUnregisterService`. It can: `IClientCallback`
//!   is a `oneway` interface, so a notification the service manager sends
//!   while answering that call is queued to this process and picked up by a
//!   binder pool thread — never delivered as a nested transaction on the
//!   thread waiting for the reply — and holding the lock would just block
//!   that pool thread for the length of the round trip. This port releases
//!   it anyway, so a notification arriving mid-round-trip is recorded when
//!   it arrives instead of queueing behind the call it answers. What AOSP's
//!   one mutex also buys — no two service-manager sequences interleaving —
//!   is kept by a second lock (`Shared::ops`) that spans a whole
//!   `register_service` or shutdown decision but is never held for the
//!   bookkeeping half of `onClients`.
//! * **Nothing aborts.** AOSP `LOG_ALWAYS_FATAL`s on an `onClients` for an
//!   unknown service, on an `onClients` that repeats the state it already
//!   believed, and on a failed re-register. Each is logged and survived here.
//! * **`register_service` is not atomic.** `addService` goes first, so a
//!   failure to register the client callback after it leaves the service
//!   registered with the service manager but untracked here — visible to
//!   clients, never shutting down. Call
//!   [`hub::try_unregister_service`](crate::hub::try_unregister_service) to
//!   undo that, or retry (a retry re-runs both calls). **Android 10 is
//!   refused before anything is published**: its service manager has
//!   neither `registerClientCallback` nor `tryUnregisterService`, so a
//!   half-registration there could never be undone. `register_service`
//!   reports `EX_UNSUPPORTED_OPERATION` without calling `addService`, and
//!   lazy services need Android 11 or newer.
//! * **Re-registering a name with a *different* binder replaces the entry**
//!   (carrying its client state forward, as the service manager does). AOSP
//!   keeps the first binder in `mRegisteredServices` while handing the new
//!   one to `addService`, so its own `onClients` — which carries the binder
//!   the service manager currently holds — then matches nothing and trips
//!   `LOG_ALWAYS_FATAL`. Replacing keeps the lookup working. The client
//!   callback is *not* re-registered either way: the service manager stores
//!   callbacks by name and does not de-duplicate them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::binder::SIBinder;
use crate::hub::{BnClientCallback, IClientCallback};
use crate::status::Status;
use crate::{Interface, Strong};

/// Reports whether any service in the process currently has clients, and
/// answers whether the shutdown was handled.
///
/// Returning `true` means "I took care of it" and suppresses the automatic
/// process exit; returning `false` leaves the registrar to shut down as
/// usual. Called only when the answer *changes* — never the same value
/// twice in a row (the first call may be `false`). AOSP
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
        crate::hub::register_client_callback_status(name, binder, callback)
    }

    fn try_unregister_service(
        &self,
        name: &str,
        binder: &SIBinder,
    ) -> std::result::Result<(), Status> {
        crate::hub::try_unregister_service_status(name, binder)
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

/// Registrar state; the `IClientCallback` bridge holds it as `Weak` (AOSP fuses both into one `sp<>`).
struct Shared {
    /// Serializes SM round trips (AOSP `mMutex`); taken before `inner`, never while holding it.
    ops: Mutex<()>,
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
        // Not an error: the service manager may still hold the proxy
        // after the last `LazyServiceRegistrar` handle is dropped.
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
                // AOSP `LOG_ALWAYS_FATAL`s here; a re-registered name can
                // legitimately repeat the state it carried forward.
                log::debug!("{name}: onClients({has_clients}) matched what we already believed");
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

    /// AOSP `maybeTryShutdownLocked`.
    fn maybe_try_shutdown(&self) {
        if self.force_persist.load(Ordering::Acquire) {
            log::info!("Shutdown prevented by force_persist override flag.");
            return;
        }
        // Waits out a `register_service` or another thread's shutdown. AOSP
        // is already holding the equivalent when it gets here.
        let _ops = self.ops.lock().unwrap_or_else(|e| e.into_inner());

        // AOSP `mPreviousHasClients`: fire only when the answer changes.
        // Cloned out so the callback runs without the state lock — it is
        // documented to call back into the registrar.
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

        // Re-read: the lock was released for the callback. Advisory — the
        // service manager refuses an unregister while clients hold it.
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

        // `try_unregister` is public, and a registrar with nothing in it
        // must not report "every service is down, safe to exit".
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

    /// Put the entry back the way it was, for a `register_service` that
    /// published it and then could not complete.
    fn restore(&self, name: &str, previous: Option<RegisteredService>) {
        let mut inner = self.lock();
        match previous {
            Some(mut entry) => {
                // Keep what changed under us: the service manager repeats neither `onClients` nor an unregister.
                if let Some(current) = inner.services.get(name) {
                    entry.has_clients = current.has_clients;
                    // Only a `false` written under us is real; `true` is this call's own optimistic publish.
                    if !current.registered {
                        entry.registered = false;
                    }
                }
                inner.services.insert(name.to_string(), entry)
            }
            None => inner.services.remove(name),
        };
        inner.update_cache_client_count();
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
            // `addService` only: the callback is registered once per name.
            match self.registry.add_lazy_service(&name, &binder) {
                Ok(()) => {
                    if let Some(entry) = self.lock().services.get_mut(&name) {
                        entry.registered = true;
                    }
                }
                // AOSP `LOG_ALWAYS_FATAL`s. Left `registered = false` so a
                // later `re_register` can try again.
                Err(e) => log::error!("Bad state: could not re-register {name} ({e:?})"),
            }
        }
    }
}

/// Registers services that the process should be shut down for when nobody
/// is using them.
///
/// A cheap handle: cloning one shares the same registrations, as AOSP's
/// `LazyServiceRegistrar` shares its `ClientCounterCallback`.
///
/// **A registrar must outlive the services it registers.** Reach for
/// [`instance`](Self::instance) unless you have a reason not to — see
/// [`new`](Self::new) for what dropping the last handle costs.
#[derive(Clone)]
pub struct LazyServiceRegistrar {
    shared: Arc<Shared>,
}

impl LazyServiceRegistrar {
    /// The process-wide registrar. AOSP
    /// [`LazyServiceRegistrar::getInstance`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/LazyServiceRegistrar.cpp;l=329),
    /// which is likewise a singleton that is never freed.
    ///
    /// Use this for an ordinary lazy service. Registering through a
    /// registrar that lives as long as the process is what makes the
    /// shutdown reachable at all.
    pub fn instance() -> &'static LazyServiceRegistrar {
        static INSTANCE: std::sync::OnceLock<LazyServiceRegistrar> = std::sync::OnceLock::new();
        INSTANCE.get_or_init(LazyServiceRegistrar::new)
    }

    /// A fresh registrar backed by the process's default service manager.
    /// AOSP `createExtraTestInstance`.
    ///
    /// **Keep it alive for as long as its services are registered.** The
    /// `IClientCallback` binder outlives the registrar — the service manager
    /// holds a reference, so the kernel keeps the local object pinned — but
    /// its link back to the bookkeeping is weak. Drop the last handle and
    /// the service manager goes on calling a callback that does nothing:
    /// the services stay registered, the process never shuts down, and
    /// nothing says so. [`instance`](Self::instance) has no such edge.
    pub fn new() -> Self {
        Self::with_registry(Arc::new(HubRegistry))
    }

    fn with_registry(registry: Arc<dyn Registry>) -> Self {
        LazyServiceRegistrar {
            shared: Arc::new(Shared {
                ops: Mutex::new(()),
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
    /// already tracked — `registerClientCallback`. AOSP
    /// [`registerServiceLocked`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/LazyServiceRegistrar.cpp;l=129).
    ///
    /// A new registration starts out with no clients, as AOSP's
    /// `Service::clients` does — the service manager reports a client only
    /// once one arrives, and never repeats itself, so assuming one here
    /// would be an assumption nothing ever takes back. Re-registering a name
    /// already tracked carries the reported state forward whatever binder
    /// comes with it, for the same reason: the service manager carries it
    /// across a re-`addService` and will not re-announce it.
    ///
    /// An `onClients` arriving on a pool thread while this call is in
    /// flight is recorded at once but cannot act until the call is done, so
    /// it never decides on a service count the service manager does not yet
    /// agree with. AOSP gets the same exclusion from holding one mutex
    /// across its round trips.
    ///
    /// **A shutdown that was waiting on this call runs the moment it
    /// returns, and shutting down means [`std::process::exit`].** Registering
    /// several services one after another can therefore end mid-loop, with
    /// no error and nothing after the call reached. That is the lazy
    /// contract working, not a failure — but when every service has to be up
    /// before any of them may take the process down, wrap the batch in
    /// [`force_persist(true)`](Self::force_persist) and release it at the
    /// end.
    ///
    /// # Errors
    ///
    /// The service manager's own [`Status`] — `EX_SECURITY` for a policy
    /// refusal, and so on. Returns before `addService` on Android 10, whose
    /// service manager cannot support the lazy contract at all.
    pub fn register_service(
        &self,
        name: &str,
        binder: impl Into<SIBinder>,
    ) -> std::result::Result<(), Status> {
        let binder = binder.into();
        // Held for the whole call, as AOSP holds `mMutex` across
        // `registerServiceLocked`.
        let _ops = self.shared.ops.lock().unwrap_or_else(|e| e.into_inner());

        // Published before the round trips: `onClients` can land on a pool thread while they are outstanding.
        let previous = {
            let mut inner = self.shared.lock();
            let previous = inner.services.get(name).cloned();
            inner.services.insert(
                name.to_string(),
                RegisteredService {
                    name: name.to_string(),
                    binder: binder.clone(),
                    has_clients: previous.as_ref().is_some_and(|e| e.has_clients),
                    registered: true,
                },
            );
            inner.update_cache_client_count();
            previous
        };
        // Everything below turns on whether the *name* is already tracked:
        // the service manager keys client callbacks on the name alone.
        let tracked = previous.is_some();

        log::info!(
            "{} service {name}",
            if tracked {
                "Re-registering"
            } else {
                "Registering"
            }
        );

        if let Err(e) = self.shared.registry.add_lazy_service(name, &binder) {
            log::error!("Failed to register service {name} ({e:?})");
            self.shared.restore(name, previous);
            return Err(e);
        }

        // A second callback registration would double every later `onClients` (AOSP guards on `!reRegister`).
        if !tracked {
            let callback = self.callback();
            if let Err(e) = self
                .shared
                .registry
                .register_client_callback(name, &binder, &callback)
            {
                log::error!("Failed to add client callback for service {name} ({e:?})");
                self.shared.restore(name, previous);
                return Err(e);
            }
        }

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
    ///
    /// The callback runs inside the shutdown decision it is answering, so
    /// [`try_unregister`](Self::try_unregister) and
    /// [`re_register`](Self::re_register) may be called from it — those are
    /// what it is *for* — but [`register_service`](Self::register_service),
    /// [`on_clients`](Self::on_clients) and
    /// [`force_persist`](Self::force_persist) may not: all three re-enter the
    /// lock the decision holds and would deadlock (`force_persist(false)`
    /// re-runs the decision itself). AOSP has the same rule for the same
    /// reason (`mActiveServicesCallback` is invoked under `mMutex`, while
    /// its `tryUnregister` / `reRegister` deliberately do not re-take it and
    /// `forcePersist` does).
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
    /// Two more cases return `false` without calling the service manager at
    /// all, so nothing came down and no [`Self::re_register`] is owed: a
    /// registrar with no services (there is nothing to take down, so it never
    /// signals "safe to exit"), and one with
    /// [`force_persist`](Self::force_persist) set. AOSP `tryUnregisterLocked`
    /// has no `forcePersist` check — it is reached only through
    /// `maybeTryShutdownLocked`, which already made it; this one is public
    /// and can be called directly.
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
    use std::sync::atomic::AtomicUsize;
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// Spin until `f` holds. The notification a fake sends runs on its own
    /// thread (`onClients` is `oneway`); the fake waits here for its
    /// bookkeeping half to land, which needs no lock the round trip holds.
    fn spin_until(mut f: impl FnMut() -> bool) {
        for _ in 0..1_000_000 {
            if f() {
                return;
            }
            std::thread::yield_now();
        }
        panic!("the notification never landed");
    }

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
        /// The binder each call carried, in order. The name alone would not
        /// catch sending the service manager a stale binder — which is what
        /// strands `onClients`, since the reply carries the binder back.
        binders: Mutex<Vec<(String, SIBinder)>>,
        /// Every `IClientCallback` the registrar handed over. The service
        /// manager keeps only a proxy, so the registrar has to keep the
        /// object alive itself — one object for the whole registrar.
        callbacks: Mutex<Vec<Strong<dyn IClientCallback>>>,
        refuse_unregister: Mutex<Vec<String>>,
    }

    impl FakeRegistry {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        /// The `IClientCallback` handed to the last `registerClientCallback`.
        fn last_callback(&self) -> Strong<dyn IClientCallback> {
            self.callbacks.lock().unwrap().last().unwrap().clone()
        }
        /// The binder of the last `call` (`"add:x"`, `"cb:x"`, `"unreg:x"`).
        fn last_binder(&self, call: &str) -> SIBinder {
            self.binders
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(c, _)| c == call)
                .unwrap_or_else(|| panic!("no {call} call recorded"))
                .1
                .clone()
        }
        fn record(&self, call: String, binder: &SIBinder) {
            self.binders
                .lock()
                .unwrap()
                .push((call.clone(), binder.clone()));
            self.calls.lock().unwrap().push(call);
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
            binder: &SIBinder,
        ) -> std::result::Result<(), Status> {
            self.record(format!("add:{name}"), binder);
            Ok(())
        }
        fn register_client_callback(
            &self,
            name: &str,
            binder: &SIBinder,
            callback: &Strong<dyn IClientCallback>,
        ) -> std::result::Result<(), Status> {
            self.callbacks.lock().unwrap().push(callback.clone());
            self.record(format!("cb:{name}"), binder);
            Ok(())
        }
        fn try_unregister_service(
            &self,
            name: &str,
            binder: &SIBinder,
        ) -> std::result::Result<(), Status> {
            self.record(format!("unreg:{name}"), binder);
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

    /// A re-register keeps the client state the service manager reported.
    /// Overwriting it strands the process: the service manager says
    /// `onClients` only on a change, so nothing would ever correct a
    /// `has_clients` invented here.
    #[test]
    fn re_registering_same_binder_keeps_the_reported_client_state() {
        let (reg, _registry) = fixture();
        let binder = fresh_binder();
        reg.register_service("keep", binder.clone()).unwrap();
        reg.on_clients("keep", true);

        reg.register_service("keep", binder).unwrap();
        assert!(
            reg.snapshot()[0].1,
            "a re-register must not throw away the reported client"
        );
        assert!(
            !reg.shared.exited.load(Ordering::Acquire),
            "a service with a client is not idle"
        );

        // The proof it matters: the client leaving must still be the
        // transition that shuts the process down.
        reg.on_clients("keep", false);
        assert!(reg.shared.exited.load(Ordering::Acquire));
    }

    /// A different binder under the same name replaces the entry but does
    /// **not** register a second client callback. The service manager keys
    /// callbacks by name and de-duplicates nothing (AOSP
    /// `ServiceManager.cpp` `mNameToClientCallback[name].push_back(cb)`), so
    /// a second registration would have it deliver every later `onClients`
    /// twice.
    #[test]
    fn re_registering_different_binder_does_not_register_a_second_callback() {
        let (reg, registry) = fixture();
        let second = fresh_binder();
        reg.register_service("dup", fresh_binder()).unwrap();
        reg.register_service("dup", second.clone()).unwrap();
        assert_eq!(registry.calls(), vec!["add:dup", "cb:dup", "add:dup"]);
        assert_eq!(
            registry.last_binder("add:dup"),
            second,
            "the re-`addService` must carry the new binder"
        );

        // The entry must still follow the new binder: `onClients` carries
        // the binder the service manager currently holds, and it is looked
        // up by identity.
        let got = reg.binder_for("dup").unwrap();
        assert!(
            std::sync::Arc::ptr_eq(got.as_arc(), second.as_arc()),
            "second register_service wins"
        );
        reg.shared.on_clients_binder(&second, true);
        assert!(
            reg.snapshot()[0].1,
            "onClients for the new binder must resolve to this entry"
        );
    }

    /// The last client leaving takes the process down with it — AOSP
    /// `tryShutdownLocked` → `exit(EXIT_SUCCESS)`.
    #[test]
    fn last_client_leaving_unregisters_and_exits() {
        let (reg, registry) = fixture();
        let binder = fresh_binder();
        reg.register_service("solo", binder.clone()).unwrap();
        reg.on_clients("solo", true);
        assert!(!reg.shared.exited.load(Ordering::Acquire), "still in use");

        reg.on_clients("solo", false);
        assert!(registry.calls().contains(&"unreg:solo".to_string()));
        assert_eq!(
            registry.last_binder("unreg:solo"),
            binder,
            "tryUnregisterService must name the binder it registered"
        );
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
        reg.on_clients("a", true);
        reg.on_clients("b", true);

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
        reg.on_clients("a", true);
        reg.on_clients("b", true);
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
    /// only touches `registered`. Resetting it either way would hide the
    /// state the service manager just reported, so both directions are
    /// pinned: a service that came down without clients stays without, and
    /// one the service manager refused *because* it has clients keeps them.
    #[test]
    fn re_register_does_not_invent_clients() {
        let (reg, _registry) = fixture();
        reg.register_service("z", fresh_binder()).unwrap();
        reg.on_clients("z", true);
        reg.force_persist(true);
        reg.on_clients("z", false);
        assert!(!reg.try_unregister(), "force_persist blocks");

        reg.force_persist(false);
        // Releasing force_persist re-checks and shuts down.
        assert!(reg.shared.exited.load(Ordering::Acquire));
        reg.re_register();
        assert_eq!(reg.registered_count(), 1);
        assert!(!reg.snapshot()[0].1, "has_clients stays false");

        // The other direction: `b` is refused because it still has clients,
        // so `a` comes down and goes back up while `b` keeps them.
        let (reg, registry) = fixture();
        reg.register_service("a", fresh_binder()).unwrap();
        reg.register_service("b", fresh_binder()).unwrap();
        reg.on_clients("a", true);
        reg.on_clients("b", true);
        registry.refuse("b");

        assert!(!reg.try_unregister(), "the service manager refused `b`");
        reg.re_register();
        assert_eq!(reg.registered_count(), 2);
        assert!(
            reg.snapshot().iter().all(|s| s.1),
            "re_register must not clear the clients the service manager reported"
        );
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
        reg.shared.on_clients_binder(&a, true);
        reg.shared.on_clients_binder(&b, true);

        reg.shared.on_clients_binder(&b, false);
        assert!(!reg.snapshot().iter().find(|s| s.0 == "b").unwrap().1);
        assert!(reg.snapshot().iter().find(|s| s.0 == "a").unwrap().1);
        assert!(!registry.calls().iter().any(|c| c.starts_with("unreg:")));

        // A binder this registrar never registered is logged, not fatal.
        reg.shared.on_clients_binder(&fresh_binder(), false);
        assert!(!reg.shared.exited.load(Ordering::Acquire));
    }

    /// Dispatches `onClients` from inside `registerClientCallback` on its own thread, as the oneway wire does.
    #[derive(Default)]
    struct ReentrantRegistry {
        shared: Mutex<Option<Weak<Shared>>>,
        report: bool,
        notifier: Mutex<Option<JoinHandle<()>>>,
    }

    impl ReentrantRegistry {
        fn join(&self) {
            if let Some(h) = self.notifier.lock().unwrap().take() {
                h.join().unwrap();
            }
        }
    }

    impl Registry for ReentrantRegistry {
        fn add_lazy_service(
            &self,
            _name: &str,
            _binder: &SIBinder,
        ) -> std::result::Result<(), Status> {
            Ok(())
        }
        fn register_client_callback(
            &self,
            name: &str,
            binder: &SIBinder,
            callback: &Strong<dyn IClientCallback>,
        ) -> std::result::Result<(), Status> {
            let shared = self.shared.lock().unwrap().clone();
            let Some(shared) = shared.as_ref().and_then(Weak::upgrade) else {
                return Ok(());
            };
            let (cb, b, want) = (callback.clone(), binder.clone(), self.report);
            *self.notifier.lock().unwrap() =
                Some(std::thread::spawn(move || cb.onClients(&b, want).unwrap()));
            // Return only once the notification has been recorded, so the
            // assertion is about the round trip's own window.
            let name = name.to_string();
            spin_until(|| shared.lock().services.get(&name).map(|e| e.has_clients) == Some(want));
            Ok(())
        }
        fn try_unregister_service(
            &self,
            _name: &str,
            _binder: &SIBinder,
        ) -> std::result::Result<(), Status> {
            Ok(())
        }
    }

    /// An `onClients` that arrives *during* `register_service`'s own round
    /// trips must still land. The service manager sends one per change and
    /// never repeats it, so dropping this one strands the process: it would
    /// go on believing it has clients with nothing left to correct it.
    #[test]
    fn a_notification_arriving_during_registration_is_not_dropped() {
        let registry = Arc::new(ReentrantRegistry {
            report: true,
            ..Default::default()
        });
        let reg = LazyServiceRegistrar::with_registry(registry.clone());
        *registry.shared.lock().unwrap() = Some(Arc::downgrade(&reg.shared));

        reg.register_service("nested", fresh_binder()).unwrap();
        registry.join();

        assert!(
            reg.snapshot()[0].1,
            "the notification sent from inside registerClientCallback was dropped"
        );
    }

    /// Every service gets the *same* `IClientCallback`, and the registrar
    /// keeps it alive. The service manager holds only a proxy, so a
    /// registrar that made a fresh callback per registration — or let the
    /// object drop — would leave it calling something that no longer
    /// routes: services registered, process never shutting down, nothing
    /// logged.
    #[test]
    fn one_client_callback_serves_the_whole_registrar() {
        let (reg, registry) = fixture();
        reg.register_service("a", fresh_binder()).unwrap();
        let first = registry.last_callback();
        reg.register_service("b", fresh_binder()).unwrap();

        assert_eq!(
            first,
            registry.last_callback(),
            "one callback per registrar"
        );
        assert!(
            reg.shared.lock().callback.is_some(),
            "the registrar must hold the callback itself"
        );

        // It is the live one: a notification through it reaches the state.
        first
            .onClients(&reg.binder_for("a").unwrap(), true)
            .unwrap();
        assert!(reg.snapshot().iter().find(|s| s.0 == "a").unwrap().1);
    }

    /// A service nobody ever looks up must not keep the process alive. The
    /// service manager reports only changes, so an unused name never draws an
    /// `onClients` at all — assuming a client for it would be an assumption
    /// nothing ever takes back. AOSP `Service::clients` starts `false`.
    #[test]
    fn a_service_nobody_used_does_not_block_shutdown() {
        let (reg, registry) = fixture();
        reg.register_service("used", fresh_binder()).unwrap();
        reg.register_service("never_used", fresh_binder()).unwrap();

        reg.on_clients("used", true);
        assert!(!reg.shared.exited.load(Ordering::Acquire), "still in use");

        reg.on_clients("used", false);
        assert!(
            reg.shared.exited.load(Ordering::Acquire),
            "`never_used` was never reported on, so it must not count as in use"
        );
        assert!(registry.calls().contains(&"unreg:never_used".to_string()));
    }

    /// Two `register_service` calls never overlap. Their round trips are
    /// outside the state lock, so without a lock of their own the loser's
    /// rollback could undo the winner's publish. AOSP gets this from
    /// `mMutex` spanning `registerServiceLocked`.
    #[test]
    fn registrations_do_not_overlap() {
        const CALLERS: usize = 4;
        #[derive(Default)]
        struct DepthProbe {
            depth: AtomicUsize,
            max_depth: AtomicUsize,
            /// Threads that have called `register_service` (set before the call).
            arrived: AtomicUsize,
        }
        impl Registry for DepthProbe {
            fn add_lazy_service(&self, _: &str, _: &SIBinder) -> std::result::Result<(), Status> {
                let depth = self.depth.fetch_add(1, Ordering::AcqRel) + 1;
                self.max_depth.fetch_max(depth, Ordering::AcqRel);
                // Hold the round trip open until every caller is in `register_service`, then give
                // them time to enter: without the `ops` lock they would, and `max_depth` would show it.
                spin_until(|| self.arrived.load(Ordering::Acquire) == CALLERS);
                std::thread::sleep(Duration::from_millis(20));
                self.max_depth
                    .fetch_max(self.depth.load(Ordering::Acquire), Ordering::AcqRel);
                self.depth.fetch_sub(1, Ordering::AcqRel);
                Ok(())
            }
            fn register_client_callback(
                &self,
                _: &str,
                _: &SIBinder,
                _: &Strong<dyn IClientCallback>,
            ) -> std::result::Result<(), Status> {
                Ok(())
            }
            fn try_unregister_service(
                &self,
                _: &str,
                _: &SIBinder,
            ) -> std::result::Result<(), Status> {
                Ok(())
            }
        }

        let registry = Arc::new(DepthProbe::default());
        let reg = LazyServiceRegistrar::with_registry(registry.clone());
        std::thread::scope(|scope| {
            for i in 0..CALLERS {
                let (reg, registry) = (&reg, &registry);
                scope.spawn(move || {
                    registry.arrived.fetch_add(1, Ordering::AcqRel);
                    reg.register_service(&format!("svc{i}"), fresh_binder())
                        .unwrap()
                });
            }
        });

        assert_eq!(reg.registered_count(), CALLERS);
        assert_eq!(
            registry.max_depth.load(Ordering::Acquire),
            1,
            "two register_service calls were inside addService at once"
        );
    }

    /// A `register_service` that fails after a notification landed must not
    /// put the pre-call state back over it. The service manager reports only
    /// changes, so the value it just sent would never come again.
    #[test]
    fn a_failed_re_register_keeps_the_notification_that_raced_it() {
        #[derive(Default)]
        struct NotifyThenFail {
            shared: Mutex<Option<Weak<Shared>>>,
            armed: AtomicBool,
            notifier: Mutex<Option<JoinHandle<bool>>>,
        }
        impl Registry for NotifyThenFail {
            fn add_lazy_service(
                &self,
                name: &str,
                _: &SIBinder,
            ) -> std::result::Result<(), Status> {
                if !self.armed.load(Ordering::Acquire) {
                    return Ok(());
                }
                let shared = self.shared.lock().unwrap().clone();
                let Some(shared) = shared.as_ref().and_then(Weak::upgrade) else {
                    return Ok(());
                };
                // The wire delivers this on a pool thread while the round
                // trip is still open.
                let (s2, n2) = (shared.clone(), name.to_string());
                *self.notifier.lock().unwrap() =
                    Some(std::thread::spawn(move || s2.on_clients(&n2, false)));
                let name = name.to_string();
                spin_until(|| {
                    shared.lock().services.get(&name).map(|e| e.has_clients) == Some(false)
                });
                Err(Status::new_service_specific_error(-1, None))
            }
            fn register_client_callback(
                &self,
                _: &str,
                _: &SIBinder,
                _: &Strong<dyn IClientCallback>,
            ) -> std::result::Result<(), Status> {
                Ok(())
            }
            fn try_unregister_service(
                &self,
                _: &str,
                _: &SIBinder,
            ) -> std::result::Result<(), Status> {
                Ok(())
            }
        }

        let registry = Arc::new(NotifyThenFail::default());
        let reg = LazyServiceRegistrar::with_registry(registry.clone());
        *registry.shared.lock().unwrap() = Some(Arc::downgrade(&reg.shared));

        let binder = fresh_binder();
        reg.register_service("racy", binder.clone()).unwrap();
        reg.on_clients("racy", true);

        registry.armed.store(true, Ordering::Release);
        assert!(reg.register_service("racy", binder).is_err());
        registry
            .notifier
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .unwrap();

        assert!(
            !reg.snapshot()[0].1,
            "the rollback put back a client the service manager had retracted"
        );
    }

    /// A `register_service` that cannot finish leaves nothing behind.
    #[test]
    fn a_failed_registration_does_not_leave_an_entry() {
        struct Failing;
        impl Registry for Failing {
            fn add_lazy_service(&self, _: &str, _: &SIBinder) -> std::result::Result<(), Status> {
                Err(Status::new_service_specific_error(-1, None))
            }
            fn register_client_callback(
                &self,
                _: &str,
                _: &SIBinder,
                _: &Strong<dyn IClientCallback>,
            ) -> std::result::Result<(), Status> {
                unreachable!("add_lazy_service fails first")
            }
            fn try_unregister_service(
                &self,
                _: &str,
                _: &SIBinder,
            ) -> std::result::Result<(), Status> {
                Ok(())
            }
        }
        let reg = LazyServiceRegistrar::with_registry(Arc::new(Failing));
        assert!(reg.register_service("gone", fresh_binder()).is_err());
        assert!(
            reg.snapshot().is_empty(),
            "the published entry was rolled back"
        );
        assert_eq!(reg.registered_count(), 0);
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
