// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
#![expect(non_snake_case, reason = "AIDL parameter names (`allowIsolated`, ..)")]

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
    context: rsbinder::CallingContext,
}

/// Collected under the `Inner` lock, fired after it drops: no outbound transaction under it (R1).
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

/// Errors are logged, not returned: one dead callback must not fail the call that fired it.
fn fire_pending(pending: Vec<PendingCallback>) {
    for cb in pending {
        cb.fire()
            .unwrap_or_else(|e| log::error!("Failed to notify client callback: {e:?}"));
    }
}

struct DeathRecipientWrapper(mpsc::Sender<rsbinder::WIBinder>);

impl rsbinder::DeathRecipient for DeathRecipientWrapper {
    fn binder_died(&self, who: &rsbinder::WIBinder) {
        self.0.send(who.clone()).unwrap_or_else(|e| {
            log::error!("Failed to send death notification: {e:?}");
        });
    }
}

/// Handlers run under `catch_unwind`, so one panic would poison every later request; recover.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

use config::{RESERVED_SERVICE_NAMES as RESERVED_NAMES, SELF_SERVICE_NAME};

/// Refused to every transaction's `add`, `allow_all` too.
fn is_reserved(name: &str) -> bool {
    RESERVED_NAMES.contains(&name)
}

/// Per-name callback cap, in place of AOSP's SELinux/uid admission that rsb_hub lacks.
const MAX_CALLBACKS_PER_NAME: usize = 256;

/// Distinct names per registry map: the per-name cap does not stop looping over new names.
const MAX_DISTINCT_NAMES: usize = 10_000;

/// Overwriting an existing key never grows the map, so only a new key can exceed the cap.
fn distinct_name_cap_exceeded<V>(map: &BTreeMap<String, V>, name: &str) -> bool {
    !map.contains_key(name) && map.len() >= MAX_DISTINCT_NAMES
}

/// AOSP `ServiceManager.cpp`'s message for `addService` and `registerClientCallback`.
const LINK_FAILED: &str = "Couldn't linkToDeath.";

/// AOSP `ServiceManager.cpp`'s message for `registerForNotifications`.
const CALLBACK_LINK_FAILED: &str = "Couldn't link to death.";

/// AOSP `ServiceManager.cpp`'s answer; `DeadObject` would read as the hub's own death.
fn link_failed(handle: u32, status: StatusCode, message: &str) -> Status {
    log::error!("could not link to the death of handle {handle}: {status:?}");
    Status::from((ExceptionCode::IllegalState, message))
}

/// Weak so it cannot outlive its registry entries; it is also what an obituary matches on.
struct DeathLink {
    weak: rsbinder::WIBinder,
    count: usize,
}

struct Inner {
    death_recipient: Arc<DeathRecipientWrapper>,
    /// Link on 0→1, unlink on 1→0: `link_to_death` does not dedupe, so ad hoc pairing drifts.
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
    /// The in-flight transaction's ref plus ours; AOSP `kKnownClients = 2`.
    const KNOWN_CLIENTS_ON_DEMAND: usize = 2;

    /// The poller runs outside any transaction, so only our own ref; AOSP passes `1` there too.
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

    /// Pair each `Ok` with one `release_death_link` or `retire_dead_binder`; native = no-op.
    fn retain_death_link(
        &mut self,
        binder: &SIBinder,
        refusal: &str,
    ) -> rsbinder::BinderResult<()> {
        let Some(handle) = binder.as_proxy().map(|proxy| proxy.handle()) else {
            return Ok(());
        };
        let recipient: Arc<dyn rsbinder::DeathRecipient> = self.death_recipient.clone();
        self.retain_counted(handle, &SIBinder::downgrade(binder), refusal, || {
            binder.link_to_death(Arc::downgrade(&recipient))
        })
    }

    /// Kernel call injected so tests can count links; a failed `link` records nothing.
    fn retain_counted(
        &mut self,
        handle: u32,
        weak: &rsbinder::WIBinder,
        refusal: &str,
        link: impl FnOnce() -> rsbinder::Result<()>,
    ) -> rsbinder::BinderResult<()> {
        if let Some(existing) = self.death_links.get_mut(&handle) {
            existing.count += 1;
            return Ok(());
        }
        link().map_err(|status| link_failed(handle, status, refusal))?;
        self.death_links.insert(
            handle,
            DeathLink {
                weak: weak.clone(),
                count: 1,
            },
        );
        Ok(())
    }

    /// Unlinks on the last release; an unpaired link lives until the handle's last proxy drops.
    fn release_death_link(&mut self, binder: &SIBinder) {
        let Some(handle) = binder.as_proxy().map(|proxy| proxy.handle()) else {
            return;
        };
        let recipient: Arc<dyn rsbinder::DeathRecipient> = self.death_recipient.clone();
        self.release_counted(handle, || {
            if let Err(e) = binder.unlink_to_death(Arc::downgrade(&recipient)) {
                // Died before this call: the ordinary race on every service restart.
                if e == rsbinder::StatusCode::DeadObject {
                    log::debug!("death notification for handle {handle} was already gone");
                } else {
                    log::warn!("failed to unlink death notification for handle {handle}: {e:?}");
                }
            }
        });
    }

    /// A release with no matching entry is ignored rather than underflowing.
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

    /// Collects `onClients` into `pending`; the caller fires them after dropping the guard (R1).
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
            // AOSP aborts (`CHECK_NE`); losing the hub takes all IPC down, so log instead.
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

    /// AOSP `handleServiceClientCallback`, collecting callbacks into `pending`.
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

        // No kernel refcount for a local binder: over-estimate, as AOSP's `count == -1`.
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
                // Temporary, as AOSP `ServiceManager.cpp:1004`; `has_clients` moves only on notify.
                service.guarantee_client = false;
                has_clients = service.has_clients;
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

    /// No `start_if_not_found` as in AOSP: this runs under the lock; callers start after it.
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
        let is_lazy = service.dump_priority & FLAG_IS_LAZY_SERVICE != 0;
        service.guarantee_client = true;
        self.handle_service_client_callback(Self::KNOWN_CLIENTS_ON_DEMAND, name, false, pending)?;

        if let Some(service) = self.name_to_service.get_mut(name) {
            service.guarantee_client = true;
        }

        Ok(Some(Lookup {
            binder: out,
            is_lazy,
        }))
    }

    /// Returns a count: each removed entry holds one death-link reference to release.
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

    /// Matching relies on `SIBinder::downgrade` not reading the proxy cache the obituary clears.
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

        // AOSP `binderDied`'s third loop; else `onClients` keeps firing at a dead proxy.
        self.name_to_client_callbacks.retain(|_, entries| {
            entries.retain(|callback| *who != callback.as_binder());
            !entries.is_empty()
        });

        // No unlink: the kernel dropped the subscription when it sent the obituary.
        self.death_links.retain(|_, link| link.weak != *who);

        (services, callbacks)
    }
}

struct ServiceManager {
    inner: Arc<Mutex<Inner>>,
    /// Held from collecting to firing, taken before `inner`: oneway fires keep their order.
    fire_order: Arc<Mutex<()>>,
    /// `--allow-cross-uid-overwrite`; off, a cross-UID overwrite is refused as a hijack.
    allow_cross_uid_overwrite: bool,
    /// Consulted before the registry by every entry point; `plans/6-1-hub-access-control.md`.
    enforcer: Arc<Enforcer>,
    /// Brings a declared service up when a lookup misses it.
    activator: Activator,
    /// The topic registry `main` registers; read by `dump` only.
    #[cfg(feature = "topic")]
    topics: Option<rsbinder_topic::registry::Registry>,
    /// Set while a dump writer holds a caller's fd; one only, as an unread pipe fills any number.
    dump_writer: DumpWriter,
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
            fire_order: Arc::new(Mutex::new(())),
            allow_cross_uid_overwrite,
            enforcer,
            activator,
            #[cfg(feature = "topic")]
            topics: None,
            dump_writer: Arc::new(Mutex::new(None)),
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
                    // Logged at zero too: retiring nothing is how a broken match shows.
                    log::info!(
                        "binder died: retired {services} service name(s) and \
                         {callbacks} registration callback(s)"
                    );
                }
            });
        if let Err(e) = spawn_result {
            log::error!("Failed to spawn death receiver thread, exiting: {e}");
            std::process::exit(1);
        }
    }

    /// AOSP `ClientCallbackCallback`: the only source of `onClients(false)` for lazy services.
    fn run_client_callback_poller(&self) {
        /// AOSP `main.cpp` timerfd interval.
        const INTERVAL: Duration = Duration::from_secs(5);

        let inner_weak = Arc::downgrade(&self.inner);
        let fire_order = Arc::clone(&self.fire_order);
        let spawn_result = std::thread::Builder::new()
            .name("rsb_hub:cbpoll".to_owned())
            .spawn(move || loop {
                std::thread::sleep(INTERVAL);

                let Some(inner_arc) = inner_weak.upgrade() else {
                    log::debug!("client callback poller exiting (ServiceManager dropped)");
                    return;
                };

                let order = lock_recover(&fire_order);
                let mut inner = lock_recover(&inner_arc);

                // Snapshot: the loop body mutates `name_to_service`.
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
                drop(order);
            });
        if let Err(e) = spawn_result {
            log::error!("Failed to spawn client callback poller thread, exiting: {e}");
            std::process::exit(1);
        }
    }

    fn is_valid_service_name(name: &str) -> bool {
        config::is_valid_service_name(name)
    }

    /// No caller = startup self-registration only; hub threads must not get a bypass (6-1 D10).
    fn allows(&self, permission: Permission, name: &str) -> bool {
        self.allows_as(rsbinder::calling_caller().as_ref(), permission, name)
    }

    /// [`Self::allows`] with the caller read once, not re-read per name by a filter.
    fn allows_as(
        &self,
        caller: Option<&rsbinder::Caller>,
        permission: Permission,
        name: &str,
    ) -> bool {
        self.allows_in(&self.enforcer.config(), caller, permission, name)
    }

    /// [`Self::allows_as`] against a config snapshot one transaction keeps for every judgement.
    fn allows_in(
        &self,
        config: &config::Config,
        caller: Option<&rsbinder::Caller>,
        permission: Permission,
        name: &str,
    ) -> bool {
        let add = permission == Permission::Add;
        match caller {
            // A reserved name taken over would hand every client the taker's binder.
            Some(_) if add && is_reserved(name) => false,
            Some(caller) => self
                .enforcer
                .check_caller_in(config, permission, name, caller),
            None => add && (name == SELF_SERVICE_NAME || is_reserved(name)),
        }
    }

    /// AOSP `canFindService`/`canAddService`: the name, and the accessor it declares if any.
    fn allows_service_in(
        &self,
        config: &config::Config,
        caller: Option<&rsbinder::Caller>,
        permission: Permission,
        name: &str,
    ) -> bool {
        self.refused_in(config, caller, permission, name).is_none()
    }

    /// The first of the name and its declared accessor the policy refuses; one rule for every call.
    fn refused_in<'a>(
        &self,
        config: &'a config::Config,
        caller: Option<&rsbinder::Caller>,
        permission: Permission,
        name: &'a str,
    ) -> Option<&'a str> {
        if !self.allows_in(config, caller, permission, name) {
            return Some(name);
        }
        config
            .declarations
            .accessor(name)
            .filter(|accessor| !self.allows_in(config, caller, permission, accessor))
    }

    /// AOSP `tryStartService`, `getService*` only; an accessor's miss starts the instance.
    fn try_start_service_in(&self, config: &config::Config, name: &str) {
        match config.declarations.activation(name) {
            Some(activation) => self.activator.try_start(name, activation),
            None if config.declarations.is_declared(name) => {
                log::debug!("{name} is declared but has no `start`; not starting it")
            }
            None => log::debug!("{name} is not declared; not starting it"),
        }
    }

    /// AOSP `tryGetService`: looks the declared accessor up in the instance's stead; judges both.
    fn try_get_service(
        &self,
        name: &str,
        start: bool,
    ) -> rsbinder::BinderResult<hub::android_16::android::os::Service::Service> {
        let config = self.enforcer.config();
        let caller = rsbinder::calling_caller();
        let accessor = config.declarations.accessor(name);
        if !self.allows_service_in(&config, caller.as_ref(), Permission::Find, name) {
            return Ok(classify_for_service_union(None, accessor.is_some()));
        }
        let order = lock_recover(&self.fire_order);
        let mut pending = Vec::new();
        let lookup = {
            let mut inner = lock_recover(&self.inner);
            inner.try_get_binder(accessor.unwrap_or(name), &mut pending)?
        };
        fire_pending(pending);
        drop(order);
        if lookup.is_none() && start {
            self.try_start_service_in(&config, name);
        }
        Ok(classify_for_service_union(lookup, accessor.is_some()))
    }

    /// Judges the name and its declared accessor; not for lookups (AOSP answers those with null).
    fn require(&self, permission: Permission, name: &str) -> rsbinder::BinderResult<()> {
        self.require_in(&self.enforcer.config(), permission, name)
    }

    /// [`Self::require`] against a snapshot the transaction also answers from.
    fn require_in(
        &self,
        config: &config::Config,
        permission: Permission,
        name: &str,
    ) -> rsbinder::BinderResult<()> {
        let caller = rsbinder::calling_caller();
        let Some(target) = self.refused_in(config, caller.as_ref(), permission, name) else {
            return Ok(());
        };
        let ctx = rsbinder::CallingContext::current();
        let why = if permission == Permission::Add && is_reserved(target) {
            "rsb_hub reserves the name and refused"
        } else {
            "policy denied"
        };
        // The caller learns no accessor name, as AOSP's "SELinux denied for the accessor".
        let of = if target == name {
            format!("{name:?}")
        } else {
            format!("the accessor of {name:?}")
        };
        let wire = format!(
            "{why} {permission} on {of} for uid={} (pid={})",
            ctx.uid, ctx.pid
        );
        log::warn!(
            "{why} {permission} on {target:?} ({of}) for uid={} (pid={})",
            ctx.uid,
            ctx.pid
        );
        Err((ExceptionCode::Security, wire.as_str()).into())
    }

    /// Pure so the hijack check is testable without two real UIDs.
    fn is_cross_uid_overwrite_rejected(
        allow_cross_uid_overwrite: bool,
        same_binder: bool,
        existing_uid: u32,
        caller_uid: u32,
    ) -> bool {
        !allow_cross_uid_overwrite && !same_binder && existing_uid != caller_uid
    }
}

/// The topic registry's policy: `[[topic]]` and `[global] topics` through the hub's enforcer.
#[cfg(feature = "topic")]
struct TopicAccess(Arc<Enforcer>);

#[cfg(feature = "topic")]
impl rsbinder_topic::registry::RegistryAccess for TopicAccess {
    fn allows(
        &self,
        action: rsbinder_topic::registry::Action,
        name: &str,
        caller: &rsbinder::Caller,
    ) -> bool {
        use rsbinder_topic::registry::Action;
        let permission = match action {
            Action::Advertise => config::TopicPermission::Advertise,
            Action::Subscribe => config::TopicPermission::Subscribe,
            Action::List => config::TopicPermission::List,
            // `Action` is `#[non_exhaustive]`: what this hub does not know, it refuses.
            _ => return false,
        };
        self.0.check_topic(permission, name, caller)
    }
}

/// What a dump says of the topic registry.
#[cfg(feature = "topic")]
struct TopicDump {
    /// Whether the registry's name holds the registry's binder.
    registered: bool,
    /// `None` when running under `--insecure-allow-all`.
    rules: Option<usize>,
    stats: rsbinder_topic::registry::RegistryStats,
}

/// Copied out of the lock: the `find` filter can hit the name service.
struct DumpRow {
    name: String,
    pid: i32,
    uid: u32,
    dump_priority: i32,
    has_clients: bool,
    guarantee_client: bool,
    /// A declaration names this as the accessor of an instance.
    is_accessor: bool,
    registration_callbacks: usize,
    client_callbacks: usize,
}

/// Everything [`render_dump`] needs, so it is testable without a registry or binder.
struct DumpSnapshot {
    services: Vec<DumpRow>,
    /// Names with callbacks held but nothing registered.
    awaited: Vec<(String, usize, usize)>,
    death_subscriptions: usize,
    /// `None` when running under `--insecure-allow-all`.
    rules: Option<usize>,
    declarations: usize,
    /// The `args` the caller passed, if they narrowed the listing.
    filter: Vec<String>,
    /// Names the snapshot dropped because the caller may not `find` them.
    hidden: usize,
    #[cfg(feature = "topic")]
    topics: Option<TopicDump>,
}

/// Empty passes all; else a substring of any arg, so the hub needs no pattern syntax.
fn dump_filter_matches(filter: &[String], name: &str) -> bool {
    filter.is_empty() || filter.iter().any(|f| name.contains(f.as_str()))
}

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
    #[cfg(feature = "topic")]
    if let Some(topics) = &snap.topics {
        render_topics(w, topics)?;
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

/// One `key=value` per counter, so a script reads `notices_failed` without parsing prose.
#[cfg(feature = "topic")]
fn render_topics(w: &mut dyn std::io::Write, topics: &TopicDump) -> std::io::Result<()> {
    let name = rsbinder_topic::aidl::ITopicRegistry::SERVICE_NAME;
    let registered = if topics.registered {
        "registered"
    } else {
        "NOT registered"
    };
    match topics.rules {
        Some(rules) => writeln!(
            w,
            "topic registry: {registered} as {name}, {rules} topic rule(s)"
        )?,
        None => writeln!(
            w,
            "topic registry: {registered} as {name}, access control DISABLED"
        )?,
    }
    let s = &topics.stats;
    writeln!(
        w,
        "  topics={} watched={} watchers={} notices_sent={} notices_failed={} \
         notices_retried={} owed={} owed_dropped={}",
        s.topics,
        s.watched,
        s.watchers,
        s.notices_sent,
        s.notices_failed,
        s.notices_retried,
        s.owed,
        s.owed_dropped,
    )
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

impl ServiceManager {
    /// The `find` filter runs outside the lock: it can hit the name service (R1).
    fn dump_snapshot(
        &self,
        config: &config::Config,
        caller: Option<&rsbinder::Caller>,
        filter: Vec<String>,
    ) -> DumpSnapshot {
        /// Generic because the two maps hold different callback traits.
        fn count<V>(map: &BTreeMap<String, Vec<V>>, name: &str) -> usize {
            map.get(name).map_or(0, |v| v.len())
        }

        #[cfg(feature = "topic")]
        let mut registered = false;
        let (mut rows, mut awaited, death_subscriptions) = {
            let inner = lock_recover(&self.inner);
            #[cfg(feature = "topic")]
            if let Some(registry) = &self.topics {
                let name = rsbinder_topic::aidl::ITopicRegistry::SERVICE_NAME;
                registered = inner
                    .name_to_service
                    .get(name)
                    .is_some_and(|service| service.binder == registry.as_binder());
            }
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
                    is_accessor: config.declarations.accessor_for(name).is_some(),
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

        // Caller's filter first, so `hidden` counts only what the policy withheld.
        rows.retain(|row| dump_filter_matches(&filter, &row.name));
        awaited.retain(|(name, _, _)| dump_filter_matches(&filter, name));

        let before = rows.len() + awaited.len();
        rows.retain(|row| self.allows_in(config, caller, Permission::Find, &row.name));
        awaited.retain(|(name, _, _)| self.allows_in(config, caller, Permission::Find, name));
        let hidden = before - rows.len() - awaited.len();

        DumpSnapshot {
            services: rows,
            awaited,
            death_subscriptions,
            rules: (!self.enforcer.is_allow_all()).then_some(config.policy.rules.len()),
            declarations: config.declarations.len(),
            filter,
            hidden,
            #[cfg(feature = "topic")]
            topics: self.topics.as_ref().map(|registry| TopicDump {
                registered,
                rules: (!self.enforcer.is_allow_all()).then_some(config.topics.rules.len()),
                stats: registry.stats(),
            }),
        }
    }
}

impl Interface for ServiceManager {
    /// Gated by `list`, each name by `find` too, else it would bypass the per-name policy.
    fn dump_fd(&self, fd: std::os::fd::OwnedFd, args: &[String]) -> rsbinder::Result<()> {
        let ctx = rsbinder::CallingContext::current();
        let config = self.enforcer.config();
        let caller = rsbinder::calling_caller();
        if !self.allows_in(&config, caller.as_ref(), Permission::List, "") {
            log::warn!(
                "policy denied dump (list) for uid={} (pid={})",
                ctx.uid,
                ctx.pid
            );
            // Also to the fd: its only channel, else a denial looks like an empty registry.
            let text = format!(
                "rsb_hub: policy denies `list` to uid={} (pid={})\n",
                ctx.uid, ctx.pid
            );
            let _ = send_dump(&self.dump_writer, fd, text.into_bytes(), &ctx);
            return Err(StatusCode::PermissionDenied);
        }
        let mut text = Vec::new();
        render_dump(
            &mut text,
            &self.dump_snapshot(&config, caller.as_ref(), args.to_vec()),
        )
        .map_err(|_| StatusCode::NoMemory)?;
        send_dump(&self.dump_writer, fd, text, &ctx)
    }
}

/// The caller whose dump a writer thread is writing, as `uid=.. (pid=..)`; `None` when idle.
type DumpWriter = Arc<Mutex<Option<String>>>;

/// Writes on a thread of its own: a caller that never reads then holds it, not the binder thread.
fn send_dump(
    writer: &DumpWriter,
    fd: std::os::fd::OwnedFd,
    text: Vec<u8>,
    caller: &rsbinder::CallingContext,
) -> rsbinder::Result<()> {
    let who = format!("uid={} (pid={})", caller.uid, caller.pid);
    {
        let mut slot = lock_recover(writer);
        if let Some(holder) = slot.as_deref() {
            log::warn!("dump: refused {who}, the dump of {holder} is still being written");
            return Err(StatusCode::WouldBlock);
        }
        *slot = Some(who.clone());
    }
    let slot = Arc::clone(writer);
    let spawned = std::thread::Builder::new()
        .name("rsb_hub:dump".to_owned())
        .spawn(move || {
            let mut file = std::fs::File::from(fd);
            if let Err(e) = std::io::Write::write_all(&mut file, &text) {
                log::error!("dump: writing to the fd of {who} failed: {e}");
            }
            // Closed before the slot frees: the caller's end-of-file is the dump's end.
            drop(file);
            *lock_recover(&slot) = None;
        });
    if let Err(e) = spawned {
        *lock_recover(writer) = None;
        log::error!("dump: cannot spawn the writer thread: {e}");
        return Err(StatusCode::NoMemory);
    }
    Ok(())
}

/// A registered service, as `getService2`/`checkService2` need to see it.
struct Lookup {
    binder: SIBinder,
    /// Clients must not cache a lazy binder: it can unregister without dying.
    is_lazy: bool,
}

/// `accessor`: the requested name declares one, so the answer is the accessor arm, found or not.
fn classify_for_service_union(
    lookup: Option<Lookup>,
    accessor: bool,
) -> hub::android_16::android::os::Service::Service {
    use hub::android_16::android::os::{Service, ServiceWithMetadata};
    if accessor {
        return Service::Service::Accessor(lookup.map(|found| found.binder));
    }
    match lookup {
        Some(Lookup { binder, is_lazy }) => {
            Service::Service::ServiceWithMetadata(ServiceWithMetadata::ServiceWithMetadata {
                service: Some(binder),
                isLazyService: is_lazy,
            })
        }
        // libbinder tries local accessors on this arm only; rsbinder on `accessor(null)` too.
        None => Service::Service::ServiceWithMetadata(ServiceWithMetadata::ServiceWithMetadata {
            service: None,
            isLazyService: false,
        }),
    }
}

impl IServiceManager for ServiceManager {
    /// Starts a declared service on a miss, as AOSP; `checkService` must not.
    fn getService(&self, name: &str) -> rsbinder::BinderResult<Option<rsbinder::SIBinder>> {
        let config = self.enforcer.config();
        let caller = rsbinder::calling_caller();
        if !self.allows_in(&config, caller.as_ref(), Permission::Find, name) {
            return Ok(None);
        }
        let order = lock_recover(&self.fire_order);
        let mut pending = Vec::new();
        let result = {
            let mut inner = lock_recover(&self.inner);
            inner
                .try_get_binder(name, &mut pending)?
                .map(|found| found.binder)
        };
        fire_pending(pending);
        drop(order);
        if result.is_none() {
            self.try_start_service_in(&config, name);
        }
        Ok(result)
    }

    /// Makes no call on `service`: a declaration, not its descriptor, says what is an accessor.
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

        // AOSP accepts it too, but no priority bit hides it from every `listServices` filter.
        if dumpPriority & DUMP_FLAG_PRIORITY_ALL == 0 {
            log::warn!(
                "addService: '{name}' registered with dumpPriority {dumpPriority:#x}, which sets \
                 no DUMP_FLAG_PRIORITY_* bit; it will not appear in listServices"
            );
        }

        let caller = rsbinder::CallingContext::current();

        // Linux has no isolated-app UID range to gate on.
        let _ = allowIsolated;

        let _order = lock_recover(&self.fire_order);
        let mut client_pending = Vec::new();
        let mut reg_pending = Vec::new();
        let result: rsbinder::BinderResult<()> = (|| {
            let mut inner = lock_recover(&self.inner);

            if distinct_name_cap_exceeded(&inner.name_to_service, name) {
                log::warn!(
                    "addService: service registry full (max {MAX_DISTINCT_NAMES} names), \
                     rejecting new name '{name}'"
                );
                return Err(ExceptionCode::IllegalState.into());
            }

            let mut prev_clients = false;
            // `SIBinder: PartialEq` is `Arc::ptr_eq`: binder identity.
            let mut same_binder = false;
            let mut old_to_unlink: Option<SIBinder> = None;
            if let Some(existing) = inner.name_to_service.get(name) {
                prev_clients = existing.has_clients;
                same_binder = existing.binder == *service;
                // Stands in for AOSP's app-UID and SELinux `canAddService` checks.
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

            // Retain before release: a failed link then leaves the old registration intact.
            if !same_binder {
                inner.retain_death_link(service, LINK_FAILED)?;
            }

            if let Some(old) = old_to_unlink {
                inner.release_death_link(&old);
            }

            // Same binder keeps the owner, else any `add`er holding it takes the uid/pid checks.
            if same_binder {
                if let Some(existing) = inner.name_to_service.get_mut(name) {
                    if existing.context.uid == caller.uid || self.allow_cross_uid_overwrite {
                        existing.dump_priority = dumpPriority;
                        existing.guarantee_client = false;
                    } else {
                        log::warn!(
                            "addService: uid={} re-added '{name}' (owned by uid={}); keeping the owner's flags",
                            caller.uid,
                            existing.context.uid
                        );
                    }
                }
            } else {
                inner.add_service(
                    name,
                    Service {
                        binder: service.clone(),
                        dump_priority: dumpPriority,
                        has_clients: prev_clients,
                        guarantee_client: false,
                        context: caller,
                    },
                )?;
            }

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
        fire_pending(reg_pending);
        Ok(())
    }

    /// Non-blocking; never starts anything, but sets `guarantee_client` as AOSP does.
    fn checkService(&self, name: &str) -> rsbinder::BinderResult<Option<SIBinder>> {
        if !self.allows(Permission::Find, name) {
            return Ok(None);
        }
        let _order = lock_recover(&self.fire_order);
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

    /// `list`, then `find` per name, as AOSP's `getUpdatableNames` filters.
    fn listServices(&self, dump_priority: i32) -> rsbinder::BinderResult<Vec<String>> {
        let config = self.enforcer.config();
        self.require_in(&config, Permission::List, "")?;

        // Filter outside the lock: `allows_in` can hit the name service.
        let candidates: Vec<String> = {
            let inner = lock_recover(&self.inner);
            inner
                .name_to_service
                .iter()
                .filter(|(_, service)| (service.dump_priority & dump_priority) != 0)
                .map(|(name, _)| name.clone())
                .collect()
        };

        let caller = rsbinder::calling_caller();
        Ok(candidates
            .into_iter()
            .filter(|name| self.allows_in(&config, caller.as_ref(), Permission::Find, name))
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

        let _order = lock_recover(&self.fire_order);
        let mut pending = Vec::new();
        {
            let mut inner = lock_recover(&self.inner);

            // Before the death link, so a rejected call leaks nothing.
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

            if distinct_name_cap_exceeded(&inner.name_to_registration_callbacks, name) {
                let msg = format!(
                    "registerForNotifications: name registry full (max {MAX_DISTINCT_NAMES})"
                );
                log::warn!("{}", msg);
                return Err((ExceptionCode::IllegalState, msg.as_str()).into());
            }

            // One reference per (name, callback) entry.
            inner.retain_death_link(&arg_callback.as_binder(), CALLBACK_LINK_FAILED)?;

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
        for _ in 0..removed {
            inner.release_death_link(&binder);
        }
        Ok(())
    }

    /// Config `[[service]]` entries stand in for AOSP's VINTF manifests.
    fn isDeclared(&self, arg_name: &str) -> rsbinder::BinderResult<bool> {
        let config = self.enforcer.config();
        self.require_in(&config, Permission::Find, arg_name)?;
        Ok(config.declarations.is_declared(arg_name))
    }

    /// Filtered by `find`, as AOSP `getDeclaredInstances` filters by `canFindService`.
    fn getDeclaredInstances(&self, arg_iface: &str) -> rsbinder::BinderResult<Vec<String>> {
        let config = self.enforcer.config();
        let all = config.declarations.instances_of(arg_iface);
        let declared = all.len();
        let caller = rsbinder::calling_caller();
        let allowed: Vec<String> = all
            .into_iter()
            .filter(|instance| {
                self.allows_service_in(
                    &config,
                    caller.as_ref(),
                    Permission::Find,
                    &format!("{arg_iface}/{instance}"),
                )
            })
            .collect();
        // AOSP `ServiceManager.cpp:748`: all filtered out is a denial, not "none declared".
        if allowed.is_empty() && declared != 0 {
            let ctx = rsbinder::CallingContext::current();
            let msg = format!(
                "policy denied find on every instance of {arg_iface:?} for uid={} (pid={})",
                ctx.uid, ctx.pid
            );
            log::warn!("{msg}");
            return Err((ExceptionCode::Security, msg.as_str()).into());
        }
        Ok(allowed)
    }

    /// No APEX on Linux; `debug`, not `warn`, since callers may ask on every lookup.
    fn updatableViaApex(&self, arg_name: &str) -> rsbinder::BinderResult<Option<String>> {
        self.require(Permission::Find, arg_name)?;
        log::debug!("updatableViaApex is not implemented on Linux (APEX is Android-only)");
        Ok(None)
    }

    /// A declaration's `connection` table stands in for AOSP's VINTF `<ip>`/`<port>`.
    fn getConnectionInfo(
        &self,
        arg_name: &str,
    ) -> rsbinder::BinderResult<Option<hub::android_16::android::os::ConnectionInfo::ConnectionInfo>>
    {
        let config = self.enforcer.config();
        self.require_in(&config, Permission::Find, arg_name)?;
        Ok(config.declarations.connection_info(arg_name).map(|info| {
            hub::android_16::android::os::ConnectionInfo::ConnectionInfo {
                ipAddress: info.ip.clone(),
                port: info.port,
            }
        }))
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

        let _order = lock_recover(&self.fire_order);
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

            // AOSP `ServiceManager.cpp:911`: EX_UNSUPPORTED_OPERATION, visible on the wire.
            if service.context.pid != rsbinder::CallingContext::current().pid {
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

            let service_binder = service.binder.clone();
            let service_has_clients = service.has_clients;

            // Before the death link, so a rejected call leaks nothing.
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

            if distinct_name_cap_exceeded(&inner.name_to_client_callbacks, name) {
                let msg = format!(
                    "registerClientCallback: name registry full (max {MAX_DISTINCT_NAMES})"
                );
                log::warn!("{}", msg);
                return Err((ExceptionCode::IllegalState, msg.as_str()).into());
            }

            // Released only on death: kept across `tryUnregisterService`, as in AOSP.
            inner.retain_death_link(&arg_callback.as_binder(), LINK_FAILED)?;

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

        let context = rsbinder::CallingContext::current();

        let _order = lock_recover(&self.fire_order);
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
            if service.context.pid != rsbinder::CallingContext::current().pid {
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

            // true = has clients; `Err` counts as true, as AOSP's `count == -1`.
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

            // The only log telling an honoured unregister from a death cleanup (AOSP :1128).
            log::info!("{context:?} Unregistering {name}");

            // Keeps a lazy service's register/tryUnregister cycle net-zero on subscriptions.
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
        let config = self.enforcer.config();
        self.require_in(&config, Permission::List, "")?;

        // See `listServices`: snapshot under the lock, filter outside it.
        let snapshot: Vec<(String, i32)> = {
            let inner = lock_recover(&self.inner);
            inner
                .name_to_service
                .iter()
                .map(|(name, service)| (name.clone(), service.context.pid))
                .collect()
        };

        let caller = rsbinder::calling_caller();
        Ok(snapshot
            .into_iter()
            .filter(|(name, _)| self.allows_in(&config, caller.as_ref(), Permission::Find, name))
            .map(|(name, debugPid)| {
                hub::android_16::android::os::ServiceDebugInfo::ServiceDebugInfo { name, debugPid }
            })
            .collect())
    }

    fn getService2(
        &self,
        name: &str,
    ) -> rsbinder::BinderResult<hub::android_16::android::os::Service::Service> {
        self.try_get_service(name, true)
    }

    fn checkService2(
        &self,
        name: &str,
    ) -> rsbinder::BinderResult<hub::android_16::android::os::Service::Service> {
        self.try_get_service(name, false)
    }

    /// No APEX on Linux; see `updatableViaApex`.
    fn getUpdatableNames(&self, _apex_name: &str) -> rsbinder::BinderResult<Vec<String>> {
        log::debug!("getUpdatableNames is not implemented on Linux (APEX is Android-only)");
        Ok(vec![])
    }
}

const DEFAULT_CONFIG_DIR: &str = "/etc/rsbinder/hub.d";

/// No permissive fallback: a config typo must not silently drop access control.
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
    eprintln!("It and every directory above it must be owned by root or by rsb_hub's user, and");
    eprintln!("writable by no one else (so not under /tmp): a start entry runs with rsb_hub's");
    eprintln!("privileges when a lookup misses.");
    eprintln!();
    eprintln!("Point rsb_hub at a different path with --config <PATH>, or pass");
    eprintln!("--insecure-allow-all to run with no access control (development only).");
    std::process::exit(1);
}

/// SIGTERM/SIGINT handled so systemd does not record a deliberate stop as a crash.
const HANDLED_SIGNALS: [libc::c_int; 3] = [libc::SIGHUP, libc::SIGTERM, libc::SIGINT];

fn handled_signal_set() -> std::io::Result<libc::sigset_t> {
    // SAFETY: `sigemptyset` initializes the set before any other use; pointers are to a live local.
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

/// Must run before any spawn: threads inherit the mask, making `sigwait` the sole consumer.
fn block_handled_signals() -> std::io::Result<()> {
    let set = handled_signal_set()?;
    // SAFETY: `set` is initialized and only read; a null `oldset` is allowed and means "no report".
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

/// Absent under `--insecure-allow-all`; the signal thread still runs for SIGTERM.
struct Reloadable {
    enforcer: Arc<Enforcer>,
    path: PathBuf,
}

/// A failed reload keeps the config in force: deny-all breaks IPC, permissive opens it.
fn spawn_signal_thread(reloadable: Option<Reloadable>, notifier: Arc<Notifier>) {
    let spawn_result = std::thread::Builder::new()
        .name("rsb_hub:signals".to_owned())
        .spawn(move || {
            let set = match handled_signal_set() {
                Ok(set) => set,
                Err(e) => {
                    log::error!("rsb_hub: cannot build the signal set, exiting: {e}");
                    std::process::exit(1);
                }
            };
            loop {
                let mut signo: libc::c_int = 0;
                // SAFETY: `set` is initialized and outlives the call; `signo` is a live local.
                let rc = unsafe { libc::sigwait(&set, &mut signo) };
                // Exit, not return: the signals stay blocked, so nothing else could stop us.
                if rc != 0 {
                    log::error!(
                        "rsb_hub: sigwait failed, exiting: {}",
                        std::io::Error::from_raw_os_error(rc)
                    );
                    std::process::exit(1);
                }
                match signo {
                    libc::SIGHUP => reload(reloadable.as_ref(), &notifier),
                    // SIGTERM (`systemctl stop`) or SIGINT (Ctrl-C): same intent.
                    _ => {
                        let name = if signo == libc::SIGINT {
                            "SIGINT"
                        } else {
                            "SIGTERM"
                        };
                        log::info!("rsb_hub: {name} received, shutting down");
                        notifier.stopping("shutting down");
                        // Nothing to flush: the registry is in memory, as in AOSP.
                        std::process::exit(0);
                    }
                }
            }
        });
    if let Err(e) = spawn_result {
        eprintln!("rsb_hub: failed to spawn the signal thread: {e}");
        std::process::exit(1);
    }
}

/// What a loaded configuration holds, for the startup and reload logs and the status line.
fn summary(config: &config::Config) -> String {
    let topics = if cfg!(feature = "topic") {
        format!(", {} topic rule(s)", config.topics.rules.len())
    } else {
        String::new()
    };
    format!(
        "{} rule(s){topics}, {} service declaration(s)",
        config.policy.rules.len(),
        config.declarations.len()
    )
}

/// One SIGHUP's worth of work.
fn reload(reloadable: Option<&Reloadable>, notifier: &Notifier) {
    let Some(Reloadable { enforcer, path }) = reloadable else {
        log::warn!("rsb_hub: SIGHUP ignored; running with --insecure-allow-all, no configuration");
        return;
    };
    match config::load(path, &SystemResolver) {
        Ok(loaded) => {
            let summary = summary(&loaded);
            enforcer.replace(loaded);
            log::info!("rsb_hub: SIGHUP reloaded {summary} from {}", path.display());
            notifier.status(&format!("serving; {summary}"));
        }
        Err(err) => log::error!(
            "rsb_hub: SIGHUP reload of {} failed, keeping the policy already in \
             force: {err}",
            path.display()
        ),
    }
}

/// Aborts on a panic off `main` and start threads, like AOSP's FATAL_IF_NOT_ONEWAY (plan 16 Q3).
fn abort_on_a_background_panic() {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        let thread = std::thread::current();
        if !matches!(thread.name(), Some("main" | config::START_THREAD_NAME)) {
            log::error!("rsb_hub: a panic on {:?} ends the hub", thread.name());
            std::process::abort();
        }
    }));
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Before any thread: the mask is inherited, and `from_environment` mutates the env.
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
    abort_on_a_background_panic();

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
        // Stderr too, so the operator sees it from the terminal that started it.
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
        let (enforcer, summary) = match config::load(&path, &SystemResolver) {
            Ok(loaded) => {
                let summary = summary(&loaded);
                log::info!("rsb_hub: loaded {summary} from {}", path.display());
                (Arc::new(Enforcer::enforcing(loaded)), summary)
            }
            Err(err) => exit_without_config(&path, &err),
        };
        (
            Arc::clone(&enforcer),
            Some(Reloadable { enforcer, path }),
            format!("serving; {summary}"),
        )
    };
    spawn_signal_thread(reloadable, Arc::clone(&notifier));

    log::info!("Starting rsb_hub with binder device: {}", binder_path);

    // 0 = AOSP's `setThreadPoolMaxThreadCount(0)`: single-threaded like `servicemanager`.
    ProcessState::init(&binder_path, 0)?;
    // AOSP `main.cpp`. Before any thread uses binder, so each copies it (plan 16 D1 S2).
    ProcessState::as_self().set_call_restriction(CallRestriction::FatalIfNotOneway);

    #[cfg_attr(
        not(feature = "topic"),
        expect(unused_mut, reason = "only `topic` sets a field")
    )]
    let mut manager = ServiceManager::new(allow_cross_uid_overwrite, Arc::clone(&enforcer));
    #[cfg(feature = "topic")]
    let registry = {
        let registry = rsbinder_topic::registry::Registry::new(TopicAccess(Arc::clone(&enforcer)))
            .allow_cross_uid_overwrite(allow_cross_uid_overwrite);
        let binder = registry.as_binder();
        manager.topics = Some(registry);
        binder
    };
    let service = BnServiceManager::new_binder(manager);
    // Non-fatal, as AOSP `main.cpp`: clients reach the hub through handle 0 regardless.
    if let Err(e) = service.addService(
        SELF_SERVICE_NAME,
        &service.as_binder(),
        false,
        DUMP_FLAG_PRIORITY_DEFAULT,
    ) {
        log::error!("rsb_hub: could not self-register as '{SELF_SERVICE_NAME}': {e:?}");
    }
    // Before `READY=1`, so a unit ordered after the hub finds the registry.
    #[cfg(feature = "topic")]
    {
        let name = rsbinder_topic::aidl::ITopicRegistry::SERVICE_NAME;
        match service.addService(name, &registry, false, DUMP_FLAG_PRIORITY_DEFAULT) {
            Ok(()) => log::info!("rsb_hub: serving the topic registry as '{name}'"),
            Err(e) => {
                log::error!("rsb_hub: could not register the topic registry as '{name}': {e:?}")
            }
        }
    }

    // The kernel enforces one context manager; the raw ioctl error does not name the cause.
    if let Err(e) = ProcessState::as_self().become_context_manager(service.as_binder()) {
        eprintln!("rsb_hub: cannot become the service manager for {binder_path}: {e}");
        eprintln!();
        eprintln!("A binder device has exactly one service manager, so this usually means");
        eprintln!("one is already running on {binder_path}. Check with:");
        eprintln!("    rsb_service --device {device_name} check manager");
        eprintln!();
        eprintln!("To run a second, independent service manager, give it its own device:");
        eprintln!("    sudo rsb_device other --group binder --mode 0660 && rsb_hub --device other");
        std::process::exit(1);
    }

    // Only once handle 0 is ours, else `After=` units race the registry.
    log::info!("rsb_hub: serving on {binder_path}");
    notifier.ready(&ready_status);

    Ok(ProcessState::join_thread_pool()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each `fake_binder()` is its own allocation, so its own `WIBinder` identity.
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
        fn link(&self) -> rsbinder::Result<()> {
            self.links.set(self.links.get() + 1);
            Ok(())
        }
        fn unlink(&self) {
            self.unlinks.set(self.unlinks.get() + 1);
        }
    }

    /// One binder under many names takes one subscription, dropped only with the last name.
    #[test]
    fn one_subscription_per_binder_regardless_of_name_count() {
        let mut inner = test_inner();
        let ledger = LinkLedger::default();
        let weak = SIBinder::downgrade(&fake_binder());

        for _ in 0..5 {
            inner
                .retain_counted(7, &weak, LINK_FAILED, || ledger.link())
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

    /// Any client can drive this cycle, so it must leave no subscription behind.
    #[test]
    fn register_unregister_cycles_do_not_accumulate_subscriptions() {
        let mut inner = test_inner();
        let ledger = LinkLedger::default();
        let weak = SIBinder::downgrade(&fake_binder());

        for _ in 0..100 {
            inner
                .retain_counted(9, &weak, LINK_FAILED, || ledger.link())
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

    /// Else a later release would unlink a subscription that was never taken.
    #[test]
    fn failed_link_records_nothing() {
        let mut inner = test_inner();
        let weak = SIBinder::downgrade(&fake_binder());

        // AOSP's answer: `DeadObject` would read as the hub's own death.
        let err = inner
            .retain_counted(3, &weak, CALLBACK_LINK_FAILED, || {
                Err(StatusCode::DeadObject)
            })
            .expect_err("link failure must propagate");
        assert_eq!(err.exception_code(), ExceptionCode::IllegalState);
        assert_eq!(err.transaction_error(), StatusCode::Ok);
        assert!(err.to_string().contains(CALLBACK_LINK_FAILED), "{err}");
        assert!(inner.death_links.is_empty());

        // And the release that follows a failed retain is inert.
        let ledger = LinkLedger::default();
        inner.release_counted(3, || ledger.unlink());
        assert_eq!(ledger.unlinks.get(), 0);
    }

    /// The sweep with real proxies is covered by `tests/scripts/run_hub_policy_ac.sh`.
    #[test]
    fn obituary_finds_its_record_by_the_stored_weak() {
        let mut inner = test_inner();
        let ledger = LinkLedger::default();
        let dead = fake_binder();
        let live = fake_binder();
        let dead_weak = SIBinder::downgrade(&dead);
        let live_weak = SIBinder::downgrade(&live);

        inner
            .retain_counted(1, &dead_weak, LINK_FAILED, || ledger.link())
            .unwrap();
        inner
            .retain_counted(1, &dead_weak, LINK_FAILED, || ledger.link())
            .unwrap();
        inner
            .retain_counted(2, &live_weak, LINK_FAILED, || ledger.link())
            .unwrap();

        let retired = inner.retire_dead_binder(&dead_weak);
        assert_eq!(retired, (0, 0), "no registrations exist in this test");

        assert!(!inner.death_links.contains_key(&1), "dead record removed");
        assert!(inner.death_links.contains_key(&2), "live record kept");
        assert_eq!(ledger.unlinks.get(), 0, "the obituary path must not unlink");
    }

    #[test]
    fn obituary_for_an_untracked_binder_is_inert() {
        let mut inner = test_inner();
        let stranger = SIBinder::downgrade(&fake_binder());
        assert_eq!(inner.retire_dead_binder(&stranger), (0, 0));
        assert!(inner.death_links.is_empty());
    }

    /// Callers rely on this instead of testing for a native binder themselves.
    #[test]
    fn native_binders_are_skipped() {
        let mut inner = test_inner();
        let native = fake_binder();
        assert!(native.as_proxy().is_none(), "precondition");

        inner
            .retain_death_link(&native, LINK_FAILED)
            .expect("retain on a native binder must be a no-op");
        assert!(inner.death_links.is_empty());
        inner.release_death_link(&native);
        assert!(inner.death_links.is_empty());
    }

    /// A stuck `false` lets clients cache a lazy binder that can unregister without dying.
    #[test]
    fn lazy_flag_reaches_the_service_union() {
        use hub::android_16::android::os::Service::Service;

        let lazy = classify_for_service_union(
            Some(Lookup {
                binder: fake_binder(),
                is_lazy: true,
            }),
            false,
        );
        match lazy {
            Service::ServiceWithMetadata(swm) => assert!(swm.isLazyService),
            other => panic!("expected ServiceWithMetadata, got {other:?}"),
        }

        let eager = classify_for_service_union(
            Some(Lookup {
                binder: fake_binder(),
                is_lazy: false,
            }),
            false,
        );
        match eager {
            Service::ServiceWithMetadata(swm) => assert!(!swm.isLazyService),
            other => panic!("expected ServiceWithMetadata, got {other:?}"),
        }

        // An accessor has no metadata arm to carry the flag.
        let accessor = classify_for_service_union(
            Some(Lookup {
                binder: fake_binder(),
                is_lazy: true,
            }),
            true,
        );
        assert!(matches!(accessor, Service::Accessor(Some(_))));
    }

    /// AOSP `tryGetService`: a declared accessor's arm, even with nothing registered under it.
    #[test]
    fn a_missing_accessor_is_a_null_accessor() {
        use hub::android_16::android::os::Service::Service;

        assert!(matches!(
            classify_for_service_union(None, true),
            Service::Accessor(None)
        ));
        assert!(matches!(
            classify_for_service_union(None, false),
            Service::ServiceWithMetadata(swm) if swm.service.is_none()
        ));
    }

    /// So a lazy registration still needs its own priority bit to appear in `listServices`.
    #[test]
    fn lazy_flag_is_disjoint_from_the_priority_mask() {
        assert_eq!(FLAG_IS_LAZY_SERVICE, 1 << 30);
        assert_eq!(FLAG_IS_LAZY_SERVICE & DUMP_FLAG_PRIORITY_ALL, 0);
    }

    /// Only the pre-`ProcessState::init` branch; AC-6.1.8 (`run_hub_policy_ac.sh`) runs the other.
    #[test]
    fn calling_caller_is_none_outside_a_transaction() {
        assert!(
            rsbinder::calling_caller().is_none(),
            "no transaction is in flight on a test thread"
        );
    }

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
        // The AOSP names that motivate the permitted charset.
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
            #[cfg(feature = "topic")]
            topics: None,
        }
    }

    fn kernel_caller(uid: u32) -> rsbinder::Caller {
        rsbinder::Caller::Kernel {
            uid,
            pid: 4242,
            sid: None,
        }
    }

    fn a_manager_over(text: &str) -> (ServiceManager, Arc<config::Config>) {
        let file = config::parse_file(Path::new("test.toml"), text, &config::SystemResolver)
            .expect("test config");
        let mut config = config::Config::default();
        config.policy.rules = file.rules;
        for (name, declaration) in file.services {
            config.declarations.insert(name, declaration);
        }
        let enforcer = Arc::new(Enforcer::enforcing(config));
        let snapshot = enforcer.config();
        (ServiceManager::new(false, enforcer), snapshot)
    }

    /// AOSP `canFindService`/`canAddService`: a grant on the instance alone misses its accessor.
    #[test]
    fn the_accessor_is_judged_with_the_instance() {
        let (manager, config) = a_manager_over(
            r#"
            [[rule]]
            name = "open.*"
            add = { uid = [1000] }
            find = { uid = [1000] }

            [[service]]
            name = "open.IFoo/default"
            accessor = "closed.foo"

            [[service]]
            name = "open.IBar/default"
            accessor = "open.bar"
            "#,
        );
        let caller = kernel_caller(1000);
        for permission in [Permission::Add, Permission::Find] {
            assert!(manager.allows_in(&config, Some(&caller), permission, "open.IFoo/default"));
            assert!(!manager.allows_service_in(
                &config,
                Some(&caller),
                permission,
                "open.IFoo/default"
            ));
            assert!(manager.allows_service_in(
                &config,
                Some(&caller),
                permission,
                "open.IBar/default"
            ));
            // An undeclared name is judged by itself.
            assert!(manager.allows_service_in(&config, Some(&caller), permission, "open.x"));
        }
    }

    /// AOSP `canFindService`: a refusal over the accessor does not name it to the caller.
    #[test]
    fn a_refused_accessor_is_not_named() {
        // Outside a transaction only `add` of the hub's own name passes, so its accessor fails.
        let text = "[[service]]\nname = \"open.IFoo/default\"\naccessor = \"closed.foo\"\n";
        let file = config::parse_file(Path::new("test.toml"), text, &config::SystemResolver)
            .expect("test config");
        let mut config = config::Config::default();
        for (_, declaration) in file.services {
            config
                .declarations
                .insert(SELF_SERVICE_NAME.to_string(), declaration);
        }
        let enforcer = Arc::new(Enforcer::enforcing(config));
        let config = enforcer.config();
        let manager = ServiceManager::new(false, enforcer);
        let err = manager
            .require_in(&config, Permission::Add, SELF_SERVICE_NAME)
            .expect_err("the accessor is refused");
        assert_eq!(err.exception_code(), ExceptionCode::Security);
        let msg = err.to_string();
        assert!(msg.contains("the accessor of"), "{msg}");
        assert!(!msg.contains("closed.foo"), "{msg}");
    }

    /// Only startup, which runs outside any transaction, may add the hub's own names.
    #[test]
    fn a_transaction_may_add_no_reserved_name_even_allowed_all() {
        let manager = ServiceManager::new(false, Arc::new(Enforcer::allow_all()));
        let caller = kernel_caller(0);
        for name in RESERVED_NAMES {
            assert!(
                !manager.allows_as(Some(&caller), Permission::Add, name),
                "{name}"
            );
            assert!(
                manager.allows_as(Some(&caller), Permission::Find, name),
                "{name}"
            );
            assert!(manager.allows_as(None, Permission::Add, name), "{name}");
        }
        assert!(manager.allows_as(None, Permission::Add, SELF_SERVICE_NAME));
        assert!(manager.allows_as(Some(&caller), Permission::Add, SELF_SERVICE_NAME));
        assert!(!manager.allows_as(None, Permission::Add, "other"));
        assert!(!manager.allows_as(None, Permission::Find, SELF_SERVICE_NAME));
    }

    /// Without the feature the registry's name is a service name like any other.
    #[cfg(not(feature = "topic"))]
    #[test]
    fn without_topic_the_registry_name_is_not_reserved() {
        let manager = ServiceManager::new(false, Arc::new(Enforcer::allow_all()));
        let name = "rsbinder.topic.registry";
        assert!(manager.allows_as(Some(&kernel_caller(0)), Permission::Add, name));
        assert!(!manager.allows_as(None, Permission::Add, name));
    }

    #[cfg(feature = "topic")]
    #[test]
    fn the_reserved_name_is_the_topic_registry() {
        assert_eq!(
            RESERVED_NAMES,
            [rsbinder_topic::aidl::ITopicRegistry::SERVICE_NAME]
        );
    }

    /// Each action reaches its own topic permission, and none a service permission.
    #[cfg(feature = "topic")]
    #[test]
    fn the_registry_asks_the_topic_rules() {
        use rsbinder_tools::config::{NamePattern, Subjects, TopicRule};
        use rsbinder_topic::registry::{Action, RegistryAccess};
        let topic_rule = |advertise: u32, subscribe: u32, list: u32| {
            let mut config = config::Config::default();
            config.topics.list = Subjects::set([list], []);
            config.topics.rules = vec![TopicRule {
                pattern: NamePattern::Any,
                advertise: Subjects::set([advertise], []),
                subscribe: Subjects::set([subscribe], []),
            }];
            config.policy.list = Subjects::Any;
            TopicAccess(Arc::new(Enforcer::enforcing(config)))
        };
        let access = topic_rule(1, 2, 3);
        for (uid, allowed) in [
            (1, [true, false, false]),
            (2, [false, true, false]),
            (3, [false, false, true]),
        ] {
            let caller = kernel_caller(uid);
            let got = [Action::Advertise, Action::Subscribe, Action::List]
                .map(|action| access.allows(action, "t", &caller));
            assert_eq!(got, allowed, "uid {uid}");
        }
    }

    #[cfg(feature = "topic")]
    #[test]
    fn a_dump_reports_the_topic_registry_and_its_counters() {
        let mut stats = rsbinder_topic::registry::RegistryStats::default();
        stats.topics = 2;
        stats.notices_failed = 7;
        stats.owed = 3;
        let mut snap = empty_snapshot();
        snap.topics = Some(TopicDump {
            registered: true,
            rules: Some(4),
            stats,
        });
        let out = dump_to_string(&snap);
        assert!(
            out.contains("topic registry: registered as rsbinder.topic.registry, 4 topic rule(s)"),
            "{out}"
        );
        assert!(out.contains(" topics=2 "), "{out}");
        assert!(out.contains(" notices_failed=7 "), "{out}");
        assert!(out.contains(" owed=3 "), "{out}");

        snap.topics = Some(TopicDump {
            registered: false,
            rules: None,
            stats,
        });
        let out = dump_to_string(&snap);
        assert!(out.contains("NOT registered"), "{out}");
        assert!(out.contains("access control DISABLED"), "{out}");
    }

    /// Polls until the writer thread of [`send_dump`] lets go of the caller's fd.
    fn wait_for_the_dump_writer(writing: &DumpWriter) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while lock_recover(writing).is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "the dump writer never finished"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The binder thread returns while the writer waits on a caller that does not read.
    #[test]
    fn a_dump_to_an_unread_fd_holds_only_its_writer() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;

        const LEN: usize = 4 << 20;
        let writing = DumpWriter::default();
        let (mut reader, fd) = UnixStream::pair().expect("socketpair");
        let (tx, rx) = mpsc::channel();
        let slot = Arc::clone(&writing);
        std::thread::spawn(move || {
            let caller = rsbinder::CallingContext::current();
            let _ = tx.send(send_dump(&slot, fd.into(), vec![b'x'; LEN], &caller));
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(Ok(())));

        let caller = rsbinder::CallingContext::current();
        let (_, other) = UnixStream::pair().expect("socketpair");
        let reply = send_dump(&writing, other.into(), b"y".to_vec(), &caller);
        assert_eq!(reply, Err(StatusCode::WouldBlock));

        let mut got = Vec::new();
        reader.read_to_end(&mut got).expect("read the dump");
        assert_eq!(got.len(), LEN);
        wait_for_the_dump_writer(&writing);

        let (mut reader, fd) = UnixStream::pair().expect("socketpair");
        assert_eq!(
            send_dump(&writing, fd.into(), b"y".to_vec(), &caller),
            Ok(())
        );
        let mut got = Vec::new();
        reader.read_to_end(&mut got).expect("read the dump");
        assert_eq!(got, b"y");
    }

    /// A socket whose send buffer is full, so the next blocking write to its fd waits for a read.
    fn a_full_socket() -> (std::os::unix::net::UnixStream, std::os::fd::OwnedFd) {
        use std::io::Write;
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        writer.set_nonblocking(true).expect("nonblocking");
        loop {
            match writer.write(&[0; 4096]) {
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("filling the socket: {e}"),
            }
        }
        writer.set_nonblocking(false).expect("blocking");
        (reader, writer.into())
    }

    /// A denial writes its reason to the fd too, and that write must not wait on the caller.
    #[test]
    fn a_dump_denial_returns_while_its_fd_is_full() {
        // No transaction, so no caller: `list` is denied.
        let (manager, _) = a_manager_over("");
        let (mut reader, fd) = a_full_socket();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(manager.dump_fd(fd, &[]));
        });
        let reply = rx.recv_timeout(Duration::from_secs(10));
        assert_eq!(reply, Ok(Err(StatusCode::PermissionDenied)));

        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut got).expect("read the denial");
        let text = String::from_utf8_lossy(&got);
        assert!(
            text.ends_with("\n") && text.contains("policy denies `list`"),
            "{text}"
        );
    }

    /// A broken caller fd answers OK: `DEAD_OBJECT` (`-EPIPE`) would mark rsb_hub dead in AOSP.
    #[test]
    fn a_dump_to_a_broken_fd_answers_ok() {
        let writing = DumpWriter::default();
        let caller = rsbinder::CallingContext::current();
        let (reader, fd) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        drop(reader);
        assert_eq!(
            send_dump(&writing, fd.into(), b"y".to_vec(), &caller),
            Ok(())
        );
        wait_for_the_dump_writer(&writing);
    }

    /// Flags decoded, not just hex: `lazy=yes` decides whether the binder may be cached.
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

    /// No priority bit hides it from `listServices`, which looks like a lost registration.
    #[test]
    fn a_registration_with_no_priority_bit_is_called_out() {
        let mut snap = empty_snapshot();
        snap.services = vec![dump_row("com.example.IFoo/default", 0)];
        let out = dump_to_string(&snap);
        assert!(out.contains("no priority bit"), "{out}");

        snap.services = vec![dump_row("com.example.IFoo/default", DUMP_FLAG_PRIORITY_ALL)];
        assert!(!dump_to_string(&snap).contains("no priority bit"));
    }

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

    /// Counted, never named: a denied caller must not learn which names exist.
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

    /// An unblocked handled signal would kill the process by its default disposition.
    #[test]
    fn the_blocked_signal_set_covers_reload_and_shutdown() {
        let set = handled_signal_set().expect("building a sigset cannot fail here");
        for signo in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
            assert!(
                HANDLED_SIGNALS.contains(&signo),
                "signal {signo} must be handled"
            );
            // SAFETY: `set` is a live, fully initialized sigset and `sigismember` only reads it.
            assert_eq!(unsafe { libc::sigismember(&set, signo) }, 1);
        }
        // Nothing else: a blocked signal nobody consumes is silently ineffective.
        // SAFETY: as above.
        assert_eq!(unsafe { libc::sigismember(&set, libc::SIGUSR1) }, 0);
    }

    /// Overwriting an existing name never grows the map, so it passes a full cap.
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
