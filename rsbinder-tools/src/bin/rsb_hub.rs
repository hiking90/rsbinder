// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
#![allow(non_snake_case)]

use env_logger::Env;
use hub::android_16::{BnServiceManager, IServiceManager, DUMP_FLAG_PRIORITY_DEFAULT};
use rsbinder::*;
use std::{
    collections::HashMap,
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

struct DeathRecipientWrapper(mpsc::Sender<rsbinder::WIBinder>);

impl rsbinder::DeathRecipient for DeathRecipientWrapper {
    fn binder_died(&self, who: &rsbinder::WIBinder) {
        self.0.send(who.clone()).unwrap_or_else(|e| {
            log::error!("Failed to send death notification: {e:?}");
        });
    }
}

struct Inner {
    death_recipient: Arc<DeathRecipientWrapper>,
    name_to_service: HashMap<String, Service>,
    name_to_registration_callbacks: HashMap<
        String,
        Vec<rsbinder::Strong<dyn hub::android_16::android::os::IServiceCallback::IServiceCallback>>,
    >,
    name_to_client_callbacks: HashMap<
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
            name_to_service: HashMap::new(),
            name_to_registration_callbacks: HashMap::new(),
            name_to_client_callbacks: HashMap::new(),
        }
    }

    fn add_service(&mut self, name: &str, service: Service) -> rsbinder::status::Result<()> {
        self.name_to_service.insert(name.to_owned(), service);
        Ok(())
    }

    fn send_client_callback_notification(
        &mut self,
        service_name: &str,
        has_clients: bool,
        context: &str,
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

        self.name_to_client_callbacks.get(service_name).map(|callbacks| {
            for callback in callbacks {
                callback.onClients(&service.binder, has_clients)
                    .unwrap_or_else(|e| {
                        log::error!("Failed to notify client callback: {e:?}");
                    });
            }
        }).unwrap_or_else(|| {
            log::warn!("send_client_callback_notification could not find callbacks for service when {context}");
        });

        service.has_clients = has_clients;
    }

    fn handle_service_client_callback(
        &mut self,
        known_clients: usize,
        service_name: &str,
        is_called_on_interval: bool,
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
            );
            if let Some(service) = self.name_to_service.get(service_name) {
                has_clients = service.has_clients;
            }
        }

        Ok(has_clients)
    }

    /// Look up a registered service by name. Returns `Some((binder,
    /// is_accessor))` if registered — the `is_accessor` flag is the
    /// one stamped at `addService` time, used by
    /// `getService2`/`checkService2` to choose between
    /// `Service::Accessor(Some(_))` and `Service::ServiceWithMetadata`.
    /// Callers that only need the binder can `.map(|(b, _)| b)`.
    fn try_get_binder(
        &mut self,
        name: &str,
        _start_if_not_found: bool,
    ) -> rsbinder::status::Result<Option<(SIBinder, bool)>> {
        let service = if let Some(service) = self.name_to_service.get_mut(name) {
            service
        } else {
            return Ok(None);
        };

        let out = service.binder.clone();
        let is_accessor = service.is_accessor;
        service.guarantee_client = true;
        self.handle_service_client_callback(Self::KNOWN_CLIENTS_ON_DEMAND, name, false)?;

        if let Some(service) = self.name_to_service.get_mut(name) {
            service.guarantee_client = true;
        }

        Ok(Some((out, is_accessor)))
    }

    fn remove_registration_callback(
        &mut self,
        name: Option<&str>,
        who: &rsbinder::WIBinder,
    ) -> bool {
        let mut found = false;
        if let Some(name) = name {
            if let Some(callbacks) = self.name_to_registration_callbacks.get_mut(name) {
                callbacks.retain(|callback| {
                    let is_not_equal = SIBinder::downgrade(&callback.as_binder()) != *who;
                    found |= !is_not_equal;
                    is_not_equal
                });
                if callbacks.is_empty() {
                    self.name_to_registration_callbacks.remove(name);
                }
            }
        } else {
            self.name_to_registration_callbacks.retain(|_, callbacks| {
                callbacks.retain(|callback| {
                    let is_not_equal = SIBinder::downgrade(&callback.as_binder()) != *who;
                    found |= !is_not_equal;
                    is_not_equal
                });
                !callbacks.is_empty()
            });
        }

        found
    }

    fn remove_client_callback(&mut self, who: &rsbinder::WIBinder) {
        // Mirror AOSP `ServiceManager::binderDied`'s third loop
        // (`removeClientCallback` over `mNameToClientCallback`): drop every
        // client callback whose binder matches the dead `who`, and remove
        // any now-empty entries. Without this the dead `IClientCallback`
        // `Strong` is leaked for the lifetime of rsb_hub and
        // `onClients` keeps firing on a dead proxy on every state change.
        self.name_to_client_callbacks.retain(|_, callbacks| {
            callbacks.retain(|callback| SIBinder::downgrade(&callback.as_binder()) != *who);
            !callbacks.is_empty()
        });
    }
}

struct ServiceManager {
    inner: Arc<Mutex<Inner>>,
}

impl ServiceManager {
    fn new() -> Self {
        let (death_sender, death_receiver) = mpsc::channel();

        let this = Self {
            inner: Arc::new(Mutex::new(Inner::new(death_sender))),
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
                    let mut inner = inner_clone.lock().unwrap();

                    inner
                        .name_to_service
                        .retain(|_, service| !(SIBinder::downgrade(&service.binder) == who));

                    inner.remove_registration_callback(None, &who);
                    inner.remove_client_callback(&who);
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

                let mut inner = match inner_arc.lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        log::error!(
                            "client callback poller: Inner mutex poisoned by another thread, exiting"
                        );
                        return;
                    }
                };

                // Snapshot the names before iterating so we don't hold a
                // borrow into `name_to_service` across calls that may
                // mutate it (e.g., `send_client_callback_notification`
                // writing back `service.has_clients`). The map is small
                // (one entry per registered service) — a transient `Vec`
                // is cheaper than the alternative refactor.
                let names: Vec<String> = inner.name_to_service.keys().cloned().collect();
                for name in &names {
                    if let Err(e) =
                        inner.handle_service_client_callback(Inner::KNOWN_CLIENTS_PERIODIC, name, true)
                    {
                        log::error!("client callback poll failed for {name}: {e:?}");
                    }
                }
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
}

impl Interface for ServiceManager {}

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
    lookup: Option<(SIBinder, bool)>,
) -> hub::android_16::android::os::Service::Service {
    use hub::android_16::android::os::{Service, ServiceWithMetadata};
    match lookup {
        Some((binder, true)) => Service::Service::Accessor(Some(binder)),
        Some((binder, false)) => {
            Service::Service::ServiceWithMetadata(ServiceWithMetadata::ServiceWithMetadata {
                service: Some(binder),
                isLazyService: false,
            })
        }
        None => Service::Service::Accessor(None),
    }
}

impl IServiceManager for ServiceManager {
    /// Linux note: this is semantically equivalent to
    /// [`checkService`](Self::checkService). AOSP's servicemanager
    /// distinguishes the two — `getService` calls `tryGetBinder(name,
    /// /*startIfNotFound=*/true)` and triggers a lazy-service start via
    /// `ctl.interface_start_<name>` init property — but lazy service
    /// activation depends on Android's init system and has no
    /// equivalent on a plain Linux host. The `start_if_not_found`
    /// argument is therefore hardcoded to `false`. Accessor routing for
    /// callers that want it lives in
    /// [`getService2`](Self::getService2)/[`checkService2`](Self::checkService2).
    fn getService(&self, name: &str) -> rsbinder::status::Result<Option<rsbinder::SIBinder>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .try_get_binder(name, false)?
            .map(|(b, _)| b))
    }

    fn addService(
        &self,
        name: &str,
        service: &SIBinder,
        allowIsolated: bool,
        dumpPriority: i32,
    ) -> rsbinder::status::Result<()> {
        if !Self::is_valid_service_name(name) {
            return Err(ExceptionCode::IllegalArgument.into());
        }

        let mut inner = self.inner.lock().unwrap();

        // Only if the service is a proxy, link to death.
        // Because the native service does not support death notification.
        if service.as_proxy().is_some() {
            service.link_to_death(Arc::downgrade(
                &(inner.death_recipient.clone() as Arc<dyn rsbinder::DeathRecipient>),
            ))?;
        }

        let mut prev_clients = false;
        {
            if let Some(service) = inner.name_to_service.get(name) {
                prev_clients = service.has_clients;
            }
        }

        // Detect `IAccessor` binders by interface descriptor at registration.
        // Hardcoding the AOSP-stable `android.os.IAccessor` string (instead of
        // pulling the `IAccessor` symbol) keeps rsb_hub buildable without the
        // `rpc` feature.
        let is_accessor = service.descriptor() == "android.os.IAccessor";

        // `allowIsolated` is accepted by AIDL for AOSP source
        // compatibility but unused on Linux — there is no isolated-app
        // sandbox UID range to gate on.
        let _ = allowIsolated;

        inner.add_service(
            name,
            Service {
                binder: service.clone(),
                dump_priority: dumpPriority,
                has_clients: prev_clients,
                guarantee_client: false,
                context: rsbinder::thread_state::CallingContext::default(),
                is_accessor,
            },
        )?;

        if inner.name_to_registration_callbacks.contains_key(name) {
            if let Some(service) = inner.name_to_service.get_mut(name) {
                service.guarantee_client = true;
            }

            inner.handle_service_client_callback(Inner::KNOWN_CLIENTS_ON_DEMAND, name, false)?;

            if let Some(service) = inner.name_to_service.get_mut(name) {
                service.guarantee_client = true;
            }

            let callbacks = inner
                .name_to_registration_callbacks
                .get(name)
                .expect("name_to_registration_callbacks must have key");
            for callback in callbacks {
                callback.onRegistration(name, service)?;
            }
        }

        Ok(())
    }

    /// Linux note: identical to [`getService`](Self::getService) —
    /// `start_if_not_found` is always `false` on this implementation
    /// (no lazy-service infrastructure). See `getService`'s rustdoc for
    /// the rationale.
    fn checkService(&self, name: &str) -> rsbinder::status::Result<Option<SIBinder>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .try_get_binder(name, false)?
            .map(|(b, _)| b))
    }

    fn listServices(&self, dump_priority: i32) -> rsbinder::status::Result<Vec<String>> {
        let inner = self.inner.lock().unwrap();

        let mut services = Vec::new();

        for (name, service) in inner.name_to_service.iter() {
            if (service.dump_priority & dump_priority) != 0 {
                services.push(name.clone());
            }
        }

        Ok(services)
    }

    fn registerForNotifications(
        &self,
        name: &str,
        arg_callback: &rsbinder::Strong<
            dyn hub::android_16::android::os::IServiceCallback::IServiceCallback,
        >,
    ) -> rsbinder::status::Result<()> {
        if !Self::is_valid_service_name(name) {
            return Err(ExceptionCode::IllegalArgument.into());
        }

        let mut inner = self.inner.lock().unwrap();

        arg_callback.as_binder().link_to_death(Arc::downgrade(
            &(inner.death_recipient.clone() as Arc<dyn rsbinder::DeathRecipient>),
        ))?;

        inner
            .name_to_registration_callbacks
            .entry(name.to_string())
            .or_default()
            .push(arg_callback.clone());

        if let Some(service) = inner.name_to_service.get(name) {
            arg_callback
                .onRegistration(name, &service.binder)
                .unwrap_or_else(|e| {
                    log::error!("Failed to notify client callback: {e:?}");
                });
        }

        Ok(())
    }

    fn unregisterForNotifications(
        &self,
        name: &str,
        callback: &rsbinder::Strong<
            dyn hub::android_16::android::os::IServiceCallback::IServiceCallback,
        >,
    ) -> rsbinder::status::Result<()> {
        let mut inner = self.inner.lock().unwrap();

        if inner
            .remove_registration_callback(Some(name), &SIBinder::downgrade(&callback.as_binder()))
        {
            Ok(())
        } else {
            Err(ExceptionCode::IllegalState.into())
        }
    }

    /// rsb_hub has no static service declaration system on Linux —
    /// AOSP's servicemanager answers this from VINTF manifests
    /// (`/system/etc/vintf/...`, `/system_ext/etc/vintf/...`), which
    /// are an Android-specific build artifact with no equivalent on a
    /// plain Linux host. Returning `false` is the truthful answer:
    /// "no, this name has no pre-declared availability — fall back to
    /// dynamic lookup via `getService`/`checkService`". Vendors that
    /// need VINTF-equivalent declaration semantics should layer that
    /// on top in their own service-manager (or run rsb_hub on a host
    /// that ships VINTF files plus a parser, which is out of scope
    /// here).
    fn isDeclared(&self, _arg_name: &str) -> rsbinder::status::Result<bool> {
        Ok(false)
    }

    /// See [`isDeclared`](Self::isDeclared) — same VINTF-on-Linux
    /// rationale. An empty `Vec` reports "no declared instances for
    /// this interface" which on a VINTF-free system is always true.
    fn getDeclaredInstances(&self, _arg_iface: &str) -> rsbinder::status::Result<Vec<String>> {
        Ok(vec![])
    }

    /// APEX (Android Pony EXpress) is an Android-only packaging
    /// system; there is no equivalent on a plain Linux host. Returning
    /// `None` truthfully reports "no APEX governs this service". Demoted
    /// to `debug` because under steady-state load every `getService`
    /// caller that asks may hit this — `warn` would flood the log.
    fn updatableViaApex(&self, _arg_name: &str) -> rsbinder::status::Result<Option<String>> {
        log::debug!("updatableViaApex is not implemented on Linux (APEX is Android-only)");
        Ok(None)
    }

    /// AOSP's servicemanager surfaces the VINTF `<ip>`+`<port>` of an
    /// AIDL service for inet-style RPC accessor connection info (see
    /// `getVintfConnectionInfo` in `frameworks/native/cmds/servicemanager/
    /// ServiceManager.cpp`). rsb_hub has no VINTF infrastructure on
    /// Linux, so the only honest reply is `None` — callers should
    /// either go through an `IAccessor` they obtained out-of-band
    /// (e.g., via the consume-side accessor arm of `getService2` +
    /// process-local `add_accessor_provider`) or fall back to a
    /// vendor-supplied lookup. Same design choice as [`isDeclared`](Self::isDeclared).
    fn getConnectionInfo(
        &self,
        _arg_name: &str,
    ) -> rsbinder::status::Result<
        Option<hub::android_16::android::os::ConnectionInfo::ConnectionInfo>,
    > {
        Ok(None)
    }

    fn registerClientCallback(
        &self,
        name: &str,
        arg_service: &rsbinder::SIBinder,
        arg_callback: &rsbinder::Strong<
            dyn hub::android_16::android::os::IClientCallback::IClientCallback,
        >,
    ) -> rsbinder::status::Result<()> {
        let mut inner = self.inner.lock().unwrap();

        let service = if let Some(service) = inner.name_to_service.get(name) {
            service
        } else {
            let msg = format!("registerClientCallback could not find service {name}");
            log::warn!("{}", &msg);
            return Err((ExceptionCode::IllegalArgument, msg.as_str()).into());
        };

        if service.context.pid != rsbinder::thread_state::CallingContext::default().pid {
            let msg = format!(
                "{:?} Only a server can register for client callbacks (for {})",
                service.context, name
            );
            log::warn!("{}", &msg);
            return Err((ExceptionCode::Security, msg.as_str()).into());
        }

        if service.binder != *arg_service {
            let msg = format!("registerClientCallback called with wrong service {name}");
            log::warn!("{}", &msg);
            return Err((ExceptionCode::IllegalArgument, msg.as_str()).into());
        }

        arg_callback.as_binder().link_to_death(Arc::downgrade(
            &(inner.death_recipient.clone() as Arc<dyn rsbinder::DeathRecipient>),
        ))?;

        if service.has_clients {
            arg_callback
                .onClients(&service.binder, true)
                .unwrap_or_else(|e| {
                    log::error!("Failed to notify client callback: {e:?}");
                });
        }

        inner
            .name_to_client_callbacks
            .entry(name.to_string())
            .or_default()
            .push(arg_callback.clone());

        inner.handle_service_client_callback(Inner::KNOWN_CLIENTS_ON_DEMAND, name, false)?;

        Ok(())
    }

    fn tryUnregisterService(
        &self,
        name: &str,
        arg_service: &rsbinder::SIBinder,
    ) -> rsbinder::status::Result<()> {
        let context = rsbinder::thread_state::CallingContext::default();

        let mut inner = self.inner.lock().unwrap();
        let service = if let Some(service) = inner.name_to_service.get(name) {
            service
        } else {
            let msg = format!(
                "{context:?} Tried to unregister {name}, but that service wasn't registered to begin with."
            );
            log::warn!("{}", &msg);
            return Err((ExceptionCode::IllegalArgument, msg.as_str()).into());
        };

        if service.context.pid != rsbinder::thread_state::CallingContext::default().pid {
            let msg = format!(
                "{:?} Only a server can register for client callbacks (for {})",
                service.context, name
            );
            log::warn!("{}", &msg);
            return Err((ExceptionCode::Security, msg.as_str()).into());
        }

        if service.binder != *arg_service {
            let msg = format!("{context:?} Tried to unregister {name}, but a different service is registered under this name.");
            log::warn!("{}", &msg);
            return Err((ExceptionCode::IllegalArgument, msg.as_str()).into());
        }

        if service.guarantee_client {
            let msg = format!(
                "{context:?} Tried to unregister {name}, but there is about to be a client."
            );
            log::warn!("{}", &msg);
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
            .handle_service_client_callback(Inner::KNOWN_CLIENTS_ON_DEMAND, name, false)
            .unwrap_or(true);
        if has_clients {
            let msg = format!("{context:?} Tried to unregister {name}, but there are clients.");
            log::warn!("{}", &msg);
            if let Some(service) = inner.name_to_service.get_mut(name) {
                service.guarantee_client = true;
            }
            return Err((ExceptionCode::IllegalState, msg.as_str()).into());
        }

        inner.name_to_service.remove(name);

        Ok(())
    }

    fn getServiceDebugInfo(
        &self,
    ) -> rsbinder::status::Result<
        Vec<hub::android_16::android::os::ServiceDebugInfo::ServiceDebugInfo>,
    > {
        let inner = self.inner.lock().unwrap();

        let mut out = Vec::with_capacity(inner.name_to_service.len());

        for (name, service) in inner.name_to_service.iter() {
            out.push(
                hub::android_16::android::os::ServiceDebugInfo::ServiceDebugInfo {
                    name: name.clone(),
                    debugPid: service.context.pid,
                },
            );
        }

        Ok(out)
    }

    fn getService2(
        &self,
        name: &str,
    ) -> rsbinder::status::Result<hub::android_16::android::os::Service::Service> {
        // Routing logic lives in `classify_for_service_union` so
        // `checkService2` stays byte-identical without re-stating the
        // match arms.
        let lookup = self.inner.lock().unwrap().try_get_binder(name, false)?;
        Ok(classify_for_service_union(lookup))
    }

    fn checkService2(
        &self,
        name: &str,
    ) -> rsbinder::status::Result<hub::android_16::android::os::Service::Service> {
        // See `getService2` — both route through
        // `classify_for_service_union`.
        let lookup = self.inner.lock().unwrap().try_get_binder(name, false)?;
        Ok(classify_for_service_union(lookup))
    }

    /// See [`updatableViaApex`](Self::updatableViaApex) — same
    /// APEX-on-Linux rationale, same demoted log level. An empty `Vec`
    /// is the truthful "no APEX-updatable services" answer.
    fn getUpdatableNames(&self, _apex_name: &str) -> rsbinder::status::Result<Vec<String>> {
        log::debug!("getUpdatableNames is not implemented on Linux (APEX is Android-only)");
        Ok(vec![])
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
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
        .after_help(
            "Examples:\n    \
            Run with the default binder device:\n    \
            $ rsb_hub\n\n    \
            Run with a custom binder device:\n    \
            $ rsb_hub --device mybinder\n    \
            $ rsb_hub -d mybinder\n\n    \
            Note: The binder device must be created first using rsb_device.",
        )
        .get_matches();

    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    let device_name = matches
        .get_one::<String>("device")
        .expect("device has a default value");
    let binder_path = format!("{}/{}", DEFAULT_BINDERFS_PATH, device_name);

    log::info!("Starting rsb_hub with binder device: {}", binder_path);

    ProcessState::init(&binder_path, 0)?;

    // Create a binder service.
    let service = BnServiceManager::new_binder(ServiceManager::new());
    service.addService(
        "manager",
        &service.as_binder(),
        false,
        DUMP_FLAG_PRIORITY_DEFAULT,
    )?;

    ProcessState::as_self().become_context_manager(service.as_binder())?;

    Ok(ProcessState::join_thread_pool()?)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
