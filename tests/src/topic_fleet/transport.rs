// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! How a node finds and joins a topic by name, serves a binder and hosts a registry. Every node
//! goes through here, so the fleet runs over another transport with no change to any role.
//!
//! Over kernel binder by default: the service manager, and topic registries registered with it.
//! With `TOPIC_FLEET_SOCKETS` set to a directory (feature `topic-uds`) a node opens no binder
//! device: what the service manager did for topics, a registry on a Unix socket does, hosted by a
//! `--registry service=hub` node; a registry's socket is named after its service, and the relay
//! serves `IFleetStatus` on a socket of its own (plan 15-pubsub-uds U6).

use rsbinder::{hub, BinderResult, ProcessState, SIBinder, StatusCode, Strong};
use rsbinder_topic::registry::{Registry, RegistryAccess, RegistryResolver, RegistryStats};
use rsbinder_topic::{
    NamedSubscriber, Payload, Publisher, PublisherPolicy, Subscriber, SubscriberPolicy,
};

use super::payload::IFleetStatus;
use super::spec::Via;

/// The service name of the socket registry that stands for the service manager.
#[cfg(feature = "topic-uds")]
pub const HUB: &str = "hub";

/// The directory of the fleet's sockets; `None` over kernel binder.
#[cfg(feature = "topic-uds")]
pub fn sockets() -> Option<std::path::PathBuf> {
    std::env::var_os("TOPIC_FLEET_SOCKETS").map(std::path::PathBuf::from)
}

/// The directory of the fleet's sockets: a build without the feature has none.
#[cfg(not(feature = "topic-uds"))]
pub fn sockets() -> Option<std::path::PathBuf> {
    assert!(
        std::env::var_os("TOPIC_FLEET_SOCKETS").is_none(),
        "TOPIC_FLEET_SOCKETS needs the feature `topic-uds`"
    );
    None
}

/// What a node does before anything else: over a socket, nothing, as no device is there to open.
pub fn init() {
    if sockets().is_none() {
        ProcessState::init_default().expect("a binder device");
        ProcessState::start_thread_pool();
    }
}

#[cfg(feature = "topic-uds")]
mod socket {
    use std::path::{Path, PathBuf};

    use rsbinder::rpc::{RpcClientConfig, RpcServer, RpcServerHandle, RpcSession};
    use rsbinder::{BinderResult, SIBinder, Strong};
    use rsbinder_topic::registry::uds::{ResolverOptions, SocketDir, UdsRegistry, UdsResolver};
    use rsbinder_topic::registry::RegistryAccess;
    use rsbinder_topic::uds::{self, Endpoint};

    use super::super::spec::Via;
    use super::IFleetStatus;

    pub fn path_of(dir: &Path, service: &str) -> PathBuf {
        dir.join(format!("{service}.sock"))
    }

    pub fn resolver(dir: &Path, via: &Via) -> BinderResult<UdsResolver> {
        let service = match via {
            Via::ServiceManager => super::HUB,
            Via::Registry(service) => service,
        };
        let endpoint = Endpoint::Path(path_of(dir, service));
        UdsResolver::connect(endpoint, &ResolverOptions::default())
    }

    pub fn registry(
        dir: &Path,
        service: &str,
        access: impl RegistryAccess + 'static,
    ) -> Result<(UdsRegistry, RpcServerHandle), String> {
        let path = path_of(dir, service);
        let directory = path.with_extension("d");
        let socket_dir = SocketDir::open(&directory).map_err(|err| format!("{service}: {err}"))?;
        let registry = UdsRegistry::new(socket_dir, access);
        let server =
            RpcServer::setup_unix_server(&path).map_err(|status| format!("{service}: {status}"))?;
        registry
            .serve(&server)
            .map_err(|status| format!("{service}: {status}"))?;
        let handle = server
            .run_background()
            .map_err(|status| format!("{service}: {status}"))?;
        Ok((registry, handle))
    }

    pub fn serve(dir: &Path, service: &str, binder: SIBinder) -> BinderResult<RpcServerHandle> {
        let server = RpcServer::setup_unix_server(path_of(dir, service))?;
        uds::serve(&server, binder)
    }

    /// `IFleetStatus` on a session of its own, which the proxy holds.
    pub fn status(dir: &Path, service: &str) -> Option<Strong<dyn IFleetStatus>> {
        let wire = rsbinder::entry::WireVersion::MAX.get();
        let config = RpcClientConfig::unix(path_of(dir, service), wire);
        let session = RpcSession::setup_client_android13plus_with_config(config).ok()?;
        session.get_root().ok()?.into_interface().ok()
    }

    pub fn registered(dir: &Path) -> BinderResult<Vec<String>> {
        let hub = resolver(dir, &Via::ServiceManager)?;
        let listed = hub.list("")?;
        Ok(listed.into_iter().map(|topic| topic.name).collect())
    }

    /// What a relay's or registry's service is held as: its name lasts as long as this.
    pub type Handle = RpcServerHandle;

    pub type Registry = UdsRegistry;
}

/// The resolver of the registry registered with the service manager under `service`.
pub fn registry(service: &str) -> BinderResult<RegistryResolver> {
    RegistryResolver::new(hub::check_service(service).ok_or(StatusCode::NameNotFound)?)
}

/// A publisher found under `name` through `via`.
pub fn advertise<T: Payload + 'static>(
    name: &str,
    policy: &PublisherPolicy,
    via: &Via,
) -> BinderResult<Publisher<T>> {
    #[cfg(feature = "topic-uds")]
    if let Some(dir) = sockets() {
        return Publisher::advertise_with(name, policy, &socket::resolver(&dir, via)?);
    }
    match via {
        Via::ServiceManager => Publisher::advertise(name, policy),
        Via::Registry(service) => Publisher::advertise_with(name, policy, &registry(service)?),
    }
}

/// Give `name` to the topic of `publisher`, as one made before it was found by name is, or one
/// whose name was taken from it is again. Over a socket that is a new socket, so a subscriber
/// connected to the old one reads it as another publisher.
pub fn register<T: Payload + 'static>(name: &str, publisher: &Publisher<T>) -> BinderResult<()> {
    #[cfg(feature = "topic-uds")]
    if let Some(dir) = sockets() {
        return publisher.advertise_on(name, &socket::resolver(&dir, &Via::ServiceManager)?);
    }
    Ok(hub::add_service(name, publisher.as_binder())?)
}

/// A subscription to the topic registered under `name` with the service manager now;
/// `NameNotFound` when none is.
pub fn subscribe<T: Payload>(name: &str, policy: &SubscriberPolicy) -> BinderResult<Subscriber<T>> {
    #[cfg(feature = "topic-uds")]
    if let Some(dir) = sockets() {
        return socket::resolver(&dir, &Via::ServiceManager)?.subscribe(name, policy);
    }
    let topic = hub::check_service(name).ok_or(StatusCode::NameNotFound)?;
    Subscriber::request(&topic, policy)
}

/// A subscriber that follows whichever publisher holds `name` through `via`.
pub fn subscribe_named<T: Payload + 'static>(
    name: &str,
    policy: &SubscriberPolicy,
    via: &Via,
) -> BinderResult<NamedSubscriber<T>> {
    #[cfg(feature = "topic-uds")]
    if let Some(dir) = sockets() {
        return NamedSubscriber::subscribe_with(name, policy, &socket::resolver(&dir, via)?);
    }
    match via {
        Via::ServiceManager => NamedSubscriber::subscribe(name, policy),
        Via::Registry(service) => {
            NamedSubscriber::subscribe_with(name, policy, &registry(service)?)
        }
    }
}

/// A binder served under a name: `again` gives it the name once more, as after the service
/// manager started again. A server on a socket has no one to give it to.
pub struct Served {
    service: String,
    binder: SIBinder,
    #[cfg(feature = "topic-uds")]
    _handle: Option<socket::Handle>,
}

impl Served {
    pub fn again(&self) -> BinderResult<()> {
        #[cfg(feature = "topic-uds")]
        if self._handle.is_some() {
            return Ok(());
        }
        Ok(hub::add_service(&self.service, self.binder.clone())?)
    }
}

/// `binder`, a service of this process, found under `service` by [`status_service`].
pub fn serve_service(service: &str, binder: SIBinder) -> BinderResult<Served> {
    #[cfg(feature = "topic-uds")]
    if let Some(dir) = sockets() {
        return Ok(Served {
            service: service.to_owned(),
            _handle: Some(socket::serve(&dir, service, binder.clone())?),
            binder,
        });
    }
    hub::add_service(service, binder.clone())?;
    Ok(Served {
        service: service.to_owned(),
        binder,
        #[cfg(feature = "topic-uds")]
        _handle: None,
    })
}

/// The `IFleetStatus` served under `service`, if it is there now.
pub fn status_service(service: &str) -> Option<Strong<dyn IFleetStatus>> {
    #[cfg(feature = "topic-uds")]
    if let Some(dir) = sockets() {
        return socket::status(&dir, service);
    }
    hub::check_interface::<dyn IFleetStatus>(service).ok()
}

/// A registry this process hosts under `service`.
pub enum Hosted {
    Kernel(Registry, String),
    #[cfg(feature = "topic-uds")]
    Socket(socket::Registry, socket::Handle),
}

impl Hosted {
    pub fn stats(&self) -> RegistryStats {
        match self {
            Hosted::Kernel(registry, _) => registry.stats(),
            #[cfg(feature = "topic-uds")]
            Hosted::Socket(registry, _) => registry.stats(),
        }
    }

    /// Give the registry its name once more, as after the service manager started again.
    pub fn again(&self) -> BinderResult<()> {
        match self {
            Hosted::Kernel(registry, service) => {
                Ok(hub::add_service(service, registry.as_binder())?)
            }
            #[cfg(feature = "topic-uds")]
            Hosted::Socket(..) => Ok(()),
        }
    }
}

/// Host a topic registry under `service`, which `access` judges.
pub fn host_registry(
    service: &str,
    access: impl RegistryAccess + 'static,
) -> Result<Hosted, String> {
    #[cfg(feature = "topic-uds")]
    if let Some(dir) = sockets() {
        let (registry, handle) = socket::registry(&dir, service, access)?;
        return Ok(Hosted::Socket(registry, handle));
    }
    let registry = Registry::new(access);
    hub::add_service(service, registry.as_binder())
        .map_err(|status| format!("addService {service}: {status}"))?;
    Ok(Hosted::Kernel(registry, service.to_owned()))
}

/// The names under which topics and this fleet's services are found now.
pub fn registered() -> Vec<String> {
    #[cfg(feature = "topic-uds")]
    if let Some(dir) = sockets() {
        return socket::registered(&dir).unwrap_or_default();
    }
    hub::list_services(hub::DUMP_FLAG_PRIORITY_ALL)
}
