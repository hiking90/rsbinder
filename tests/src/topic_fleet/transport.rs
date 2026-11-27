// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! How a node finds and joins a topic by name, serves a binder and hosts a registry: the service
//! manager, and topic registries registered with it. Every node goes through here.

use rsbinder::{hub, BinderResult, ProcessState, SIBinder, StatusCode, Strong};
use rsbinder_topic::registry::{Registry, RegistryAccess, RegistryResolver, RegistryStats};
use rsbinder_topic::{
    EventTopic, NamedSubscriber, Payload, Publisher, PublisherBuilder, ServiceManager, Subscriber,
    SubscriberBuilder,
};

use super::payload::IFleetStatus;
use super::spec::Via;

/// What a node does before anything else.
pub fn init() {
    ProcessState::init_default().expect("a binder device");
    ProcessState::start_thread_pool();
}

/// The resolver of the registry registered with the service manager under `service`.
pub fn registry(service: &str) -> BinderResult<RegistryResolver> {
    RegistryResolver::new(hub::check_service(service).ok_or(StatusCode::NameNotFound)?)
}

/// A publisher found under `name` through `via`.
pub fn advertise<T: Payload + 'static>(
    name: &str,
    builder: PublisherBuilder<T, EventTopic>,
    via: &Via,
) -> BinderResult<Publisher<T>> {
    let publisher = builder.build()?;
    match via {
        Via::ServiceManager => publisher.advertise_on(name, &ServiceManager)?,
        Via::Registry(service) => publisher.advertise_on(name, &registry(service)?)?,
    }
    Ok(publisher)
}

/// Give `name` to the topic of `publisher`, as one made before it was found by name is, or one
/// whose name was taken from it is again.
pub fn register<T: Payload + 'static>(name: &str, publisher: &Publisher<T>) -> BinderResult<()> {
    hub::add_service(name, publisher.as_binder())
}

/// A subscription to the topic registered under `name` with the service manager now;
/// `NameNotFound` when none is.
pub fn subscribe<T: Payload>(
    name: &str,
    builder: SubscriberBuilder<T>,
) -> BinderResult<Subscriber<T>> {
    let topic = hub::check_service(name).ok_or(StatusCode::NameNotFound)?;
    builder.subscribe(&topic)
}

/// A subscriber that follows whichever publisher holds `name` through `via`.
pub fn subscribe_named<T: Payload + 'static>(
    name: &str,
    builder: SubscriberBuilder<T>,
    via: &Via,
) -> BinderResult<NamedSubscriber<T>> {
    match via {
        Via::ServiceManager => builder.subscribe_named(name),
        Via::Registry(service) => builder.subscribe_named_on(name, &registry(service)?),
    }
}

/// A binder served under a name: `again` gives it the name once more, as after the service
/// manager started again.
pub struct Served {
    service: String,
    binder: SIBinder,
}

impl Served {
    pub fn again(&self) -> BinderResult<()> {
        hub::add_service(&self.service, self.binder.clone())
    }
}

/// `binder`, a service of this process, found under `service` by [`status_service`].
pub fn serve_service(service: &str, binder: SIBinder) -> BinderResult<Served> {
    hub::add_service(service, binder.clone())?;
    Ok(Served {
        service: service.to_owned(),
        binder,
    })
}

/// The `IFleetStatus` served under `service`, if it is there now.
pub fn status_service(service: &str) -> Option<Strong<dyn IFleetStatus>> {
    hub::check_interface::<dyn IFleetStatus>(service).ok()
}

/// A registry this process hosts under a service name.
pub struct Hosted {
    registry: Registry,
    service: String,
}

impl Hosted {
    pub fn stats(&self) -> RegistryStats {
        self.registry.stats()
    }

    /// Give the registry its name once more, as after the service manager started again.
    pub fn again(&self) -> BinderResult<()> {
        hub::add_service(&self.service, self.registry.as_binder())
    }
}

/// Host a topic registry under `service`, which `access` judges.
pub fn host_registry(
    service: &str,
    access: impl RegistryAccess + 'static,
) -> Result<Hosted, String> {
    let registry = Registry::new(access);
    hub::add_service(service, registry.as_binder())
        .map_err(|status| format!("addService {service}: {status}"))?;
    Ok(Hosted {
        registry,
        service: service.to_owned(),
    })
}

/// The names under which topics and this fleet's services are found now.
pub fn registered() -> Vec<String> {
    hub::list_services(hub::DUMP_FLAG_PRIORITY_ALL)
}
