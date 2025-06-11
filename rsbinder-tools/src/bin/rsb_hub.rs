// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
#![allow(non_snake_case)]

use env_logger::Env;
use hub::android_13::{BnServiceManager, IServiceManager, DUMP_FLAG_PRIORITY_DEFAULT};
use rsbinder::*;
use std::{
    collections::HashMap,
    sync::{mpsc, Arc, Mutex},
};

struct Service {
    binder: SIBinder,
    _allow_isolated: bool,
    dump_priority: i32,
    has_clients: bool,
    guarentee_client: bool,
    _debug_pid: u32,
    context: rsbinder::thread_state::CallingContext,
}

impl Service {
    fn _get_node_strong_ref_count(&self) -> usize {
        unimplemented!("get_node_strong_ref_count")
    }

    fn _try_start_service(&self) -> rsbinder::Result<SIBinder> {
        unimplemented!("try_start_service")
    }
}

struct DeathRecipientWrapper(mpsc::Sender<rsbinder::WIBinder>);

impl rsbinder::DeathRecipient for DeathRecipientWrapper {
    fn binder_died(&self, who: &rsbinder::WIBinder) {
        self.0.send(who.clone()).unwrap_or_else(|e| {
            log::error!("Failed to send death notification: {:?}", e);
        });
    }
}

struct Inner {
    death_recipient: Arc<DeathRecipientWrapper>,
    name_to_service: HashMap<String, Service>,
    name_to_registration_callbacks: HashMap<
        String,
        Vec<rsbinder::Strong<dyn hub::android_13::android::os::IServiceCallback::IServiceCallback>>,
    >,
    name_to_client_callbacks: HashMap<
        String,
        Vec<rsbinder::Strong<dyn hub::android_13::android::os::IClientCallback::IClientCallback>>,
    >,
}

impl Inner {
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
                "send_client_callback_notification could not find service {} when {}",
                service_name,
                context
            );
            return;
        };

        if service.has_clients == has_clients {
            log::error!(
                "send_client_callback_notification called with the same state {} when {}",
                has_clients,
                context
            );
            std::process::abort()
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
                        log::error!("Failed to notify client callback: {:?}", e);
                    });
            }
        }).unwrap_or_else(|| {
            log::warn!("send_client_callback_notification could not find callbacks for service when {}", context);
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
            // Clippy recommands to use 'is_none_or' here, but 'is_none_or' is not supported by 1.77.
            #[allow(clippy::unnecessary_map_or)]
            if self
                .name_to_client_callbacks
                .get(service_name)
                .map_or(true, |callbacks| callbacks.is_empty())
            {
                return Ok(true);
            }
            service
        } else {
            return Ok(true);
        };

        let count = match rsbinder::ProcessState::as_self()
            .strong_ref_count_for_node(service.binder.as_proxy().expect("Service must be a proxy"))
        {
            Ok(count) => count,
            Err(e) => {
                log::error!(
                    "Failed to get strong ref count for {}: {:?}",
                    service_name,
                    e
                );
                return Ok(true);
            }
        };
        let has_kernel_reported_clients = count > known_clients;

        // To avoid the borrow checker, we need to get the value of has_clients
        let mut has_clients = service.has_clients;

        if service.guarentee_client {
            if !has_clients && !has_kernel_reported_clients {
                self.send_client_callback_notification(
                    service_name,
                    true,
                    "service is guaranteed to be in use",
                );
            }

            if let Some(service) = self.name_to_service.get_mut(service_name) {
                // guarantee is temporary
                service.has_clients = true;
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

    fn try_get_binder(
        &mut self,
        name: &str,
        _start_if_not_found: bool,
    ) -> rsbinder::status::Result<Option<SIBinder>> {
        let service = if let Some(service) = self.name_to_service.get_mut(name) {
            service
        } else {
            return Ok(None);
        };

        let out = service.binder.clone();
        service.guarentee_client = true;
        self.handle_service_client_callback(2 /* sm + transaction */, name, false)?;

        if let Some(service) = self.name_to_service.get_mut(name) {
            service.guarentee_client = true;
        }

        Ok(Some(out))
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

        this
    }

    fn run_death_receiver(&self, death_receiver: mpsc::Receiver<rsbinder::WIBinder>) {
        let inner_clone = Arc::clone(&self.inner);
        std::thread::spawn(move || {
            for who in death_receiver {
                let mut inner = inner_clone.lock().unwrap();

                inner
                    .name_to_service
                    .retain(|_, service| !(SIBinder::downgrade(&service.binder) == who));

                inner.remove_registration_callback(None, &who);
            }
        });
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

impl IServiceManager for ServiceManager {
    fn getService(&self, name: &str) -> rsbinder::status::Result<Option<rsbinder::SIBinder>> {
        self.inner.lock().unwrap().try_get_binder(name, false)
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

        inner.add_service(
            name,
            Service {
                binder: service.clone(),
                _allow_isolated: allowIsolated,
                dump_priority: dumpPriority,
                has_clients: prev_clients,
                guarentee_client: false,
                _debug_pid: 0,
                context: rsbinder::thread_state::CallingContext::default(),
            },
        )?;

        if inner.name_to_registration_callbacks.contains_key(name) {
            if let Some(service) = inner.name_to_service.get_mut(name) {
                service.guarentee_client = true;
            }

            inner.handle_service_client_callback(2, name, false)?;

            if let Some(service) = inner.name_to_service.get_mut(name) {
                service.guarentee_client = true;
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

    fn checkService(&self, name: &str) -> rsbinder::status::Result<Option<SIBinder>> {
        self.inner.lock().unwrap().try_get_binder(name, false)
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
            dyn hub::android_13::android::os::IServiceCallback::IServiceCallback,
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
                    log::error!("Failed to notify client callback: {:?}", e);
                });
        }

        Ok(())
    }

    fn unregisterForNotifications(
        &self,
        name: &str,
        callback: &rsbinder::Strong<
            dyn hub::android_13::android::os::IServiceCallback::IServiceCallback,
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

    fn isDeclared(&self, _arg_name: &str) -> rsbinder::status::Result<bool> {
        // TODO: Implement this
        log::warn!("isDeclared is not implemented");
        Ok(false)
    }

    fn getDeclaredInstances(&self, _arg_iface: &str) -> rsbinder::status::Result<Vec<String>> {
        log::warn!("getDeclaredInstances is not implemented");
        Ok(vec![])
    }

    fn updatableViaApex(&self, _arg_name: &str) -> rsbinder::status::Result<Option<String>> {
        log::warn!("updatableViaApex is not implemented");
        Ok(None)
    }

    fn getConnectionInfo(
        &self,
        _arg_name: &str,
    ) -> rsbinder::status::Result<
        Option<hub::android_13::android::os::ConnectionInfo::ConnectionInfo>,
    > {
        log::warn!("getConnectionInfo is not implemented");
        Ok(None)
    }

    fn registerClientCallback(
        &self,
        name: &str,
        arg_service: &rsbinder::SIBinder,
        arg_callback: &rsbinder::Strong<
            dyn hub::android_13::android::os::IClientCallback::IClientCallback,
        >,
    ) -> rsbinder::status::Result<()> {
        let mut inner = self.inner.lock().unwrap();

        let service = if let Some(service) = inner.name_to_service.get(name) {
            service
        } else {
            let msg = format!("registerClientCallback could not find service {}", name);
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
            let msg = format!("registerClientCallback called with wrong service {}", name);
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
                    log::error!("Failed to notify client callback: {:?}", e);
                });
        }

        inner
            .name_to_client_callbacks
            .entry(name.to_string())
            .or_default()
            .push(arg_callback.clone());

        inner.handle_service_client_callback(2 /* sm + transaction */, name, false)?;

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
                "{:?} Tried to unregister {}, but that service wasn't registered to begin with.",
                context, name
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

        if arg_service.clone() != service.binder.clone() {
            let msg = format!("{:?} Tried to unregister {}, but a different service is registered under this name.",
                context, name);
            log::warn!("{}", &msg);
            return Err((ExceptionCode::IllegalArgument, msg.as_str()).into());
        }

        if service.guarentee_client {
            let msg = format!(
                "{:?} Tried to unregister {}, but there is about to be a client.",
                context, name
            );
            log::warn!("{}", &msg);
            return Err((ExceptionCode::IllegalState, msg.as_str()).into());
        }

        let res = inner.handle_service_client_callback(2, name, false);
        if res.is_err() {
            let msg = format!(
                "{:?} Tried to unregister {}, but there are clients.",
                context, name
            );
            log::warn!("{}", &msg);
            if let Some(service) = inner.name_to_service.get_mut(name) {
                service.guarentee_client = true;
            }
            return Err((ExceptionCode::IllegalState, msg.as_str()).into());
        }

        inner.name_to_service.remove(name);

        Ok(())
    }

    fn getServiceDebugInfo(
        &self,
    ) -> rsbinder::status::Result<
        Vec<hub::android_13::android::os::ServiceDebugInfo::ServiceDebugInfo>,
    > {
        let inner = self.inner.lock().unwrap();

        let mut out = Vec::with_capacity(inner.name_to_service.len());

        for (name, service) in inner.name_to_service.iter() {
            out.push(
                hub::android_13::android::os::ServiceDebugInfo::ServiceDebugInfo {
                    name: name.clone(),
                    debugPid: service.context.pid,
                },
            );
        }

        Ok(out)
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let _ = clap::Command::new("rsb_hub")
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about("A service manager for Binder IPC on Linux. Facilitates service registration, discovery, and management.")
        .get_matches();

    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    ProcessState::init(DEFAULT_BINDER_PATH, 0);

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
    fn test_is_valid_service_name() {
        assert!(ServiceManager::is_valid_service_name("test"));
        assert!(ServiceManager::is_valid_service_name("test-"));
        assert!(ServiceManager::is_valid_service_name("test_"));
        assert!(ServiceManager::is_valid_service_name("test."));
        assert!(ServiceManager::is_valid_service_name("test/"));
        assert!(ServiceManager::is_valid_service_name("test0"));
        assert!(ServiceManager::is_valid_service_name("test1"));
        assert!(ServiceManager::is_valid_service_name("TEST2"));
    }
}
