// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
#![allow(non_snake_case)]

use std::{collections::HashMap, sync::{RwLock, Arc}};
use rsbinder_hub::{IServiceManager, BnServiceManager, DUMP_FLAG_PRIORITY_DEFAULT};
use env_logger::Env;
use rsbinder::*;

struct Service {
    binder: SIBinder,
    allow_isolated: bool,
    dump_priority: i32,
    has_clients: bool,
    guarentee_client: bool,
    debug_pid: u32,
}

impl Service {
    fn get_node_strong_ref_count(&self) -> usize {
        unimplemented!("get_node_strong_ref_count")
    }

    fn try_start_service(&self) -> rsbinder::Result<SIBinder> {
        unimplemented!("try_start_service")
    }
}

struct ServiceManagerInner {
    name_to_service: RwLock<HashMap<String, Service>>,
    name_to_registration_callbacks: RwLock<HashMap<String, Vec<rsbinder::Strong<dyn rsbinder_hub::android::os::IServiceCallback::IServiceCallback>>>>,
}

impl ServiceManagerInner {
    fn try_get_service(&self, name: &str, _start_if_not_found: bool) -> rsbinder::status::Result<Option<SIBinder>> {
        self.name_to_service.write().unwrap().get_mut(name).map(|service| {
            service.guarentee_client = true;
            Ok(Some(service.binder.clone()))
        }).unwrap_or_else(|| {
            Ok(None)
        })
    }

    fn add_service(&self, name: &str, service: Service) -> rsbinder::status::Result<()> {
        self.name_to_service.write().unwrap().insert(name.to_owned(), service);
        Ok(())
    }

    fn on_registration(&self, name: &str) -> rsbinder::status::Result<()> {
        if let Some(service) = self.name_to_service.read().unwrap().get(name) {
            let callbacks = self.name_to_registration_callbacks.read().unwrap().get(name).cloned();
            if let Some(callbacks) = callbacks {
                for callback in callbacks {
                    callback.onRegistration(name, &service.binder)?;
                }
            }    
        }
        Ok(())
    }

    fn list_services(&self, dump_priority: i32) -> rsbinder::status::Result<Vec<String>> {
        let mut services = Vec::new();

        for (name, service) in self.name_to_service.read().unwrap().iter() {
            if (service.dump_priority & dump_priority) != 0 {
                services.push(name.clone());
            }
        }

        Ok(services)
    }

    fn register_for_notifications(&self, name: &str, callback: &rsbinder::Strong<dyn rsbinder_hub::android::os::IServiceCallback::IServiceCallback>) -> rsbinder::status::Result<()> {
        let mut callbacks = self.name_to_registration_callbacks.write().unwrap();
        let callbacks = callbacks.entry(name.to_owned()).or_default();
        callbacks.push(callback.clone());

        if let Some(service) = self.name_to_service.read().unwrap().get(name) {
            callback.onRegistration(name, &service.binder)?;
        }

        Ok(())
    }

    fn unregister_for_notifications(&self, name: &str, callback: &rsbinder::Strong<dyn rsbinder_hub::android::os::IServiceCallback::IServiceCallback>) -> rsbinder::status::Result<()> {
        let mut callbacks = self.name_to_registration_callbacks.write().unwrap();
        if let Some(callbacks) = callbacks.get_mut(name) {
            callbacks.retain(|c| c.as_binder().id() != callback.as_binder().id());
            Ok(())
        } else {
            log::error!("Trying to unregister callback, but none exists {}", name);
            Err(ExceptionCode::IllegalState.into())
        }
    }
}

impl Default for ServiceManagerInner {
    fn default() -> Self {
        Self {
            name_to_service: RwLock::new(HashMap::new()),
            name_to_registration_callbacks: RwLock::new(HashMap::new()),
        }
    }
}

impl rsbinder::DeathRecipient for ServiceManagerInner {
    fn binder_died(&self, who: &rsbinder::WIBinder) {
        self.name_to_service.write().unwrap().retain(|_, service| {
            !(SIBinder::downgrade(&service.binder) == *who)
        });

        self.name_to_registration_callbacks.write().unwrap().retain(|_, callbacks| {
            callbacks.retain(|callback| {
                SIBinder::downgrade(&callback.as_binder()) != *who
            });
            !callbacks.is_empty()
        });
    }
}

struct ServiceManager {
    inner: Arc<ServiceManagerInner>,
}

impl ServiceManager {
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

impl Default for ServiceManager {
    fn default() -> Self {
        Self {
            inner: Arc::new(ServiceManagerInner::default()),
        }
    }
}

impl Interface for ServiceManager {}

impl IServiceManager for ServiceManager {
    fn getService(&self,_arg_name: &str) -> rsbinder::status::Result<Option<rsbinder::SIBinder>> {
        self.inner.try_get_service(_arg_name, true)
    }

    fn addService(&self, name: &str, service: &SIBinder, allowIsolated: bool, dumpPriority: i32) -> rsbinder::status::Result<()> {
        if !Self::is_valid_service_name(name) {
            return Err(ExceptionCode::IllegalArgument.into());
        }

        if service.as_proxy().is_some() {
            service.link_to_death(self.inner.clone())?;
        }
        self.inner.add_service(name, Service {
            binder: service.clone(),
            allow_isolated: allowIsolated,
            dump_priority: dumpPriority,
            has_clients: false,
            guarentee_client: false,
            debug_pid: 0,
        })?;

        self.inner.on_registration(name)?;

        Ok(())
    }

    fn checkService(&self, name: &str) -> rsbinder::status::Result<Option<SIBinder>> {
        self.inner.try_get_service(name, false)
    }

    fn listServices(&self, dumpPriority: i32) -> rsbinder::status::Result<Vec<String>> {
        self.inner.list_services(dumpPriority)
    }

    fn registerForNotifications(&self, _arg_name: &str, _arg_callback: &rsbinder::Strong<dyn rsbinder_hub::android::os::IServiceCallback::IServiceCallback>) -> rsbinder::status::Result<()> {
        if !Self::is_valid_service_name(_arg_name) {
            return Err(ExceptionCode::IllegalArgument.into());
        }

        _arg_callback.as_binder().link_to_death(self.inner.clone())?;

        self.inner.register_for_notifications(_arg_name, _arg_callback)
    }

    fn unregisterForNotifications(&self, _arg_name: &str, _arg_callback: &rsbinder::Strong<dyn rsbinder_hub::android::os::IServiceCallback::IServiceCallback>) -> rsbinder::status::Result<()> {
        self.inner.unregister_for_notifications(_arg_name, _arg_callback)
    }

    fn isDeclared(&self,_arg_name: &str) -> rsbinder::status::Result<bool> {
        println!("isDeclared");
        Ok(false)
    }

    fn getDeclaredInstances(&self,_arg_iface: &str) -> rsbinder::status::Result<Vec<String>> {
        println!("getDeclaredInstances");
        Ok(vec![])
    }

    fn updatableViaApex(&self,_arg_name: &str) -> rsbinder::status::Result<Option<String>> {
        println!("updatableViaApex");
        Ok(None)
    }

    fn getConnectionInfo(&self,_arg_name: &str) -> rsbinder::status::Result<Option<rsbinder_hub::android::os::ConnectionInfo::ConnectionInfo>> {
        println!("getConnectionInfo");
        Ok(None)
    }

    fn registerClientCallback(&self,_arg_name: &str,_arg_service: &rsbinder::SIBinder,_arg_callback: &rsbinder::Strong<dyn rsbinder_hub::android::os::IClientCallback::IClientCallback>) -> rsbinder::status::Result<()> {
        println!("registerClientCallback");
        Ok(())
    }

    fn tryUnregisterService(&self,_arg_name: &str,_arg_service: &rsbinder::SIBinder) -> rsbinder::status::Result<()> {
        println!("tryUnregisterService");
        Ok(())
    }

    fn getServiceDebugInfo(&self) -> rsbinder::status::Result<Vec<rsbinder_hub::android::os::ServiceDebugInfo::ServiceDebugInfo>> {
        println!("getServiceDebugInfo");
        Ok(vec![])
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    ProcessState::init(DEFAULT_BINDER_PATH, 1);

    // Create a binder service.
    let service = BnServiceManager::new_binder(ServiceManager::default());
    service.addService("manager", &service.as_binder(), false, DUMP_FLAG_PRIORITY_DEFAULT)?;

    ProcessState::as_self().become_context_manager(service.as_binder())?;

    Ok(ProcessState::join_thread_pool()?)
}
