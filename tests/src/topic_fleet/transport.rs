// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! How a node finds and joins a topic by name. Every node goes through here, so another
//! transport is one more variant of these calls rather than a change to every role.
//!
//! The service manager or a topic registry over kernel binder for now; a socket transport comes
//! with plan 15's UDS work.

use rsbinder::{hub, BinderResult, StatusCode};
use rsbinder_topic::registry::RegistryResolver;
use rsbinder_topic::{
    NamedSubscriber, Payload, Publisher, PublisherPolicy, Subscriber, SubscriberPolicy,
};

use super::spec::Via;

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
    match via {
        Via::ServiceManager => Publisher::advertise(name, policy),
        Via::Registry(service) => Publisher::advertise_with(name, policy, &registry(service)?),
    }
}

/// A subscription to the topic registered under `name` with the service manager now;
/// `NameNotFound` when none is.
pub fn subscribe<T: Payload>(name: &str, policy: &SubscriberPolicy) -> BinderResult<Subscriber<T>> {
    let topic = hub::check_service(name).ok_or(StatusCode::NameNotFound)?;
    Subscriber::request(&topic, policy)
}

/// A subscriber that follows whichever publisher holds `name` through `via`.
pub fn subscribe_named<T: Payload + 'static>(
    name: &str,
    policy: &SubscriberPolicy,
    via: &Via,
) -> BinderResult<NamedSubscriber<T>> {
    match via {
        Via::ServiceManager => NamedSubscriber::subscribe(name, policy),
        Via::Registry(service) => {
            NamedSubscriber::subscribe_with(name, policy, &registry(service)?)
        }
    }
}
