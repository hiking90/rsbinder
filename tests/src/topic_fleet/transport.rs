// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! How a node finds and joins a topic by name. Every node goes through here, so another
//! transport is one more variant of these calls rather than a change to every role.
//!
//! Only the service manager over kernel binder for now: the topic registry comes with the
//! registry's role, and a socket transport with plan 15's UDS work.

use rsbinder::{hub, BinderResult, StatusCode};
use rsbinder_topic::{
    NamedSubscriber, Payload, Publisher, PublisherPolicy, Subscriber, SubscriberPolicy,
};

/// A publisher registered with the service manager under `name`.
pub fn advertise<T: Payload + 'static>(
    name: &str,
    policy: &PublisherPolicy,
) -> BinderResult<Publisher<T>> {
    Publisher::advertise(name, policy)
}

/// A subscription to the topic registered under `name` now; `NameNotFound` when none is.
pub fn subscribe<T: Payload>(name: &str, policy: &SubscriberPolicy) -> BinderResult<Subscriber<T>> {
    let topic = hub::check_service(name).ok_or(StatusCode::NameNotFound)?;
    Subscriber::request(&topic, policy)
}

/// A subscriber that follows whichever publisher holds `name`.
pub fn subscribe_named<T: Payload + 'static>(
    name: &str,
    policy: &SubscriberPolicy,
) -> BinderResult<NamedSubscriber<T>> {
    NamedSubscriber::subscribe(name, policy)
}
