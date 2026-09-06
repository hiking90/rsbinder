// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
#![allow(non_snake_case)]

use env_logger::Env;
use hub::android_16::{
    BnServiceManager, IServiceManager, DUMP_FLAG_PRIORITY_ALL, DUMP_FLAG_PRIORITY_DEFAULT,
    FLAG_IS_LAZY_SERVICE,
};
use rsbinder::*;
use rsbinder_tools::config::{self, Activator, Enforcer, Permission, SystemResolver, SystemRunner};
use rsbinder_tools::notify::Notifier;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

struct Service {
    binder: SIBinder,
    dump_priority: i32,
    has_clients: bool,
    guarantee_client: bool,
    context: rsbinder::thread_state::CallingContext,
    /// Descriptor-based accessor marking. Set at `addService` time by
    /// checking whether `binder.descriptor()` equals
    /// `"android.os.IAccessor"`. AOSP's servicemanager distinguishes
    /// accessors via VINTF `<accessor>` entries; without VINTF,
    /// descriptor inspection is the closest semantic equivalent (the
    /// binder itself self-identifies as `IAccessor`). When `true`,
    /// `getService2`/`checkService2` wraps the binder in
    /// `Service::Accessor(Some(binder))` instead of
    /// `Service::ServiceWithMetadata`, so the consume-side accessor
    /// arm in `rsbinder::hub::servicemanager_16` picks it up.
    is_accessor: bool,
}

/// A callback invocation deferred until after the `Inner` mutex guard is
/// dropped. Collected while holding the lock, fired afterwards — outbound
/// binder transactions must never run while holding state (R1), and AOSP's
/// servicemanager likewise fans out callbacks without holding its lock.
enum PendingCallback {
    Registration {
        callback:
            rsbinder::Strong<dyn hub::android_16::android::os::IServiceCallback::IServiceCallback>,
        name: String,
        binder: SIBinder,
    },
    Clients {
        callback:
            rsbinder::Strong<dyn hub::android_16::android::os::IClientCallback::IClientCallback>,
        binder: SIBinder,
        has_clients: bool,
    },
}

impl PendingCallback {
    fn fire(self) -> rsbinder::BinderResult<()> {
        match self {
            PendingCallback::Registration {
                callback,
                name,
                binder,
            } => callback.onRegistration(&name, &binder),
            PendingCallback::Clients {
                callback,
                binder,
                has_clients,
            } => callback.onClients(&binder, has_clients),
        }
    }
}

/// Fire each collected callback after the caller has dropped the `Inner`
/// guard. Order is preserved (collection order = invocation order). Errors
/// are logged and swallowed — matches the prior in-lock notification paths.
fn fire_pending(pending: Vec<PendingCallback>) {
    for cb in pending {
        cb.fire()
            .unwrap_or_else(|e| log::error!("Failed to notify client callback: {e:?}"));
    }
}

/// Like [`fire_pending`] but propagates the first callback error, preserving
/// `addService`'s original error-propagating semantics for `onRegistration`.
fn fire_pending_propagate(pending: Vec<PendingCallback>) -> rsbinder::BinderResult<()> {
    for cb in pending {
        cb.fire()?;
    }
    Ok(())
}

struct DeathRecipientWrapper(mpsc::Sender<rsbinder::WIBinder>);

impl rsbinder::DeathRecipient for DeathRecipientWrapper {
    fn binder_died(&self, who: &rsbinder::WIBinder) {
        self.0.send(who.clone()).unwrap_or_else(|e| {
            log::error!("Failed to send death notification: {e:?}");
        });
    }
}

/// Lock the registry mutex, recovering from poisoning instead of
/// propagating it. The service manager is the single point of failure for
/// the whole device's IPC: every transaction handler runs under
/// `catch_unwind`, so a panic *under* the guard poisons the mutex rather
/// than crashing the process — and a plain `.lock().unwrap()` would then
/// turn that one panic into a permanent outage (every subsequent request,
/// and the death-notification thread, would panic on the poison).
/// `into_inner` always yields a type-valid `Inner`, and the worst surviving
/// inconsistency from a panic mid-section is a leaked death link or a
/// skipped notification — never a corrupted map or a deadlock — so
/// continuing on the recovered state degrades gracefully rather than
/// wedging device IPC.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The name `rsb_hub` publishes itself under, so clients can reach the
/// service manager through the registry as well as through handle 0.
/// Matches AOSP `main.cpp`'s `addService("manager", ...)`.
const SELF_SERVICE_NAME: &str = "manager";

/// Upper bound on registration / client callbacks held per service name.
/// `registerForNotifications` has no uid gating on Linux, so without a cap
/// (and identity de-duplication) a client could loop-register to grow the
/// heap without bound and multiply every per-service notification by the
/// duplicate count. AOSP relies on SELinux/uid admission that rsb_hub does
/// not have; this is the defense in its place. Generous enough that no
/// legitimate caller hits it.
const MAX_CALLBACKS_PER_NAME: usize = 256;

/// Upper bound on the number of *distinct service names* held in each registry
/// map (`name_to_service`, `name_to_registration_callbacks`,
/// `name_to_client_callbacks`). `MAX_CALLBACKS_PER_NAME` caps callbacks per
/// name but not the number of names, so without this a single client could
/// loop over distinct names (`addService("svc.{i}")`,
/// `registerForNotifications("svc.{i}")`, ...) and grow the heap without bound.
/// Like `MAX_CALLBACKS_PER_NAME`, this is the defense in place of the
/// SELinux/uid admission AOSP relies on and rsb_hub lacks. Generous enough that
/// no real device (a few hundred services) hits it.
const MAX_DISTINCT_NAMES: usize = 10_000;

/// True when inserting `name` would add a *new* distinct key to a registry
/// `map` that already holds `MAX_DISTINCT_NAMES` names. Overwriting an existing
/// key never grows the map, so it is always allowed. Factored out for testing.
fn distinct_name_cap_exceeded<V>(map: &BTreeMap<String, V>, name: &str) -> bool {
    !map.contains_key(name) && map.len() >= MAX_DISTINCT_NAMES
}

/// One kernel death subscription, and how many registry entries currently
/// depend on it.
///
/// The binder is held *weakly*: the maps that need it alive already hold it
/// strongly, and a strong reference here would keep a proxy — and its
/// kernel ref — alive past the registry entry that justified it. The weak
/// reference is what an obituary carries, so it is also what
/// [`Inner::retire_dead_binder`] matches on.
struct DeathLink {
    weak: rsbinder::WIBinder,
    count: usize,
}

struct Inner {
    death_recipient: Arc<DeathRecipientWrapper>,
    /// Death subscriptions, one per proxy handle, reference-counted by the
    /// number of registry entries depending on each.
    ///
    /// `ProxyHandle::link_to_death` appends to a per-proxy `Vec` without
    /// deduplicating and `unlink_to_death` removes a single entry, so a
    /// registry that links and unlinks ad hoc drifts in both directions.
    /// It drifted both ways: one binder registered under K names took K
    /// subscriptions and so fired `binder_died` K times, each a full
    /// O(registry) cleanup sweep; and `unregisterForNotifications` removed
    /// the callback without unlinking, so every register/unregister cycle
    /// left another subscription behind — unbounded, and reached by any
    /// local caller, which is exactly what `MAX_CALLBACKS_PER_NAME` exists
    /// to prevent.
    ///
    /// Counting here makes the pairing structural: link on 0→1, unlink on
    /// 1→0, and neither drift is expressible. Keyed by handle because that
    /// is what the kernel subscription is keyed by; native (`Bn*`) binders
    /// have no death notification and are skipped.
    death_links: BTreeMap<u32, DeathLink>,
    name_to_service: BTreeMap<String, Service>,
    name_to_registration_callbacks: BTreeMap<
        String,
        Vec<rsbinder::Strong<dyn hub::android_16::android::os::IServiceCallback::IServiceCallback>>,
    >,
    name_to_client_callbacks: BTreeMap<
        String,
        Vec<rsbinder::Strong<dyn hub::android_16::android::os::IClientCallback::IClientCallback>>,
    >,
}

impl Inner {
    /// "Known clients" subtraction passed to
    /// [`Inner::handle_service_client_callback`] from **on-demand**
    /// callsites (`addService`, `tryGetBinder`, `registerClientCallback`,
    /// `tryUnregisterService`). The active binder transaction holds one
    /// ref + servicemanager holds one ref ⇒ `2`. Matches AOSP
    /// `ServiceManager.cpp:1109` (`constexpr size_t kKnownClients = 2`).
    const KNOWN_CLIENTS_ON_DEMAND: usize = 2;

    /// "Known clients" subtraction passed from the **periodic poller**
    /// ([`ServiceManager::run_client_callback_poller`]). The poller runs
    /// outside any binder transaction, so only servicemanager's own ref
    /// counts as "known" ⇒ `1`. Matches AOSP `ServiceManager.cpp:985`
    /// (`handleClientCallbacks` body: `handleServiceClientCallback(1
    /// /* sm has one refcount */, name, true)`). Using `2` here would
    /// under-count, masking the presence of a single real client and
    /// firing spurious `onClients(false)` notifications.
    const KNOWN_CLIENTS_PERIODIC: usize = 1;

    fn new(death_sender: mpsc::Sender<rsbinder::WIBinder>) -> Self {
        Self {
            death_recipient: Arc::new(DeathRecipientWrapper(death_sender)),
            death_links: BTreeMap::new(),
            name_to_service: BTreeMap::new(),
            name_to_registration_callbacks: BTreeMap::new(),
            name_to_client_callbacks: BTreeMap::new(),
        }
    }

    fn add_service(&mut self, name: &str, service: Service) -> rsbinder::BinderResult<()> {
        self.name_to_service.insert(name.to_owned(), service);
        Ok(())
    }

    /// Start depending on `binder`'s death notification, taking the kernel
    /// subscription if this is the first dependant.
    ///
    /// A native binder cannot be linked and is silently skipped, so callers
    /// do not need to test for it. Every successful call must be paired
    /// with exactly one [`Inner::release_death_link`] or, once the binder
    /// has died, one [`Inner::retire_dead_binder`].
    fn retain_death_link(&mut self, binder: &SIBinder) -> rsbinder::BinderResult<()> {
        let Some(handle) = binder.as_proxy().map(|proxy| proxy.handle()) else {
            return Ok(());
        };
        let recipient: Arc<dyn rsbinder::DeathRecipient> = self.death_recipient.clone();
        self.retain_counted(handle, &SIBinder::downgrade(binder), || {
            binder
                .link_to_death(Arc::downgrade(&recipient))
                .map_err(Into::into)
        })
    }

    /// Bookkeeping half of [`Inner::retain_death_link`], with the kernel
    /// call passed in.
    ///
    /// Split because the two halves fail differently: taking a subscription
    /// needs a live proxy and so a live binder device, while the accounting
    /// is pure — and the accounting is what drifted. The seam lets a test
    /// assert *how many times* the kernel call happens for a given sequence
    /// of retains and releases, which is the property that was wrong.
    ///
    /// `link` runs only when this is the first dependant, and the entry is
    /// recorded only if it succeeds — a failed link leaves no accounting
    /// behind to release.
    fn retain_counted(
        &mut self,
        handle: u32,
        weak: &rsbinder::WIBinder,
        link: impl FnOnce() -> rsbinder::BinderResult<()>,
    ) -> rsbinder::BinderResult<()> {
        if let Some(existing) = self.death_links.get_mut(&handle) {
            existing.count += 1;
            return Ok(());
        }
        link()?;
        self.death_links.insert(
            handle,
            DeathLink {
                weak: weak.clone(),
                count: 1,
            },
        );
        Ok(())
    }

    /// Stop depending on `binder`'s death notification, dropping the kernel
    /// subscription once nothing depends on it.
    ///
    /// Dropping it matters: `ProxyHandle::Drop` does not clear the kernel
    /// subscription (rsbinder has no `~Service` hook the way AOSP does), so
    /// a registration that is replaced or unregistered would otherwise leak
    /// a `BC_REQUEST_DEATH_NOTIFICATION` for the rest of the process's life.
    fn release_death_link(&mut self, binder: &SIBinder) {
        let Some(handle) = binder.as_proxy().map(|proxy| proxy.handle()) else {
            return;
        };
        let recipient: Arc<dyn rsbinder::DeathRecipient> = self.death_recipient.clone();
        self.release_counted(handle, || {
            if let Err(e) = binder.unlink_to_death(Arc::downgrade(&recipient)) {
                // A binder that died between its obituary and this call is
                // the ordinary racing case — not worth a warning on every
                // service restart.
                if e == rsbinder::StatusCode::DeadObject {
                    log::debug!("death notification for handle {handle} was already gone");
                } else {
                    log::warn!("failed to unlink death notification for handle {handle}: {e:?}");
                }
            }
        });
    }

    /// Bookkeeping half of [`Inner::release_death_link`]; see
    /// [`Inner::retain_counted`] for why it is split. `unlink` runs, and the
    /// entry is dropped, only when the last dependant goes away. A release
    /// with no matching entry is ignored rather than underflowing.
    fn release_counted(&mut self, handle: u32, unlink: impl FnOnce()) {
        let Some(link) = self.death_links.get_mut(&handle) else {
            return;
        };
        link.count -= 1;
        if link.count > 0 {
            return;
        }
        unlink();
        self.death_links.remove(&handle);
    }

    /// Mutate `service.has_clients` under the lock and *collect* (do not
    /// invoke) the resulting `onClients` notifications into `pending`. The
    /// caller fires them via [`fire_pending`] after dropping the guard (R1).
    fn send_client_callback_notification(
        &mut self,
        service_name: &str,
        has_clients: bool,
        context: &str,
        pending: &mut Vec<PendingCallback>,
    ) {
        let service = if let Some(service) = self.name_to_service.get_mut(service_name) {
            service
        } else {
            log::warn!(
                "send_client_callback_notification could not find service {service_name} when {context}"
            );
            return;
        };

        if service.has_clients == has_clients {
            // AOSP's servicemanager treats this with `CHECK_NE` (process
            // abort) — same invariant ("we only notify on state
            // transitions"), but losing the SM kills the whole machine
            // on Linux too. Demote to a loud error + no-op; the only
            // visible consequence of a spurious duplicate is a missed
            // diagnostic, not corrupted state.
            log::error!(
                "send_client_callback_notification called with the same state {has_clients} when {context} — ignored"
            );
            return;
        }

        log::info!(
            "Notifying {} they {} (previously: {}) have clients when {}",
            service_name,
            if has_clients { "do" } else { "don't" },
            if service.has_clients { "do" } else { "don't" },
            context
        );

        let binder = service.binder.clone();
        match self.name_to_client_callbacks.get(service_name) {
            Some(callbacks) => {
                for callback in callbacks {
                    pending.push(PendingCallback::Clients {
                        callback: callback.clone(),
                        binder: binder.clone(),
                        has_clients,
                    });
                }
            }
            None => {
                log::warn!("send_client_callback_notification could not find callbacks for service when {context}");
            }
        }

        if let Some(service) = self.name_to_service.get_mut(service_name) {
            service.has_clients = has_clients;
        }
    }

    /// Like its name suggests, but collects callbacks into `pending` instead
    /// of invoking them (see [`Inner::send_client_callback_notification`]).
    fn handle_service_client_callback(
        &mut self,
        known_clients: usize,
        service_name: &str,
        is_called_on_interval: bool,
        pending: &mut Vec<PendingCallback>,
    ) -> Result<bool> {
        let service = if let Some(service) = self.name_to_service.get(service_name) {
            if self
                .name_to_client_callbacks
                .get(service_name)
                .is_none_or(|callbacks| callbacks.is_empty())
            {
                return Ok(true);
            }
            service
        } else {
            return Ok(true);
        };

        // `strong_ref_count_for_node` is a kernel-binder ioctl over a
        // `ProxyHandle`; it has no analogue for native (Bn*) services
        // hosted in this process, since the kernel never sees their
        // refcount. AOSP's servicemanager can't be queried for its own
        // refcount either; we mirror that by reporting `has_clients =
        // true` (the safe over-estimate that suppresses spurious
        // `onClients(false)` notifications).
        let Some(proxy) = service.binder.as_proxy() else {
            return Ok(true);
        };
        let count = match rsbinder::ProcessState::as_self().strong_ref_count_for_node(proxy) {
            Ok(count) => count,
            Err(e) => {
                log::error!("Failed to get strong ref count for {service_name}: {e:?}");
                return Ok(true);
            }
        };
        let has_kernel_reported_clients = count > known_clients;

        // To avoid the borrow checker, we need to get the value of has_clients
        let mut has_clients = service.has_clients;

        if service.guarantee_client {
            if !has_clients && !has_kernel_reported_clients {
                self.send_client_callback_notification(
                    service_name,
                    true,
                    "service is guaranteed to be in use",
                    pending,
                );
            }

            if let Some(service) = self.name_to_service.get_mut(service_name) {
                service.has_clients = true;
                // Guarantee is temporary — fired (or skipped) above; reset so
                // subsequent (periodic-poll or on-demand) entries don't
                // re-trigger the "guaranteed in use" branch every call.
                // Mirrors AOSP `ServiceManager.cpp:1012`. Without this
                // reset the 5s poller ping-pongs onClients(true/false)
                // every cycle for any service that ever guaranteed a
                // client (i.e., that anyone ever called `tryGetBinder`
                // on), defeating the lazy-service contract.
                service.guarantee_client = false;
                has_clients = true;
            }
        }

        if has_kernel_reported_clients && !has_clients {
            self.send_client_callback_notification(
                service_name,
                true,
                "we now have a record of a client",
                pending,
            );
            if let Some(service) = self.name_to_service.get(service_name) {
                has_clients = service.has_clients;
            }
        }

        if is_called_on_interval && !has_kernel_reported_clients && has_clients {
            self.send_client_callback_notification(
                service_name,
                false,
                "we now have no record of a client",
                pending,
            );
            if let Some(service) = self.name_to_service.get(service_name) {
                has_clients = service.has_clients;
            }
        }

        Ok(has_clients)
    }

    /// Look up a registered service by name.
    ///
    /// The metadata rides along because `getService2`/`checkService2` put it
    /// on the wire; see [`Lookup`].
    /// No `start_if_not_found`: AOSP takes it here, but starting a service
    /// means spawning a process, and this runs under the registry lock.
    /// `getService` triggers the start itself, after the lock is dropped
    /// and the reply is decided — see
    /// [`ServiceManager::try_start_service`].
    fn try_get_binder(
        &mut self,
        name: &str,
        pending: &mut Vec<PendingCallback>,
    ) -> rsbinder::BinderResult<Option<Lookup>> {
        let service = if let Some(service) = self.name_to_service.get_mut(name) {
            service
        } else {
            return Ok(None);
        };

        let out = service.binder.clone();
        let is_accessor = service.is_accessor;
        let is_lazy = service.dump_priority & FLAG_IS_LAZY_SERVICE != 0;
        service.guarantee_client = true;
        self.handle_service_client_callback(Self::KNOWN_CLIENTS_ON_DEMAND, name, false, pending)?;

        if let Some(service) = self.name_to_service.get_mut(name) {
            service.guarantee_client = true;
        }

        Ok(Some(Lookup {
            binder: out,
            is_accessor,
            is_lazy,
        }))
    }

    /// Drop every registration callback for `binder` under `name`, and
    /// return how many entries went.
    ///
    /// The count, not a bool: each entry holds one reference on the
    /// callback's death subscription, so `unregisterForNotifications` has
    /// to release exactly as many as it removed.
    ///
    /// Matches on binder identity (`Arc` pointer equality), which is what
    /// the proxy cache guarantees for two references to the same live
    /// handle. The dead-binder path cannot use this and goes through
    /// [`Inner::retire_dead_binder`] instead.
    fn remove_registration_callback(&mut self, name: &str, binder: &SIBinder) -> usize {
        let mut removed = 0;
        if let Some(callbacks) = self.name_to_registration_callbacks.get_mut(name) {
            callbacks.retain(|callback| {
                let keep = callback.as_binder() != *binder;
                removed += usize::from(!keep);
                keep
            });
            if callbacks.is_empty() {
                self.name_to_registration_callbacks.remove(name);
            }
        }
        removed
    }

    /// Drop every registration owned by a binder that has died: its service
    /// names, its registration callbacks, its client callbacks, and its
    /// death-subscription record. Returns `(services, callbacks)` retired.
    ///
    /// Matching `who` against a stored `SIBinder` is only sound because
    /// [`SIBinder::downgrade`] takes a proxy's identity from the proxy
    /// itself. It used to read it from the proxy cache, which the obituary
    /// retires *before* dispatching — so this comparison answered `false`
    /// for every binder and each death retired nothing, leaving the
    /// registry to hand out dead binders and hold their names forever.
    fn retire_dead_binder(&mut self, who: &rsbinder::WIBinder) -> (usize, usize) {
        let before = self.name_to_service.len();
        self.name_to_service
            .retain(|_, service| *who != service.binder);
        let services = before - self.name_to_service.len();

        let mut callbacks = 0;
        self.name_to_registration_callbacks.retain(|_, entries| {
            entries.retain(|callback| {
                let keep = *who != callback.as_binder();
                callbacks += usize::from(!keep);
                keep
            });
            !entries.is_empty()
        });

        // AOSP `ServiceManager::binderDied`'s third loop: without this the
        // dead `IClientCallback` is held for the lifetime of rsb_hub and
        // `onClients` keeps firing at a dead proxy on every state change.
        self.name_to_client_callbacks.retain(|_, entries| {
            entries.retain(|callback| *who != callback.as_binder());
            !entries.is_empty()
        });

        // Last: the entries above each held a reference on this
        // subscription, and the kernel released it when it sent the
        // obituary, so there is nothing to unlink.
        self.death_links.retain(|_, link| link.weak != *who);

        (services, callbacks)
    }
}

struct ServiceManager {
    inner: Arc<Mutex<Inner>>,
    /// When `false` (the default), `addService` refuses to overwrite a live
    /// registration owned by a different UID — the signature of a
    /// service-name hijack. `--allow-cross-uid-overwrite` sets it `true`
    /// for deployments that intentionally re-register across UIDs.
    allow_cross_uid_overwrite: bool,
    /// Per-name `add`/`find`/`list` access control. Every AIDL entry point
    /// consults this before touching the registry; see
    /// [`ServiceManager::require`] and `plans/6-1-hub-access-control.md`.
    enforcer: Arc<Enforcer>,
    /// Brings a declared service up when a lookup misses it.
    activator: Activator,
}

impl ServiceManager {
    fn new(allow_cross_uid_overwrite: bool, enforcer: Arc<Enforcer>) -> Self {
        Self::with_activator(
            allow_cross_uid_overwrite,
            enforcer,
            Activator::new(Arc::new(SystemRunner)),
        )
    }

    fn with_activator(
        allow_cross_uid_overwrite: bool,
        enforcer: Arc<Enforcer>,
        activator: Activator,
    ) -> Self {
        let (death_sender, death_receiver) = mpsc::channel();

        let this = Self {
            inner: Arc::new(Mutex::new(Inner::new(death_sender))),
            allow_cross_uid_overwrite,
            enforcer,
            activator,
        };

        this.run_death_receiver(death_receiver);
        this.run_client_callback_poller();

        this
    }

    fn run_death_receiver(&self, death_receiver: mpsc::Receiver<rsbinder::WIBinder>) {
        let inner_clone = Arc::clone(&self.inner);
        let spawn_result = std::thread::Builder::new()
            .name("rsb_hub:death".to_owned())
            .spawn(move || {
                for who in death_receiver {
                    let (services, callbacks) = {
                        let mut inner = lock_recover(&inner_clone);
                        inner.retire_dead_binder(&who)
                    };
                    // Worth a line even at zero: a death that retires nothing
                    // means the obituary matched no registration, which is
                    // the shape this cleanup failing takes — and when it
                    // fails the registry keeps handing out a dead binder.
                    log::info!(
                        "binder died: retired {services} service name(s) and \
                         {callbacks} registration callback(s)"
                    );
                }
            });
        if let Err(e) = spawn_result {
            log::error!("Failed to spawn death receiver thread: {e}");
        }
    }

    /// AOSP parity: mirror of `ClientCallbackCallback`
    /// (`frameworks/native/cmds/servicemanager/main.cpp:91-144`) — on a
    /// 5-second cadence, walk every registered service and call
    /// [`Inner::handle_service_client_callback`] with
    /// `is_called_on_interval = true`.
    ///
    /// Without this, lazy services never receive the `onClients(false)`
    /// notification that tells them to shut down: the on-demand
    /// callsites scattered through `addService` / `try_get_binder` /
    /// `registerClientCallback` all pass `is_called_on_interval =
    /// false`, which by design short-circuits the "no clients" arm at
    /// [`Inner::handle_service_client_callback`] (the comment at the
    /// `is_called_on_interval` branch documents the contract).
    ///
    /// The poller holds a `Weak<Mutex<Inner>>` and exits when the
    /// upgrade fails (i.e., the owning [`ServiceManager`] is dropped).
    /// In production the SM lives for the whole process lifetime, so
    /// the exit branch is primarily a test-cleanup convenience; in
    /// tests it lets the thread die without an extra shutdown channel.
    fn run_client_callback_poller(&self) {
        /// 5-second cadence matches AOSP `kClientCallbackCheckInterval`
        /// (`main.cpp:103-112` — timerfd interval). The per-call
        /// "known clients" subtraction is [`Inner::KNOWN_CLIENTS_PERIODIC`]
        /// (= 1, matches AOSP `ServiceManager.cpp:985`), distinct from
        /// the on-demand callsites' [`Inner::KNOWN_CLIENTS_ON_DEMAND`]
        /// (= 2) because the poller runs outside any binder transaction.
        const INTERVAL: Duration = Duration::from_secs(5);

        let inner_weak = Arc::downgrade(&self.inner);
        let spawn_result = std::thread::Builder::new()
            .name("rsb_hub:cbpoll".to_owned())
            .spawn(move || loop {
                std::thread::sleep(INTERVAL);

                let Some(inner_arc) = inner_weak.upgrade() else {
                    log::debug!("client callback poller exiting (ServiceManager dropped)");
                    return;
                };

                // Recover from poisoning rather than exiting: if the poller
                // gave up here, lazy services would never again receive their
                // periodic `onClients(false)` and could never shut down. The
                // guarded state is a self-contained map mutation, so the
                // recovered state is safe to continue from.
                let mut inner = lock_recover(&inner_arc);

                // Snapshot the names before iterating so we don't hold a
                // borrow into `name_to_service` across calls that may
                // mutate it (e.g., `send_client_callback_notification`
                // writing back `service.has_clients`). The map is small
                // (one entry per registered service) — a transient `Vec`
                // is cheaper than the alternative refactor.
                let names: Vec<String> = inner.name_to_service.keys().cloned().collect();
                let mut pending = Vec::new();
                for name in &names {
                    if let Err(e) = inner.handle_service_client_callback(
                        Inner::KNOWN_CLIENTS_PERIODIC,
                        name,
                        true,
                        &mut pending,
                    ) {
                        log::error!("client callback poll failed for {name}: {e:?}");
                    }
                }
                drop(inner);
                fire_pending(pending);
            });
        if let Err(e) = spawn_result {
            log::error!("Failed to spawn client callback poller thread: {e}");
        }
    }

    fn is_valid_service_name(name: &str) -> bool {
        if name.is_empty() || name.len() > 127 {
            return false;
        }
        for c in name.chars() {
            if c == '_' || c == '-' || c == '.' || c == '/' {
                continue;
            }
            if c.is_ascii_lowercase() {
                continue;
            }
            if c.is_ascii_uppercase() {
                continue;
            }
            if c.is_ascii_digit() {
                continue;
            }
            return false;
        }

        true
    }

    /// May the current caller exercise `permission` on `name`?
    ///
    /// `rsbinder::calling_caller()` is `None` exactly when this thread is
    /// not dispatching a binder transaction. For `rsb_hub` that is one call
    /// and one only: publishing itself as [`SELF_SERVICE_NAME`] at startup,
    /// which the generated code routes as a direct Rust call rather than a
    /// transaction. There is no remote peer to authorize, and a policy
    /// cannot be expected to grant the hub access to itself.
    ///
    /// The bypass is scoped to exactly that permission and that name rather
    /// than to "no transaction" in general: a request that arrived over
    /// binder is always inside a transaction, but `rsb_hub`'s own
    /// death-notification and client-callback-poller threads are not, and
    /// if either ever grows a path through here it must be denied, not
    /// waved through. See `plans/6-1-hub-access-control.md` D10.
    fn allows(&self, permission: Permission, name: &str) -> bool {
        match rsbinder::calling_caller() {
            Some(caller) => self.enforcer.check_caller(permission, name, &caller),
            None => permission == Permission::Add && name == SELF_SERVICE_NAME,
        }
    }

    /// Ask for `name` to be started, if it is declared with a way to start
    /// it. AOSP's `tryStartService`, with a declaration standing in for the
    /// init property.
    ///
    /// Only `getService`/`getService2` reach this — `checkService` is
    /// documented as non-blocking and must not have side effects. It
    /// returns immediately either way: the start runs on its own thread,
    /// and the caller has already answered "not registered". What tells the
    /// client the service came up is the registration notification it is
    /// waiting on, exactly as on Android.
    fn try_start_service(&self, name: &str) {
        let config = self.enforcer.config();
        match config.declarations.activation(name) {
            Some(activation) => self.activator.try_start(name, activation),
            None if config.declarations.is_declared(name) => {
                log::debug!("{name} is declared but has no `start`; not starting it")
            }
            None => log::debug!("{name} is not declared; not starting it"),
        }
    }

    /// [`Self::allows`], as a `Result` for the entry points whose denial is
    /// reported to the caller as `EX_SECURITY`.
    ///
    /// The *lookup* entry points do not use this: AOSP's `tryGetBinder`
    /// returns an empty result rather than an error when `canFind` fails
    /// (`ServiceManager.cpp:468-470`), and `getService` is documented to
    /// return ok regardless. Reporting a denied lookup as "not registered"
    /// also keeps a denied caller from using the error to probe which names
    /// exist.
    fn require(&self, permission: Permission, name: &str) -> rsbinder::BinderResult<()> {
        if self.allows(permission, name) {
            return Ok(());
        }
        let ctx = rsbinder::thread_state::CallingContext::default();
        let msg = format!(
            "policy denied {permission} on {name:?} for uid={} (pid={})",
            ctx.uid, ctx.pid
        );
        log::warn!("{msg}");
        Err((ExceptionCode::Security, msg.as_str()).into())
    }

    /// Add-time access-control decision for an `addService` that would
    /// overwrite an existing registration: returns `true` when the
    /// overwrite must be rejected as a likely service-name hijack.
    ///
    /// Rejected iff the operator has NOT opted into cross-UID overwrites,
    /// the incoming binder is a *different* object than the one registered
    /// (`!same_binder`), AND the caller's UID differs from the registrant's.
    /// A same-UID overwrite (e.g. a service restart under a new PID) and a
    /// re-registration of the identical binder are always allowed. Pure and
    /// total so the security-relevant branch is unit-testable without two
    /// real OS UIDs.
    fn is_cross_uid_overwrite_rejected(
        allow_cross_uid_overwrite: bool,
        same_binder: bool,
        existing_uid: u32,
        caller_uid: u32,
    ) -> bool {
        !allow_cross_uid_overwrite && !same_binder && existing_uid != caller_uid
    }
}

/// One registry row, copied out from under the mutex.
///
/// The rendering pass runs outside the lock because the per-name `find`
/// filter it depends on resolves group membership, which can hit the name
/// service — the same reason [`IServiceManager::listServices`] snapshots.
struct DumpRow {
    name: String,
    pid: i32,
    uid: u32,
    dump_priority: i32,
    has_clients: bool,
    guarantee_client: bool,
    is_accessor: bool,
    registration_callbacks: usize,
    client_callbacks: usize,
}

/// Everything [`render_dump`] needs, so the renderer is pure and testable
/// without a registry, a policy, or a live binder.
struct DumpSnapshot {
    services: Vec<DumpRow>,
    /// Names something is *waiting* on: a registration or client callback
    /// is held for them, but nothing has registered. AOSP has no equivalent
    /// (its `servicemanager` does not implement `dump` at all) — but "who is
    /// blocked on a service that never came up" is the question an operator
    /// actually arrives with.
    awaited: Vec<(String, usize, usize)>,
    death_subscriptions: usize,
    /// `None` when running under `--insecure-allow-all`.
    rules: Option<usize>,
    declarations: usize,
    /// The `args` the caller passed, if they narrowed the listing.
    filter: Vec<String>,
    /// Names the snapshot dropped because the caller may not `find` them.
    hidden: usize,
}

/// True when `name` passes the caller-supplied `args` filter. An empty
/// filter passes everything; otherwise a substring match against any arg
/// is enough, which is what makes `rsb_service dump manager IFoo` useful
/// without teaching the hub a pattern syntax.
fn dump_filter_matches(filter: &[String], name: &str) -> bool {
    filter.is_empty() || filter.iter().any(|f| name.contains(f.as_str()))
}

/// Render a registry snapshot as the `dumpsys`-style text a
/// `DUMP_TRANSACTION` returns.
fn render_dump(w: &mut dyn std::io::Write, snap: &DumpSnapshot) -> std::io::Result<()> {
    writeln!(w, "rsb_hub {}", env!("CARGO_PKG_VERSION"))?;
    match snap.rules {
        Some(rules) => writeln!(w, "access control: enforcing, {rules} rule(s)")?,
        None => writeln!(
            w,
            "access control: DISABLED (--insecure-allow-all); every caller may \
             register, look up and enumerate everything"
        )?,
    }
    writeln!(w, "declarations: {}", snap.declarations)?;
    writeln!(w, "death subscriptions: {}", snap.death_subscriptions)?;
    if !snap.filter.is_empty() {
        writeln!(w, "filter: {}", snap.filter.join(" "))?;
    }
    if snap.hidden > 0 {
        writeln!(w, "hidden by policy: {} name(s)", snap.hidden)?;
    }

    writeln!(w)?;
    writeln!(w, "services ({}):", snap.services.len())?;
    for row in &snap.services {
        writeln!(w, "  {}", row.name)?;
        writeln!(
            w,
            "    pid={} uid={} dump_priority=0x{:x}{} lazy={} accessor={}",
            row.pid,
            row.uid,
            row.dump_priority,
            if row.dump_priority & DUMP_FLAG_PRIORITY_ALL == 0 {
                " (no priority bit: `listServices` will never report it)"
            } else {
                ""
            },
            yes_no(row.dump_priority & FLAG_IS_LAZY_SERVICE != 0),
            yes_no(row.is_accessor),
        )?;
        writeln!(
            w,
            "    clients={} guarantee_client={} callbacks: registration={} client={}",
            yes_no(row.has_clients),
            yes_no(row.guarantee_client),
            row.registration_callbacks,
            row.client_callbacks,
        )?;
    }

    if !snap.awaited.is_empty() {
        writeln!(w)?;
        writeln!(w, "awaiting registration ({}):", snap.awaited.len())?;
        for (name, registration, client) in &snap.awaited {
            writeln!(
                w,
                "  {name}  callbacks: registration={registration} client={client}"
            )?;
        }
    }
    Ok(())
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

impl ServiceManager {
    /// Copy the registry out from under the mutex for [`render_dump`],
    /// keeping only what this caller is allowed to see.
    ///
    /// The `find` filter runs *outside* the lock, as in
    /// [`IServiceManager::listServices`]: it resolves group membership,
    /// which can hit the name service, and no binder-visible state may be
    /// held across that (R1).
    fn dump_snapshot(&self, filter: Vec<String>) -> DumpSnapshot {
        /// How many callbacks a name holds in one of the callback maps.
        /// Generic because the two maps hold different callback traits.
        fn count<V>(map: &BTreeMap<String, Vec<V>>, name: &str) -> usize {
            map.get(name).map_or(0, |v| v.len())
        }

        let (mut rows, mut awaited, death_subscriptions) = {
            let inner = lock_recover(&self.inner);
            let rows: Vec<DumpRow> = inner
                .name_to_service
                .iter()
                .map(|(name, service)| DumpRow {
                    name: name.clone(),
                    pid: service.context.pid,
                    uid: service.context.uid,
                    dump_priority: service.dump_priority,
                    has_clients: service.has_clients,
                    guarantee_client: service.guarantee_client,
                    is_accessor: service.is_accessor,
                    registration_callbacks: count(&inner.name_to_registration_callbacks, name),
                    client_callbacks: count(&inner.name_to_client_callbacks, name),
                })
                .collect();
            let awaited: Vec<(String, usize, usize)> = inner
                .name_to_registration_callbacks
                .keys()
                .chain(inner.name_to_client_callbacks.keys())
                .filter(|name| !inner.name_to_service.contains_key(*name))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|name| {
                    (
                        name.clone(),
                        count(&inner.name_to_registration_callbacks, name),
                        count(&inner.name_to_client_callbacks, name),
                    )
                })
                .collect();
            (rows, awaited, inner.death_links.len())
        };

        // Narrow by the caller's own filter first, so `hidden` counts only
        // what the *policy* withheld from what was asked for.
        rows.retain(|row| dump_filter_matches(&filter, &row.name));
        awaited.retain(|(name, _, _)| dump_filter_matches(&filter, name));

        let before = rows.len() + awaited.len();
        rows.retain(|row| self.allows(Permission::Find, &row.name));
        awaited.retain(|(name, _, _)| self.allows(Permission::Find, name));
        let hidden = before - rows.len() - awaited.len();

        let config = self.enforcer.config();
        DumpSnapshot {
            services: rows,
            awaited,
            death_subscriptions,
            rules: (!self.enforcer.is_allow_all()).then(|| config.policy.rules.len()),
            declarations: config.declarations.len(),
            filter,
            hidden,
        }
    }
}

impl Interface for ServiceManager {
    /// The registry as `rsb_hub` sees it — what `rsb_service dump manager`
    /// prints, and what a `DUMP_TRANSACTION` to handle 0 returns.
    ///
    /// Not AOSP parity: `servicemanager` does not override `dump` at all,
    /// because on Android the same questions are answered by `dumpsys -l`,
    /// `service list` and the init/VINTF files. None of those exist on
    /// Linux, so the hub answers them itself.
    ///
    /// Gated by `list`, like [`listServices`](IServiceManager::listServices)
    /// and `getServiceDebugInfo`, with each name filtered by `find` on top:
    /// a dump that named services the caller may not look up would be a way
    /// around the per-name policy. `args` narrow the listing by substring.
    fn dump(&self, writer: &mut dyn std::io::Write, args: &[String]) -> rsbinder::Result<()> {
        if !self.allows(Permission::List, "") {
            let ctx = rsbinder::thread_state::CallingContext::default();
            log::warn!(
                "policy denied dump (list) for uid={} (pid={})",
                ctx.uid,
                ctx.pid
            );
            // Written to the caller's fd as well as logged: the fd is the
            // only channel `dump` has, and a caller that sees nothing at
            // all cannot tell a denial from an empty registry.
            let _ = writeln!(
                writer,
                "rsb_hub: policy denies `list` to uid={} (pid={})",
                ctx.uid, ctx.pid
            );
            return Err(StatusCode::PermissionDenied);
        }
        let snapshot = self.dump_snapshot(args.to_vec());
        render_dump(writer, &snapshot).map_err(|e| {
            log::error!("dump: writing to the caller's fd failed: {e}");
            e.raw_os_error()
                .map_or(StatusCode::Unknown, StatusCode::Errno)
        })
    }
}

/// A registered service, as `getService2`/`checkService2` need to see it.
struct Lookup {
    binder: SIBinder,
    /// Stamped at `addService` time from the binder's own descriptor;
    /// selects the `Service::Accessor` arm below.
    is_accessor: bool,
    /// `dumpPriority & FLAG_IS_LAZY_SERVICE`, which the client uses to
    /// decide whether the binder may be cached. AOSP's
    /// `BackendUnifiedServiceManager::updateCache`
    /// (`BackendUnifiedServiceManager.cpp:150-153`) returns early for a lazy
    /// service, because a lazy service can be unregistered via
    /// `tryUnregisterService` *without dying* — so no death notification
    /// invalidates the cache, and the client would keep handing out a binder
    /// to a service that has withdrawn.
    is_lazy: bool,
}

/// Convert a `Inner::try_get_binder` lookup result
/// into the `Service` union arm shape returned by
/// `getService2`/`checkService2`. Routes `is_accessor=true`
/// registrations to `Service::Accessor(Some(_))` and regular services
/// to `Service::ServiceWithMetadata`; the `None` arm preserves the
/// prior placeholder behavior so the consume-side accessor arm + the
/// process-local fallback in `rsbinder::hub::servicemanager_16` can
/// still pick up locally-registered providers when servicemanager has
/// no binder under this name.
fn classify_for_service_union(
    lookup: Option<Lookup>,
) -> hub::android_16::android::os::Service::Service {
    use hub::android_16::android::os::{Service, ServiceWithMetadata};
    match lookup {
        Some(Lookup {
            binder,
            is_accessor: true,
            ..
        }) => Service::Service::Accessor(Some(binder)),
        Some(Lookup {
            binder,
            is_accessor: false,
            is_lazy,
        }) => Service::Service::ServiceWithMetadata(ServiceWithMetadata::ServiceWithMetadata {
            service: Some(binder),
            isLazyService: is_lazy,
        }),
        // Not found. AOSP returns `serviceWithMetadata(nullptr)` for a
        // missing non-accessor name, which an AOSP client routes into its
        // local `getInjectedAccessor(name)` fallback; an `accessor(nullptr)`
        // reply would instead just log and return null, skipping that
        // fallback. rsbinder's own consume side runs
        // `try_process_local_fallback` on this arm too (it handles
        // `swm.service.is_none()` identically).
        None => Service::Service::ServiceWithMetadata(ServiceWithMetadata::ServiceWithMetadata {
            service: None,
            isLazyService: false,
        }),
    }
}

impl IServiceManager for ServiceManager {
    /// Like AOSP's, this starts a declared service that is not running:
    /// `getService` is the "start it if you have to" half of the pair, and
    /// [`checkService`](Self::checkService) is the non-blocking half that
    /// must not. See [`try_start_service`](Self::try_start_service).
    fn getService(&self, name: &str) -> rsbinder::BinderResult<Option<rsbinder::SIBinder>> {
        if !self.allows(Permission::Find, name) {
            return Ok(None);
        }
        let mut pending = Vec::new();
        let result = {
            let mut inner = lock_recover(&self.inner);
            inner
                .try_get_binder(name, &mut pending)?
                .map(|found| found.binder)
        };
        fire_pending(pending);
        if result.is_none() {
            self.try_start_service(name);
        }
        Ok(result)
    }

    /// Security note (Linux): unlike AOSP's `servicemanager`, this
    /// implementation performs **no add-time access control**. AOSP
    /// rejects app UIDs (`multiuser_get_app_id(uid) >= AID_APP`) and
    /// runs the SELinux `canAddService` hook; both depend on Android's
    /// UID model / policy and have no equivalent on a plain Linux host.
    /// Consequently any client can register, or silently overwrite, any
    /// service name — and a binder that reports the
    /// `android.os.IAccessor` descriptor is trusted as an accessor on
    /// the registrant's word alone (AOSP instead derives the accessor
    /// relationship from a signed VINTF `<accessor>` manifest entry).
    /// As a minimal mitigation we log a loud warning when a registration
    /// overwrites an entry owned by a different uid/pid (the signature of
    /// a hijack). Vendors needing real enforcement must wrap this with
    /// their own authorization layer.
    fn addService(
        &self,
        name: &str,
        service: &SIBinder,
        allowIsolated: bool,
        dumpPriority: i32,
    ) -> rsbinder::BinderResult<()> {
        self.require(Permission::Add, name)?;

        if !Self::is_valid_service_name(name) {
            return Err(ExceptionCode::IllegalArgument.into());
        }

        // Not fatal, and AOSP does not reject it either
        // (`ServiceManager.cpp:539`) — but a service registered with no
        // priority bit is invisible to every `listServices` filter, which is
        // almost always a caller bug rather than an intent.
        if dumpPriority & DUMP_FLAG_PRIORITY_ALL == 0 {
            log::warn!(
                "addService: '{name}' registered with dumpPriority {dumpPriority:#x}, which sets \
                 no DUMP_FLAG_PRIORITY_* bit; it will not appear in listServices"
            );
        }

        // Detect `IAccessor` binders by interface descriptor at registration.
        // Hardcoding the AOSP-stable `android.os.IAccessor` string (instead of
        // pulling the `IAccessor` symbol) keeps rsb_hub buildable without the
        // `rpc` feature. NOTE: self-asserted by the registrant — see the fn
        // rustdoc for the trust caveat vs. AOSP's VINTF-derived accessors.
        let is_accessor = service.descriptor() == "android.os.IAccessor";

        // Capture the registrant's identity once on this binder thread, so
        // we can both stamp the new entry and detect cross-identity overwrites.
        let caller = rsbinder::thread_state::CallingContext::default();

        // `allowIsolated` is accepted by AIDL for AOSP source
        // compatibility but unused on Linux — there is no isolated-app
        // sandbox UID range to gate on.
        let _ = allowIsolated;

        // `client_pending` errors are swallowed (prior `onClients` path),
        // `reg_pending` errors are propagated (prior `onRegistration` used `?`).
        let mut client_pending = Vec::new();
        let mut reg_pending = Vec::new();
        let result: rsbinder::BinderResult<()> = (|| {
            let mut inner = lock_recover(&self.inner);

            // distinct-name DoS cap: refuse a *new* service name once the
            // registry is full (overwriting an existing name never grows the
            // map, so it is still allowed). See `MAX_DISTINCT_NAMES`.
            if distinct_name_cap_exceeded(&inner.name_to_service, name) {
                log::warn!(
                    "addService: service registry full (max {MAX_DISTINCT_NAMES} names), \
                     rejecting new name '{name}'"
                );
                return Err(ExceptionCode::IllegalState.into());
            }

            let mut prev_clients = false;
            // `SIBinder: PartialEq` is `Arc::ptr_eq`, so this is binder
            // identity: is the *same* object being re-registered under this
            // name?
            let mut same_binder = false;
            // The old binder being replaced, captured (cheap Arc clone) so it
            // can be unlinked *after* the new one is linked — see below.
            let mut old_to_unlink: Option<SIBinder> = None;
            if let Some(existing) = inner.name_to_service.get(name) {
                prev_clients = existing.has_clients;
                same_binder = existing.binder == *service;
                // Add-time access control (see fn rustdoc and
                // `is_cross_uid_overwrite_rejected`). A cross-UID overwrite of
                // a *different* live binder is the signature of a service-name
                // hijack (MITM): AOSP rejects app UIDs and runs the SELinux
                // canAddService hook, neither of which exists on a plain Linux
                // host, so reject it by default. A same-UID re-registration
                // (e.g. a service restart under a new PID) and re-registering
                // the identical binder are always allowed; the owning UID
                // dying frees the name via the death recipient, after which
                // any UID may claim it. `--allow-cross-uid-overwrite` opts
                // back into the prior permissive (warn-only) behavior.
                if Self::is_cross_uid_overwrite_rejected(
                    self.allow_cross_uid_overwrite,
                    same_binder,
                    existing.context.uid,
                    caller.uid,
                ) {
                    log::warn!(
                        "addService: rejecting cross-uid overwrite of '{name}' by uid={} \
                         pid={} (registered by uid={} pid={}); pass \
                         --allow-cross-uid-overwrite to permit",
                        caller.uid,
                        caller.pid,
                        existing.context.uid,
                        existing.context.pid
                    );
                    return Err(ExceptionCode::Security.into());
                }
                // The opt-in cross-uid overwrite path: proceed but log loudly.
                if self.allow_cross_uid_overwrite
                    && !same_binder
                    && existing.context.uid != caller.uid
                {
                    log::warn!(
                        "addService: '{name}' overwritten across uid by uid={} pid={} \
                         (previously uid={} pid={}); permitted by \
                         --allow-cross-uid-overwrite",
                        caller.uid,
                        caller.pid,
                        existing.context.uid,
                        existing.context.pid
                    );
                }
                if !same_binder {
                    old_to_unlink = Some(existing.binder.clone());
                }
            }

            // Take a reference on the new binder's subscription BEFORE
            // releasing the old one's: a failure here then leaves the
            // existing registration and its subscription intact (a clean
            // no-op) instead of stranding an unmonitored entry. Skipped when
            // the same binder is re-registered, since this entry's existing
            // reference carries over.
            if !same_binder {
                inner.retain_death_link(service)?;
            }

            if let Some(old) = old_to_unlink {
                inner.release_death_link(&old);
            }

            inner.add_service(
                name,
                Service {
                    binder: service.clone(),
                    dump_priority: dumpPriority,
                    has_clients: prev_clients,
                    guarantee_client: false,
                    context: caller,
                    is_accessor,
                },
            )?;

            if inner.name_to_registration_callbacks.contains_key(name) {
                if let Some(service) = inner.name_to_service.get_mut(name) {
                    service.guarantee_client = true;
                }

                inner.handle_service_client_callback(
                    Inner::KNOWN_CLIENTS_ON_DEMAND,
                    name,
                    false,
                    &mut client_pending,
                )?;

                if let Some(service) = inner.name_to_service.get_mut(name) {
                    service.guarantee_client = true;
                }

                let callbacks = inner
                    .name_to_registration_callbacks
                    .get(name)
                    .expect("name_to_registration_callbacks must have key");
                for callback in callbacks {
                    reg_pending.push(PendingCallback::Registration {
                        callback: callback.clone(),
                        name: name.to_owned(),
                        binder: service.clone(),
                    });
                }
            }

            Ok(())
        })();

        result?;
        fire_pending(client_pending);
        fire_pending_propagate(reg_pending)
    }

    /// Non-blocking, and free of side effects: unlike
    /// [`getService`](Self::getService) this never starts anything.
    fn checkService(&self, name: &str) -> rsbinder::BinderResult<Option<SIBinder>> {
        if !self.allows(Permission::Find, name) {
            return Ok(None);
        }
        let mut pending = Vec::new();
        let result = {
            let mut inner = lock_recover(&self.inner);
            inner
                .try_get_binder(name, &mut pending)?
                .map(|found| found.binder)
        };
        fire_pending(pending);
        Ok(result)
    }

    /// Two gates, unlike AOSP's single all-or-nothing `canList`: the
    /// caller must be allowed to enumerate at all, *and* each name it
    /// would learn about must be one it may `find`. The per-name filter
    /// mirrors what AOSP already does in `getUpdatableNames`
    /// (`ServiceManager.cpp:789-793`) — a name the caller could not look
    /// up is a name it has no business learning the existence of.
    fn listServices(&self, dump_priority: i32) -> rsbinder::BinderResult<Vec<String>> {
        self.require(Permission::List, "")?;

        // Collect under the lock, filter outside it: `allows` resolves
        // group membership, which can hit the name service.
        let candidates: Vec<String> = {
            let inner = lock_recover(&self.inner);
            inner
                .name_to_service
                .iter()
                .filter(|(_, service)| (service.dump_priority & dump_priority) != 0)
                .map(|(name, _)| name.clone())
                .collect()
        };

        Ok(candidates
            .into_iter()
            .filter(|name| self.allows(Permission::Find, name))
            .collect())
    }

    fn registerForNotifications(
        &self,
        name: &str,
        arg_callback: &rsbinder::Strong<
            dyn hub::android_16::android::os::IServiceCallback::IServiceCallback,
        >,
    ) -> rsbinder::BinderResult<()> {
        self.require(Permission::Find, name)?;

        if !Self::is_valid_service_name(name) {
            return Err(ExceptionCode::IllegalArgument.into());
        }

        let mut pending = Vec::new();
        {
            let mut inner = lock_recover(&self.inner);

            // Drop idempotent re-registrations and cap the list before
            // taking a death link or storing the callback (so neither leaks
            // on the rejected paths). See `MAX_CALLBACKS_PER_NAME`.
            if let Some(existing) = inner.name_to_registration_callbacks.get(name) {
                let cb_binder = arg_callback.as_binder();
                if existing.iter().any(|c| c.as_binder() == cb_binder) {
                    return Ok(());
                }
                if existing.len() >= MAX_CALLBACKS_PER_NAME {
                    let msg = format!(
                        "registerForNotifications: too many callbacks for {name} (max {MAX_CALLBACKS_PER_NAME})"
                    );
                    log::warn!("{}", msg);
                    return Err((ExceptionCode::IllegalState, msg.as_str()).into());
                }
            }

            // distinct-name DoS cap (orthogonal to the per-name cap above):
            // refuse a *new* name once the map is full. See `MAX_DISTINCT_NAMES`.
            if distinct_name_cap_exceeded(&inner.name_to_registration_callbacks, name) {
                let msg = format!(
                    "registerForNotifications: name registry full (max {MAX_DISTINCT_NAMES})"
                );
                log::warn!("{}", msg);
                return Err((ExceptionCode::IllegalState, msg.as_str()).into());
            }

            // One reference per (name, callback) entry; the idempotency
            // guard above means this cannot double-count a single name.
            inner.retain_death_link(&arg_callback.as_binder())?;

            inner
                .name_to_registration_callbacks
                .entry(name.to_string())
                .or_default()
                .push(arg_callback.clone());

            if let Some(service) = inner.name_to_service.get(name) {
                pending.push(PendingCallback::Registration {
                    callback: arg_callback.clone(),
                    name: name.to_owned(),
                    binder: service.binder.clone(),
                });
            }
        }
        fire_pending(pending);

        Ok(())
    }

    fn unregisterForNotifications(
        &self,
        name: &str,
        callback: &rsbinder::Strong<
            dyn hub::android_16::android::os::IServiceCallback::IServiceCallback,
        >,
    ) -> rsbinder::BinderResult<()> {
        self.require(Permission::Find, name)?;

        let mut inner = lock_recover(&self.inner);

        let binder = callback.as_binder();
        let removed = inner.remove_registration_callback(name, &binder);
        if removed == 0 {
            return Err(ExceptionCode::IllegalState.into());
        }
        // Release one reference per entry removed. Without this the
        // subscription outlives every registration that justified it, and
        // the next `registerForNotifications` for the same binder takes a
        // second one — unbounded growth across register/unregister cycles.
        for _ in 0..removed {
            inner.release_death_link(&binder);
        }
        Ok(())
    }

    /// Answered from the `[[service]]` entries in rsb_hub's configuration,
    /// which stand in for AOSP's VINTF manifests: both say which instances
    /// are expected to exist before anyone registers them, which is what
    /// lets a client tell "not installed" from "not started yet". A host
    /// that declares nothing gets `false` for everything, as before.
    fn isDeclared(&self, arg_name: &str) -> rsbinder::BinderResult<bool> {
        self.require(Permission::Find, arg_name)?;
        Ok(self.enforcer.config().declarations.is_declared(arg_name))
    }

    /// See [`isDeclared`](Self::isDeclared). Instances are filtered by
    /// `find`, as AOSP filters `getUpdatableNames`: an instance the caller
    /// could not look up is one it has no business learning about.
    fn getDeclaredInstances(&self, arg_iface: &str) -> rsbinder::BinderResult<Vec<String>> {
        let declarations = &self.enforcer.config().declarations;
        Ok(declarations
            .instances_of(arg_iface)
            .into_iter()
            .filter(|instance| self.allows(Permission::Find, &format!("{arg_iface}/{instance}")))
            .collect())
    }

    /// APEX (Android Pony EXpress) is an Android-only packaging
    /// system; there is no equivalent on a plain Linux host. Returning
    /// `None` truthfully reports "no APEX governs this service". Demoted
    /// to `debug` because under steady-state load every `getService`
    /// caller that asks may hit this — `warn` would flood the log.
    fn updatableViaApex(&self, arg_name: &str) -> rsbinder::BinderResult<Option<String>> {
        self.require(Permission::Find, arg_name)?;
        log::debug!("updatableViaApex is not implemented on Linux (APEX is Android-only)");
        Ok(None)
    }

    /// AOSP surfaces the VINTF `<ip>`+`<port>` of an AIDL service here
    /// (`getVintfConnectionInfo`); rsb_hub reads the same pair from a
    /// declaration's `connection` table. `None` when the instance is not
    /// declared or declared without one.
    fn getConnectionInfo(
        &self,
        arg_name: &str,
    ) -> rsbinder::BinderResult<Option<hub::android_16::android::os::ConnectionInfo::ConnectionInfo>>
    {
        self.require(Permission::Find, arg_name)?;
        Ok(self
            .enforcer
            .config()
            .declarations
            .connection_info(arg_name)
            .map(
                |info| hub::android_16::android::os::ConnectionInfo::ConnectionInfo {
                    ipAddress: info.ip.clone(),
                    port: info.port,
                },
            ))
    }

    fn registerClientCallback(
        &self,
        name: &str,
        arg_service: &rsbinder::SIBinder,
        arg_callback: &rsbinder::Strong<
            dyn hub::android_16::android::os::IClientCallback::IClientCallback,
        >,
    ) -> rsbinder::BinderResult<()> {
        self.require(Permission::Add, name)?;

        let mut pending = Vec::new();
        let result: rsbinder::BinderResult<()> = (|| {
            let mut inner = lock_recover(&self.inner);

            let service = if let Some(service) = inner.name_to_service.get(name) {
                service
            } else {
                let msg = format!("registerClientCallback could not find service {name}");
                log::warn!("{}", msg);
                return Err((ExceptionCode::IllegalArgument, msg.as_str()).into());
            };

            // AOSP `ServiceManager.cpp:911` answers this with
            // `EX_UNSUPPORTED_OPERATION`, not `EX_SECURITY`; the code is on
            // the wire, so a C++ `LazyServiceRegistrar` sees the difference.
            if service.context.pid != rsbinder::thread_state::CallingContext::default().pid {
                let msg = format!(
                    "{:?} Only a server can register for client callbacks (for {})",
                    service.context, name
                );
                log::warn!("{}", msg);
                return Err((ExceptionCode::UnsupportedOperation, msg.as_str()).into());
            }

            if service.binder != *arg_service {
                let msg = format!("registerClientCallback called with wrong service {name}");
                log::warn!("{}", msg);
                return Err((ExceptionCode::IllegalArgument, msg.as_str()).into());
            }

            // Copy what the rest of this function needs so the borrow on the
            // registry ends here; everything below mutates it.
            let service_binder = service.binder.clone();
            let service_has_clients = service.has_clients;

            // Drop idempotent re-registrations and cap the list before
            // taking a death link or storing the callback. See
            // `MAX_CALLBACKS_PER_NAME`.
            if let Some(existing) = inner.name_to_client_callbacks.get(name) {
                let cb_binder = arg_callback.as_binder();
                if existing.iter().any(|c| c.as_binder() == cb_binder) {
                    return Ok(());
                }
                if existing.len() >= MAX_CALLBACKS_PER_NAME {
                    let msg = format!(
                        "registerClientCallback: too many callbacks for {name} (max {MAX_CALLBACKS_PER_NAME})"
                    );
                    log::warn!("{}", msg);
                    return Err((ExceptionCode::IllegalState, msg.as_str()).into());
                }
            }

            // distinct-name DoS cap. Defense-in-depth: client callbacks require
            // a registered service (checked above), so this map's names are
            // already a subset of the service-name map, which is itself capped —
            // but bound it explicitly to stay robust. See `MAX_DISTINCT_NAMES`.
            if distinct_name_cap_exceeded(&inner.name_to_client_callbacks, name) {
                let msg = format!(
                    "registerClientCallback: name registry full (max {MAX_DISTINCT_NAMES})"
                );
                log::warn!("{}", msg);
                return Err((ExceptionCode::IllegalState, msg.as_str()).into());
            }

            // One reference per (name, callback) entry, as in
            // `registerForNotifications`. There is no unregister call for a
            // client callback, so these are released only when the binder
            // dies — matching AOSP, which likewise keeps them across a
            // `tryUnregisterService` so a reactivating lazy service does not
            // have to re-register.
            inner.retain_death_link(&arg_callback.as_binder())?;

            if service_has_clients {
                pending.push(PendingCallback::Clients {
                    callback: arg_callback.clone(),
                    binder: service_binder,
                    has_clients: true,
                });
            }

            inner
                .name_to_client_callbacks
                .entry(name.to_string())
                .or_default()
                .push(arg_callback.clone());

            inner.handle_service_client_callback(
                Inner::KNOWN_CLIENTS_ON_DEMAND,
                name,
                false,
                &mut pending,
            )?;

            Ok(())
        })();

        result?;
        fire_pending(pending);
        Ok(())
    }

    fn tryUnregisterService(
        &self,
        name: &str,
        arg_service: &rsbinder::SIBinder,
    ) -> rsbinder::BinderResult<()> {
        self.require(Permission::Add, name)?;

        let context = rsbinder::thread_state::CallingContext::default();

        let mut pending = Vec::new();
        let result: rsbinder::BinderResult<()> = (|| {
            let mut inner = lock_recover(&self.inner);
            let service = if let Some(service) = inner.name_to_service.get(name) {
                service
            } else {
                let msg = format!(
                    "{context:?} Tried to unregister {name}, but that service wasn't registered to begin with."
                );
                log::warn!("{}", msg);
                // AOSP `ServiceManager.cpp:1084`: EX_ILLEGAL_STATE.
                return Err((ExceptionCode::IllegalState, msg.as_str()).into());
            };

            // AOSP `ServiceManager.cpp:1090`: EX_UNSUPPORTED_OPERATION.
            if service.context.pid != rsbinder::thread_state::CallingContext::default().pid {
                let msg = format!(
                    "{:?} Only a server can unregister itself (for {})",
                    service.context, name
                );
                log::warn!("{}", msg);
                return Err((ExceptionCode::UnsupportedOperation, msg.as_str()).into());
            }

            if service.binder != *arg_service {
                let msg = format!("{context:?} Tried to unregister {name}, but a different service is registered under this name.");
                log::warn!("{}", msg);
                // AOSP `ServiceManager.cpp:1098`: EX_ILLEGAL_STATE.
                return Err((ExceptionCode::IllegalState, msg.as_str()).into());
            }

            if service.guarantee_client {
                let msg = format!(
                    "{context:?} Tried to unregister {name}, but there is about to be a client."
                );
                log::warn!("{}", msg);
                return Err((ExceptionCode::IllegalState, msg.as_str()).into());
            }

            // AOSP `ServiceManager.cpp:1111` checks the *return value*
            // (`bool` → "this service has clients, refuse to unregister").
            // The earlier port checked `res.is_err()` instead — but
            // `handle_service_client_callback` never returns `Err` in
            // current rsbinder (all "can't determine" paths fall back to
            // `Ok(true)`), so the refusal branch was dead and every
            // unregister request silently succeeded even with live clients.
            // `unwrap_or(true)` is defensive — if a future change makes
            // `Err` reachable, we conservatively treat it as "clients
            // present" (matches AOSP `ServiceManager.cpp:1001` `if (count
            // == -1) return true;`).
            let has_clients = inner
                .handle_service_client_callback(
                    Inner::KNOWN_CLIENTS_ON_DEMAND,
                    name,
                    false,
                    &mut pending,
                )
                .unwrap_or(true);
            if has_clients {
                let msg = format!("{context:?} Tried to unregister {name}, but there are clients.");
                log::warn!("{}", msg);
                if let Some(service) = inner.name_to_service.get_mut(name) {
                    service.guarantee_client = true;
                }
                return Err((ExceptionCode::IllegalState, msg.as_str()).into());
            }

            // AOSP `ServiceManager.cpp:1128`. The only log that separates an
            // honoured unregister from a death-driven cleanup, which is what
            // the entry disappearing looks like either way.
            log::info!("{context:?} Unregistering {name}");

            // Release this registration's reference on the subscription
            // before dropping the entry, so a register→tryUnregister cycle
            // (a lazy service idling and reactivating) is net-zero.
            inner.release_death_link(arg_service);
            inner.name_to_service.remove(name);

            Ok(())
        })();

        fire_pending(pending);
        result
    }

    fn getServiceDebugInfo(
        &self,
    ) -> rsbinder::BinderResult<Vec<hub::android_16::android::os::ServiceDebugInfo::ServiceDebugInfo>>
    {
        self.require(Permission::List, "")?;

        // See `listServices`: snapshot under the lock, filter outside it.
        let snapshot: Vec<(String, i32)> = {
            let inner = lock_recover(&self.inner);
            inner
                .name_to_service
                .iter()
                .map(|(name, service)| (name.clone(), service.context.pid))
                .collect()
        };

        Ok(snapshot
            .into_iter()
            .filter(|(name, _)| self.allows(Permission::Find, name))
            .map(|(name, debugPid)| {
                hub::android_16::android::os::ServiceDebugInfo::ServiceDebugInfo { name, debugPid }
            })
            .collect())
    }

    fn getService2(
        &self,
        name: &str,
    ) -> rsbinder::BinderResult<hub::android_16::android::os::Service::Service> {
        // Routing logic lives in `classify_for_service_union` so
        // `checkService2` stays byte-identical without re-stating the
        // match arms.
        if !self.allows(Permission::Find, name) {
            return Ok(classify_for_service_union(None));
        }
        let mut pending = Vec::new();
        let lookup = {
            let mut inner = lock_recover(&self.inner);
            inner.try_get_binder(name, &mut pending)?
        };
        fire_pending(pending);
        if lookup.is_none() {
            self.try_start_service(name);
        }
        Ok(classify_for_service_union(lookup))
    }

    fn checkService2(
        &self,
        name: &str,
    ) -> rsbinder::BinderResult<hub::android_16::android::os::Service::Service> {
        // See `getService2` — both route through
        // `classify_for_service_union`.
        if !self.allows(Permission::Find, name) {
            return Ok(classify_for_service_union(None));
        }
        let mut pending = Vec::new();
        let lookup = {
            let mut inner = lock_recover(&self.inner);
            inner.try_get_binder(name, &mut pending)?
        };
        fire_pending(pending);
        Ok(classify_for_service_union(lookup))
    }

    /// See [`updatableViaApex`](Self::updatableViaApex) — same
    /// APEX-on-Linux rationale, same demoted log level. An empty `Vec`
    /// is the truthful "no APEX-updatable services" answer.
    fn getUpdatableNames(&self, _apex_name: &str) -> rsbinder::BinderResult<Vec<String>> {
        log::debug!("getUpdatableNames is not implemented on Linux (APEX is Android-only)");
        Ok(vec![])
    }
}

/// Where `rsb_hub` looks for its configuration when `--config` is not
/// given. Every `*.toml` in it is loaded, sorted by file name.
const DEFAULT_CONFIG_DIR: &str = "/etc/rsbinder/hub.d";

/// Report an unloadable configuration and exit.
///
/// `rsb_hub` does not start without one. Falling back to a permissive mode
/// here would mean a typo in a config file silently drops all access
/// control on a running system — the one failure mode that must never be
/// quiet. `--insecure-allow-all` exists for the cases that genuinely want
/// no policy, and it has to be asked for by that name.
fn exit_without_config(path: &Path, err: &config::ConfigError) -> ! {
    eprintln!("rsb_hub: cannot load its configuration");
    eprintln!("  {err}");
    eprintln!();
    eprintln!("rsb_hub denies every request its configuration does not allow. Create a");
    eprintln!("file, for example {}/10-local.toml:", path.display());
    eprintln!();
    eprintln!("    [global]");
    eprintln!("    list = {{ group = [\"binder-admin\"] }}");
    eprintln!();
    eprintln!("    [[rule]]");
    eprintln!("    name = \"com.example.*\"");
    eprintln!("    add  = {{ user = [\"exampled\"] }}");
    eprintln!("    find = {{ group = [\"binder-clients\"] }}");
    eprintln!();
    eprintln!("    [[service]]");
    eprintln!("    name = \"com.example.IFoo/default\"");
    eprintln!("    start = {{ systemd = \"example-foo.service\" }}");
    eprintln!();
    eprintln!("It must not be writable by anyone but its owner: a start entry runs with");
    eprintln!("rsb_hub's privileges when a lookup misses.");
    eprintln!();
    eprintln!("Point rsb_hub at a different path with --config <PATH>, or pass");
    eprintln!("--insecure-allow-all to run with no access control (development only).");
    std::process::exit(1);
}

/// The signals `rsb_hub` consumes itself rather than dying from.
///
/// SIGHUP reloads the configuration; SIGTERM and SIGINT are a deliberate
/// stop. Handling the latter two is what lets the hub tell its supervisor
/// it is going down on purpose — a hub killed by the default disposition
/// exits "by signal", which systemd records as a failure and a reader of
/// the journal cannot tell from a crash.
const HANDLED_SIGNALS: [libc::c_int; 3] = [libc::SIGHUP, libc::SIGTERM, libc::SIGINT];

/// A `sigset_t` containing exactly [`HANDLED_SIGNALS`].
fn handled_signal_set() -> std::io::Result<libc::sigset_t> {
    // SAFETY: `sigemptyset` fully initializes the zeroed set before
    // `sigaddset` or any reader touches it, and both take a valid pointer
    // to a live local.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        if libc::sigemptyset(&mut set) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for signo in HANDLED_SIGNALS {
            if libc::sigaddset(&mut set, signo) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(set)
    }
}

/// Block [`HANDLED_SIGNALS`] in this thread, and therefore in every thread
/// spawned later.
///
/// `pthread_sigmask` is per-thread and a new thread inherits its creator's
/// mask, so doing this before anything else spawns is what makes the signal
/// thread's `sigwait` the single consumer. Without the block, each signal's
/// default disposition would simply kill rsb_hub.
fn block_handled_signals() -> std::io::Result<()> {
    let set = handled_signal_set()?;
    // SAFETY: `set` is initialized above and only read by the call; the
    // null `oldset` means "do not report the previous mask".
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

/// What a SIGHUP reloads, when there is a configuration to reload.
///
/// `None` under `--insecure-allow-all`: there is no configuration, so a
/// SIGHUP has nothing to do — but the thread still runs, because SIGTERM
/// must be handled in that mode too.
struct Reloadable {
    enforcer: Arc<Enforcer>,
    path: PathBuf,
}

/// Consume [`HANDLED_SIGNALS`] for the life of the process.
///
/// A failed reload **keeps the configuration already in force**. Dropping
/// to deny-all would take the machine's IPC down over a typo, and falling
/// back to permissive would do the opposite and worse; continuing with the
/// last known-good configuration is the only option that neither breaks nor
/// silently opens the system. The failure is logged at error level.
///
/// SIGTERM/SIGINT exit the process from this thread. There is nothing to
/// flush — the registry is in memory and dies with it, exactly as AOSP's
/// `servicemanager` does when init kills it — so "graceful" here means
/// telling the supervisor this was intentional and saying so in the log.
fn spawn_signal_thread(reloadable: Option<Reloadable>, notifier: Arc<Notifier>) {
    let spawn_result = std::thread::Builder::new()
        .name("rsb_hub:signals".to_owned())
        .spawn(move || {
            let set = match handled_signal_set() {
                Ok(set) => set,
                Err(e) => {
                    log::error!(
                        "rsb_hub: cannot build the signal set; reload and clean shutdown \
                         are disabled: {e}"
                    );
                    return;
                }
            };
            loop {
                let mut signo: libc::c_int = 0;
                // SAFETY: `set` is an initialized sigset that outlives the
                // call, and `signo` is a live local the call writes once.
                let rc = unsafe { libc::sigwait(&set, &mut signo) };
                if rc != 0 {
                    log::error!(
                        "rsb_hub: sigwait failed; reload and clean shutdown are disabled: {}",
                        std::io::Error::from_raw_os_error(rc)
                    );
                    return;
                }
                match signo {
                    libc::SIGHUP => reload(reloadable.as_ref(), &notifier),
                    // SIGTERM is `systemctl stop`; SIGINT is Ctrl-C from the
                    // terminal that started it. Same intent, same answer.
                    _ => {
                        let name = if signo == libc::SIGINT {
                            "SIGINT"
                        } else {
                            "SIGTERM"
                        };
                        log::info!("rsb_hub: {name} received, shutting down");
                        notifier.stopping("shutting down");
                        // Exit successfully: this was asked for. Nothing is
                        // pending that a longer unwind would finish, and the
                        // binder device is released by the kernel on exit.
                        std::process::exit(0);
                    }
                }
            }
        });
    if let Err(e) = spawn_result {
        log::error!("rsb_hub: failed to spawn the signal thread: {e}");
    }
}

/// One SIGHUP's worth of work.
fn reload(reloadable: Option<&Reloadable>, notifier: &Notifier) {
    let Some(Reloadable { enforcer, path }) = reloadable else {
        log::warn!("rsb_hub: SIGHUP ignored; running with --insecure-allow-all, no configuration");
        return;
    };
    match config::load(path, &SystemResolver) {
        Ok(loaded) => {
            let rules = loaded.policy.rules.len();
            let services = loaded.declarations.len();
            enforcer.replace(loaded);
            log::info!(
                "rsb_hub: SIGHUP reloaded {rules} rule(s) and {services} \
                 service declaration(s) from {}",
                path.display()
            );
            notifier.status(&format!(
                "serving; {rules} rule(s), {services} declaration(s)"
            ));
        }
        Err(err) => log::error!(
            "rsb_hub: SIGHUP reload of {} failed, keeping the policy already in \
             force: {err}",
            path.display()
        ),
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Both before anything spawns a thread: the signal mask is inherited by
    // every later thread, and `Notifier::from_environment` mutates the
    // environment, which is only sound while single-threaded.
    if let Err(e) = block_handled_signals() {
        eprintln!("rsb_hub: cannot block signals: {e}");
        std::process::exit(1);
    }
    let notifier = Arc::new(Notifier::from_environment());

    let matches = clap::Command::new("rsb_hub")
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about("A service manager for Binder IPC on Linux. Facilitates service registration, discovery, and management.")
        .arg(
            clap::Arg::new("device")
                .short('d')
                .long("device")
                .value_name("NAME")
                .help("Name of the binder device to use (e.g., 'binder', 'mybinder')")
                .default_value("binder"),
        )
        .arg(
            clap::Arg::new("config")
                .short('c')
                .long("config")
                .value_name("PATH")
                .help(
                    "Service manager configuration: a .toml file, or a directory of \
                     *.toml files loaded in file-name order \
                     (default: /etc/rsbinder/hub.d)",
                ),
        )
        .arg(
            clap::Arg::new("insecure-allow-all")
                .long("insecure-allow-all")
                .action(clap::ArgAction::SetTrue)
                .help(
                    "Run with NO access control: every caller may register, look up, \
                     and enumerate every service. Development and test use only",
                ),
        )
        .arg(
            clap::Arg::new("allow-cross-uid-overwrite")
                .long("allow-cross-uid-overwrite")
                .action(clap::ArgAction::SetTrue)
                .help(
                    "Permit a client to overwrite a service name registered by a \
                     different UID. Off by default: a cross-UID overwrite is the \
                     signature of a service-name hijack, so it is rejected unless \
                     this flag is set.",
                ),
        )
        .after_help(
            "Examples:\n    \
            Run with the default binder device:\n    \
            $ rsb_hub\n\n    \
            Run with a custom binder device:\n    \
            $ rsb_hub --device mybinder\n    \
            $ rsb_hub -d mybinder\n\n    \
            Run with configuration from somewhere other than /etc/rsbinder/hub.d:\n    \
            $ rsb_hub --config /usr/local/etc/rsbinder/hub.d\n\n    \
            Run with no access control (development only):\n    \
            $ rsb_hub --insecure-allow-all\n\n\
            Signals:\n    \
            SIGHUP           reload the configuration (a failed reload keeps the\n                     \
            one already in force)\n    \
            SIGTERM, SIGINT  stop, reporting a clean exit to the supervisor\n\n\
            Under systemd, use Type=notify: rsb_hub reports READY=1 only once it\n\
            holds handle 0, so units ordered After= it never race the registry.\n\n    \
            Note: The binder device must be created first using rsb_device.\n    \
            rsb_hub denies every request that its policy does not allow, and \n    \
            refuses to start when no policy can be loaded.",
        )
        .get_matches();

    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    let device_name = matches
        .get_one::<String>("device")
        .expect("device has a default value");
    let binder_path = format!("{}/{}", DEFAULT_BINDERFS_PATH, device_name);
    let allow_cross_uid_overwrite = matches.get_flag("allow-cross-uid-overwrite");
    if allow_cross_uid_overwrite {
        log::warn!(
            "rsb_hub: --allow-cross-uid-overwrite is set; clients may overwrite \
             services registered by other UIDs (service-hijack protection disabled)"
        );
    }

    let insecure_allow_all = matches.get_flag("insecure-allow-all");
    let config_path = matches.get_one::<String>("config").map(PathBuf::from);
    if insecure_allow_all && config_path.is_some() {
        eprintln!("rsb_hub: --config and --insecure-allow-all are mutually exclusive");
        std::process::exit(1);
    }

    let (enforcer, reloadable, ready_status) = if insecure_allow_all {
        // Loud on stderr as well as the log: this is a running service
        // manager with no access control, and the operator has to be able
        // to see that from the terminal that started it.
        eprintln!(
            "rsb_hub: WARNING --insecure-allow-all: no access control. Any local \
             process may register, look up, and enumerate any service."
        );
        log::warn!("rsb_hub: running with --insecure-allow-all; no access control is applied");
        log::warn!("rsb_hub: SIGHUP will be ignored (there is no policy to reload)");
        (
            Arc::new(Enforcer::allow_all()),
            None,
            "serving; NO ACCESS CONTROL (--insecure-allow-all)".to_owned(),
        )
    } else {
        let path = config_path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_DIR));
        let (enforcer, rules, services) = match config::load(&path, &SystemResolver) {
            Ok(loaded) => {
                let (rules, services) = (loaded.policy.rules.len(), loaded.declarations.len());
                log::info!(
                    "rsb_hub: loaded {rules} rule(s) and {services} service declaration(s) from {}",
                    path.display()
                );
                (Arc::new(Enforcer::enforcing(loaded)), rules, services)
            }
            Err(err) => exit_without_config(&path, &err),
        };
        (
            Arc::clone(&enforcer),
            Some(Reloadable { enforcer, path }),
            format!("serving; {rules} rule(s), {services} declaration(s)"),
        )
    };
    spawn_signal_thread(reloadable, Arc::clone(&notifier));

    log::info!("Starting rsb_hub with binder device: {}", binder_path);

    // 0 = AOSP's `setThreadPoolMaxThreadCount(0)`: rsb_hub is deliberately
    // single-threaded, exactly like `servicemanager`, and the kernel is now
    // told exactly that.
    ProcessState::init(&binder_path, 0)?;

    // Create a binder service.
    let service = BnServiceManager::new_binder(ServiceManager::new(
        allow_cross_uid_overwrite,
        Arc::clone(&enforcer),
    ));
    // Log and carry on, as AOSP does (`main.cpp`: "Could not self register
    // servicemanager"). Clients reach the hub through handle 0 regardless;
    // the registry entry is a convenience, and refusing to come up without
    // it would take the machine's IPC down for a cosmetic failure.
    if let Err(e) = service.addService(
        SELF_SERVICE_NAME,
        &service.as_binder(),
        false,
        DUMP_FLAG_PRIORITY_DEFAULT,
    ) {
        log::error!("rsb_hub: could not self-register as '{SELF_SERVICE_NAME}': {e:?}");
    }

    // A binder device has exactly one context manager, and the kernel is
    // what enforces it — a lock file here would be a second, weaker truth.
    // What is worth adding is a legible failure: the raw ioctl error says
    // nothing about the one thing that is almost always the cause.
    if let Err(e) = ProcessState::as_self().become_context_manager(service.as_binder()) {
        eprintln!("rsb_hub: cannot become the service manager for {binder_path}: {e}");
        eprintln!();
        eprintln!("A binder device has exactly one service manager, so this usually means");
        eprintln!("one is already running on {binder_path}. Check with:");
        eprintln!("    rsb_service --device {device_name} check manager");
        eprintln!();
        eprintln!("To run a second, independent service manager, give it its own device:");
        eprintln!("    sudo rsb_device other && rsb_hub --device other");
        std::process::exit(1);
    }

    // Only now: handle 0 is ours, so this is the first instant at which a
    // client can reach the hub. Announcing readiness any earlier would let
    // systemd release units ordered `After=` into a race.
    log::info!("rsb_hub: serving on {binder_path}");
    notifier.ready(&ready_status);

    Ok(ProcessState::join_thread_pool()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local binder, purely as a source of distinct `WIBinder` identities
    /// for the death-link tests — every `fake_binder()` is its own
    /// allocation and so its own identity. Never transacted on.
    struct FakeBinder;

    impl Interface for FakeBinder {}

    impl Remotable for FakeBinder {
        fn descriptor() -> &'static str {
            "rsbinder.test.hub.IFake"
        }
        fn on_transact(
            &self,
            _code: TransactionCode,
            _reader: &mut Parcel,
            _reply: &mut Parcel,
        ) -> rsbinder::Result<()> {
            Err(rsbinder::StatusCode::UnknownTransaction)
        }
        fn on_dump(&self, _w: &mut dyn std::io::Write, _args: &[String]) -> rsbinder::Result<()> {
            Ok(())
        }
    }

    fn fake_binder() -> SIBinder {
        Interface::as_binder(&rsbinder::Binder::new(FakeBinder))
    }

    fn test_inner() -> Inner {
        let (tx, _rx) = mpsc::channel();
        Inner::new(tx)
    }

    /// Counts how many times the kernel half of a retain/release would run.
    #[derive(Default)]
    struct LinkLedger {
        links: std::cell::Cell<usize>,
        unlinks: std::cell::Cell<usize>,
    }

    impl LinkLedger {
        fn link(&self) -> rsbinder::BinderResult<()> {
            self.links.set(self.links.get() + 1);
            Ok(())
        }
        fn unlink(&self) {
            self.unlinks.set(self.unlinks.get() + 1);
        }
    }

    /// H-1b: one binder registered under many names must take **one**
    /// kernel subscription, so its death fires `binder_died` once and
    /// triggers one cleanup sweep — not one per name. Releasing all but the
    /// last name must not drop the subscription the remaining name needs.
    #[test]
    fn one_subscription_per_binder_regardless_of_name_count() {
        let mut inner = test_inner();
        let ledger = LinkLedger::default();
        let weak = SIBinder::downgrade(&fake_binder());

        for _ in 0..5 {
            inner
                .retain_counted(7, &weak, || ledger.link())
                .expect("retain must succeed");
        }
        assert_eq!(ledger.links.get(), 1, "five names, one subscription");
        assert_eq!(inner.death_links[&7].count, 5);

        for _ in 0..4 {
            inner.release_counted(7, || ledger.unlink());
        }
        assert_eq!(ledger.unlinks.get(), 0, "one name still depends on it");
        assert!(inner.death_links.contains_key(&7));

        inner.release_counted(7, || ledger.unlink());
        assert_eq!(ledger.unlinks.get(), 1, "last release drops it");
        assert!(!inner.death_links.contains_key(&7));
    }

    /// H-1: the register/unregister cycle that used to leak. Each iteration
    /// re-links because the previous unregister removed the callback from
    /// the map without releasing the subscription, so a client could grow
    /// the recipient list — and the per-death sweep count — without bound.
    /// Paired accounting must keep it flat.
    #[test]
    fn register_unregister_cycles_do_not_accumulate_subscriptions() {
        let mut inner = test_inner();
        let ledger = LinkLedger::default();
        let weak = SIBinder::downgrade(&fake_binder());

        for _ in 0..100 {
            inner
                .retain_counted(9, &weak, || ledger.link())
                .expect("retain must succeed");
            inner.release_counted(9, || ledger.unlink());
        }

        assert_eq!(ledger.links.get(), 100, "one link per cycle");
        assert_eq!(ledger.unlinks.get(), 100, "and one unlink per cycle");
        assert!(
            inner.death_links.is_empty(),
            "no residue after the last release"
        );
    }

    /// A failed link must leave no accounting behind: a later release would
    /// otherwise unlink a subscription that was never taken.
    #[test]
    fn failed_link_records_nothing() {
        let mut inner = test_inner();
        let weak = SIBinder::downgrade(&fake_binder());

        let err = inner
            .retain_counted(3, &weak, || Err(ExceptionCode::IllegalState.into()))
            .expect_err("link failure must propagate");
        assert_eq!(err.exception_code(), ExceptionCode::IllegalState);
        assert!(inner.death_links.is_empty());

        // And the release that follows a failed retain is inert.
        let ledger = LinkLedger::default();
        inner.release_counted(3, || ledger.unlink());
        assert_eq!(ledger.unlinks.get(), 0);
    }

    /// H-2: an obituary retires the dead binder's record — whatever its
    /// count, since the kernel released the subscription itself — and
    /// attempts no unlink. Only that binder's record goes.
    ///
    /// The identity comparison this rests on is pinned in `rsbinder`
    /// (`proxy_downgrade_keeps_its_identity_without_a_cache_entry`); the
    /// registry-sweeping half needs proxies with real handles and lives in
    /// `tests/scripts/run_hub_policy_ac.sh`.
    #[test]
    fn obituary_finds_its_record_by_the_stored_weak() {
        let mut inner = test_inner();
        let ledger = LinkLedger::default();
        let dead = fake_binder();
        let live = fake_binder();
        let dead_weak = SIBinder::downgrade(&dead);
        let live_weak = SIBinder::downgrade(&live);

        inner
            .retain_counted(1, &dead_weak, || ledger.link())
            .unwrap();
        inner
            .retain_counted(1, &dead_weak, || ledger.link())
            .unwrap();
        inner
            .retain_counted(2, &live_weak, || ledger.link())
            .unwrap();

        let retired = inner.retire_dead_binder(&dead_weak);
        assert_eq!(retired, (0, 0), "no registrations exist in this test");

        assert!(!inner.death_links.contains_key(&1), "dead record removed");
        assert!(inner.death_links.contains_key(&2), "live record kept");
        assert_eq!(ledger.unlinks.get(), 0, "the obituary path must not unlink");
    }

    /// An obituary for a binder nothing tracks must be inert, not a panic
    /// and not a sweep keyed on a handle it guessed.
    #[test]
    fn obituary_for_an_untracked_binder_is_inert() {
        let mut inner = test_inner();
        let stranger = SIBinder::downgrade(&fake_binder());
        assert_eq!(inner.retire_dead_binder(&stranger), (0, 0));
        assert!(inner.death_links.is_empty());
    }

    /// A native (`Bn*`) binder has no death notification, so both halves are
    /// no-ops. Callers rely on this to avoid testing for it themselves.
    #[test]
    fn native_binders_are_skipped() {
        let mut inner = test_inner();
        let native = fake_binder();
        assert!(native.as_proxy().is_none(), "precondition");

        inner
            .retain_death_link(&native)
            .expect("retain on a native binder must be a no-op");
        assert!(inner.death_links.is_empty());
        inner.release_death_link(&native);
        assert!(inner.death_links.is_empty());
    }

    /// M-1: `isLazyService` must reflect `FLAG_IS_LAZY_SERVICE` in the
    /// registered dumpPriority. AOSP's client uses it to decide whether the
    /// binder may be cached, and a lazy service withdraws via
    /// `tryUnregisterService` *without dying* — so a stuck `false` hands out
    /// a cached binder to a service that is gone.
    #[test]
    fn lazy_flag_reaches_the_service_union() {
        use hub::android_16::android::os::Service::Service;

        let lazy = classify_for_service_union(Some(Lookup {
            binder: fake_binder(),
            is_accessor: false,
            is_lazy: true,
        }));
        match lazy {
            Service::ServiceWithMetadata(swm) => assert!(swm.isLazyService),
            other => panic!("expected ServiceWithMetadata, got {other:?}"),
        }

        let eager = classify_for_service_union(Some(Lookup {
            binder: fake_binder(),
            is_accessor: false,
            is_lazy: false,
        }));
        match eager {
            Service::ServiceWithMetadata(swm) => assert!(!swm.isLazyService),
            other => panic!("expected ServiceWithMetadata, got {other:?}"),
        }

        // An accessor has no metadata arm to carry the flag.
        let accessor = classify_for_service_union(Some(Lookup {
            binder: fake_binder(),
            is_accessor: true,
            is_lazy: true,
        }));
        assert!(matches!(accessor, Service::Accessor(Some(_))));
    }

    /// The lazy bit is the one AOSP defines, and it is outside
    /// `DUMP_FLAG_PRIORITY_ALL` — so a lazy registration still needs a
    /// priority bit of its own to appear in `listServices`.
    #[test]
    fn lazy_flag_is_disjoint_from_the_priority_mask() {
        assert_eq!(FLAG_IS_LAZY_SERVICE, 1 << 30);
        assert_eq!(FLAG_IS_LAZY_SERVICE & DUMP_FLAG_PRIORITY_ALL, 0);
    }

    /// The self-registration bypass in [`ServiceManager::allows`] rests
    /// entirely on `calling_caller()` being `None` outside a transaction.
    /// If that ever changed, `rsb_hub` would authorize its own startup
    /// `addService("manager")` against the caller's own uid and refuse to
    /// start under any policy that does not happen to grant it — a
    /// startup failure with a very confusing message. Pin the premise.
    #[test]
    fn calling_caller_is_none_outside_a_transaction() {
        assert!(
            rsbinder::calling_caller().is_none(),
            "no transaction is in flight on a test thread"
        );
    }

    /// Add-time access control: a cross-UID overwrite of a different live
    /// binder is rejected by default; same-UID restarts, identical-binder
    /// re-registration, and the explicit opt-in are all allowed. (The
    /// real two-UID transaction path is exercised by deployment; this pins
    /// the decision predicate deterministically.)
    #[test]
    fn cross_uid_overwrite_decision() {
        // Reject: different UID, different binder, flag off (hijack).
        assert!(ServiceManager::is_cross_uid_overwrite_rejected(
            false, false, 1000, 2000
        ));
        // Allow: same UID (e.g. a service restart under a new PID).
        assert!(!ServiceManager::is_cross_uid_overwrite_rejected(
            false, false, 1000, 1000
        ));
        // Allow: re-registering the identical binder, even across UIDs.
        assert!(!ServiceManager::is_cross_uid_overwrite_rejected(
            false, true, 1000, 2000
        ));
        // Allow: operator opted into cross-UID overwrites.
        assert!(!ServiceManager::is_cross_uid_overwrite_rejected(
            true, false, 1000, 2000
        ));
        // Allow: opt-in + same UID (trivially permitted).
        assert!(!ServiceManager::is_cross_uid_overwrite_rejected(
            true, false, 1000, 1000
        ));
    }

    #[test]
    fn test_is_valid_service_name_accepts_valid() {
        assert!(ServiceManager::is_valid_service_name("test"));
        assert!(ServiceManager::is_valid_service_name("test-"));
        assert!(ServiceManager::is_valid_service_name("test_"));
        assert!(ServiceManager::is_valid_service_name("test."));
        assert!(ServiceManager::is_valid_service_name("test/"));
        assert!(ServiceManager::is_valid_service_name("test0"));
        assert!(ServiceManager::is_valid_service_name("test1"));
        assert!(ServiceManager::is_valid_service_name("TEST2"));
        // 127-char boundary — the inclusive upper bound.
        let max_len = "a".repeat(127);
        assert!(ServiceManager::is_valid_service_name(&max_len));
        // The legacy AOSP service names that motivate the
        // permitted-charset list — all should pass.
        assert!(ServiceManager::is_valid_service_name(
            "android.os.IServiceManager"
        ));
        assert!(ServiceManager::is_valid_service_name(
            "android.hardware.audio.IDevice/default"
        ));
    }

    #[test]
    fn test_is_valid_service_name_rejects_empty() {
        assert!(!ServiceManager::is_valid_service_name(""));
    }

    #[test]
    fn test_is_valid_service_name_rejects_too_long() {
        // Just past the 127-char bound.
        let too_long = "a".repeat(128);
        assert!(!ServiceManager::is_valid_service_name(&too_long));
        let way_too_long = "a".repeat(1024);
        assert!(!ServiceManager::is_valid_service_name(&way_too_long));
    }

    #[test]
    fn test_is_valid_service_name_rejects_disallowed_chars() {
        // Whitespace.
        assert!(!ServiceManager::is_valid_service_name("test name"));
        assert!(!ServiceManager::is_valid_service_name("test\tname"));
        assert!(!ServiceManager::is_valid_service_name("test\nname"));
        // ASCII punctuation outside the `_-./` allowlist.
        assert!(!ServiceManager::is_valid_service_name("test:name"));
        assert!(!ServiceManager::is_valid_service_name("test,name"));
        assert!(!ServiceManager::is_valid_service_name("test@name"));
        assert!(!ServiceManager::is_valid_service_name("test+name"));
        assert!(!ServiceManager::is_valid_service_name("test*name"));
        assert!(!ServiceManager::is_valid_service_name("test\\name"));
        // Control / NUL.
        assert!(!ServiceManager::is_valid_service_name("test\0name"));
        // Non-ASCII (Unicode lowercase letters not in `[a-z]`).
        assert!(!ServiceManager::is_valid_service_name("테스트"));
        assert!(!ServiceManager::is_valid_service_name("café"));
    }

    /// HUB-1: the distinct-name cap rejects a *new* name once a registry map is
    /// full, but always allows overwriting an existing name (which does not
    /// grow the map). Bounds the unbounded heap growth a client could cause by
    /// looping over distinct service names — the per-name cap does not cover
    /// the distinct-name axis.
    fn dump_row(name: &str, dump_priority: i32) -> DumpRow {
        DumpRow {
            name: name.to_owned(),
            pid: 4242,
            uid: 1000,
            dump_priority,
            has_clients: false,
            guarantee_client: false,
            is_accessor: false,
            registration_callbacks: 0,
            client_callbacks: 0,
        }
    }

    fn dump_to_string(snap: &DumpSnapshot) -> String {
        let mut out = Vec::new();
        render_dump(&mut out, snap).expect("a Vec never fails to write");
        String::from_utf8(out).expect("the renderer only writes UTF-8")
    }

    fn empty_snapshot() -> DumpSnapshot {
        DumpSnapshot {
            services: Vec::new(),
            awaited: Vec::new(),
            death_subscriptions: 0,
            rules: Some(0),
            declarations: 0,
            filter: Vec::new(),
            hidden: 0,
        }
    }

    /// A registration's flags have to be readable *as flags*, not just as
    /// the hex the client sent: `lazy=yes` is the difference between "this
    /// binder may be cached" and "it may not".
    #[test]
    fn a_dump_reports_each_registration_and_its_flags() {
        let mut snap = empty_snapshot();
        snap.services = vec![
            dump_row("com.example.IFoo/default", DUMP_FLAG_PRIORITY_DEFAULT),
            dump_row(
                "com.example.ILazy/default",
                DUMP_FLAG_PRIORITY_DEFAULT | FLAG_IS_LAZY_SERVICE,
            ),
        ];
        snap.declarations = 2;
        snap.death_subscriptions = 2;
        let out = dump_to_string(&snap);

        assert!(out.contains("services (2):"), "{out}");
        assert!(out.contains("com.example.IFoo/default"), "{out}");
        assert!(out.contains("pid=4242 uid=1000"), "{out}");
        assert!(out.contains("declarations: 2"), "{out}");
        assert!(out.contains("death subscriptions: 2"), "{out}");
        // Exactly one of the two is lazy.
        assert_eq!(out.matches("lazy=yes").count(), 1, "{out}");
        assert_eq!(out.matches("lazy=no").count(), 1, "{out}");
    }

    /// L-3's warning, on the read side: a registration with no priority bit
    /// never appears in `listServices`, which looks like a lost
    /// registration until you see the mask.
    #[test]
    fn a_registration_with_no_priority_bit_is_called_out() {
        let mut snap = empty_snapshot();
        snap.services = vec![dump_row("com.example.IFoo/default", 0)];
        let out = dump_to_string(&snap);
        assert!(out.contains("no priority bit"), "{out}");

        snap.services = vec![dump_row("com.example.IFoo/default", DUMP_FLAG_PRIORITY_ALL)];
        assert!(!dump_to_string(&snap).contains("no priority bit"));
    }

    /// "Nothing is registered and something is waiting for it" is the state
    /// an operator debugs most often, so it gets its own section rather
    /// than being invisible.
    #[test]
    fn names_that_are_only_waited_on_get_their_own_section() {
        let mut snap = empty_snapshot();
        snap.awaited = vec![("com.example.IBar/default".to_owned(), 2, 1)];
        let out = dump_to_string(&snap);
        assert!(out.contains("awaiting registration (1):"), "{out}");
        assert!(
            out.contains("com.example.IBar/default  callbacks: registration=2 client=1"),
            "{out}"
        );

        // ... and it is absent, not empty, when nothing is waiting.
        assert!(!dump_to_string(&empty_snapshot()).contains("awaiting registration"));
    }

    /// `--insecure-allow-all` is the one fact about a running hub that a
    /// dump must never bury: everything else in the output is the same
    /// whether or not access control is on.
    #[test]
    fn allow_all_is_reported_as_disabled_access_control() {
        let mut snap = empty_snapshot();
        snap.rules = None;
        let out = dump_to_string(&snap);
        assert!(out.contains("access control: DISABLED"), "{out}");

        snap.rules = Some(7);
        let out = dump_to_string(&snap);
        assert!(
            out.contains("access control: enforcing, 7 rule(s)"),
            "{out}"
        );
    }

    /// Names withheld by the policy are counted, never listed: the count
    /// tells the operator the view is partial without telling a denied
    /// caller which names exist.
    #[test]
    fn withheld_names_are_counted_not_named() {
        let mut snap = empty_snapshot();
        snap.hidden = 3;
        snap.filter = vec!["com.example".to_owned()];
        let out = dump_to_string(&snap);
        assert!(out.contains("hidden by policy: 3 name(s)"), "{out}");
        assert!(out.contains("filter: com.example"), "{out}");
        // Neither line appears when there is nothing to say.
        let out = dump_to_string(&empty_snapshot());
        assert!(!out.contains("hidden by policy"), "{out}");
        assert!(!out.contains("filter:"), "{out}");
    }

    #[test]
    fn the_dump_filter_is_an_or_of_substrings() {
        assert!(dump_filter_matches(&[], "anything"));
        let filter = vec!["IFoo".to_owned(), "manager".to_owned()];
        assert!(dump_filter_matches(&filter, "com.example.IFoo/default"));
        assert!(dump_filter_matches(&filter, "manager"));
        assert!(!dump_filter_matches(&filter, "com.example.IBar/default"));
    }

    /// The mask has to cover *shutdown* as well as reload: a signal that
    /// reaches the thread but is not in the set blocked at startup would
    /// have already killed the process by its default disposition.
    #[test]
    fn the_blocked_signal_set_covers_reload_and_shutdown() {
        let set = handled_signal_set().expect("building a sigset cannot fail here");
        for signo in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
            assert!(
                HANDLED_SIGNALS.contains(&signo),
                "signal {signo} must be handled"
            );
            // SAFETY: `set` is a live, fully initialized sigset and
            // `sigismember` only reads it.
            assert_eq!(unsafe { libc::sigismember(&set, signo) }, 1);
        }
        // And nothing else: blocking a signal nobody consumes would make it
        // silently ineffective instead of doing what the operator expects.
        // SAFETY: as above.
        assert_eq!(unsafe { libc::sigismember(&set, libc::SIGUSR1) }, 0);
    }

    #[test]
    fn distinct_name_cap_rejects_new_but_allows_existing() {
        // Below capacity: any name is allowed.
        let mut small: BTreeMap<String, ()> = BTreeMap::new();
        small.insert("a".to_owned(), ());
        assert!(!distinct_name_cap_exceeded(&small, "a"));
        assert!(!distinct_name_cap_exceeded(&small, "brand-new"));

        // Fill exactly to capacity with distinct names.
        let mut full: BTreeMap<String, ()> = BTreeMap::new();
        for i in 0..MAX_DISTINCT_NAMES {
            full.insert(format!("svc.{i}"), ());
        }
        assert_eq!(full.len(), MAX_DISTINCT_NAMES);

        // A new name is rejected once full...
        assert!(distinct_name_cap_exceeded(&full, "one-too-many"));
        // ...but overwriting an existing name is still allowed.
        assert!(!distinct_name_cap_exceeded(&full, "svc.0"));
        assert!(!distinct_name_cap_exceeded(
            &full,
            &format!("svc.{}", MAX_DISTINCT_NAMES - 1)
        ));
    }
}
