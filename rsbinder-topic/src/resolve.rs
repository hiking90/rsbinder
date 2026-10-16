// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Names for topics (plan 15 §5.7c): how a publisher makes its topic findable by a name, and how
//! a subscriber hears of every binder the name is given.

use std::any::Any;
use std::fmt;

use rsbinder::hub::{self, BnServiceCallback, IServiceCallback};
use rsbinder::{BinderResult, Interface, SIBinder, Strong};

/// Turns topic names into topic binders, for [`Publisher::advertise_with`] and
/// [`NamedSubscriber::subscribe_with`].
///
/// [`ServiceManager`] is the implementation [`Publisher::advertise`] and
/// [`NamedSubscriber::subscribe`] use.
///
/// [`Publisher::advertise`]: crate::Publisher::advertise
/// [`Publisher::advertise_with`]: crate::Publisher::advertise_with
/// [`NamedSubscriber::subscribe`]: crate::NamedSubscriber::subscribe
/// [`NamedSubscriber::subscribe_with`]: crate::NamedSubscriber::subscribe_with
pub trait Resolver: Send + Sync {
    /// Give `name` to `topic`, an `ITopic` binder.
    fn advertise(&self, name: &str, topic: SIBinder) -> BinderResult<()>;

    /// Call `on_registered` with every binder `name` is given from now on, and at once with the
    /// one it has, if any. The calls stop when the returned [`Watch`] is dropped.
    ///
    /// `on_registered` may run on any thread, a binder thread among them, and must return
    /// without waiting: a [`NamedSubscriber`](crate::NamedSubscriber)'s keeps the binder and
    /// hands the subscription call to a thread of its own. Calls for the same binder may repeat.
    fn watch(
        &self,
        name: &str,
        on_registered: Box<dyn Fn(SIBinder) + Send + Sync>,
    ) -> BinderResult<Watch>;
}

/// The calls of one [`Resolver::watch`]: dropping it ends them.
pub struct Watch {
    _guard: Box<dyn Any + Send + Sync>,
}

impl Watch {
    /// A watch that ends when `guard` is dropped, for a [`Resolver`] of the application's own.
    pub fn new(guard: impl Any + Send + Sync) -> Self {
        Self {
            _guard: Box::new(guard),
        }
    }
}

impl fmt::Debug for Watch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watch").finish_non_exhaustive()
    }
}

/// The service manager as a [`Resolver`]: a topic's name is a service name.
///
/// `advertise` is `rsbinder::hub::add_service`, `watch` is
/// `rsbinder::hub::register_for_notifications`, so the names follow the service manager's rules
/// and its access control: who may add, and who may find, the name. Android 10's service
/// manager has no registration notices, and there `watch` fails.
///
/// A registration notice is a call into this process: it needs the binder thread pool
/// (`rsbinder::ProcessState::start_thread_pool`), without which a watch hears nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct ServiceManager;

impl Resolver for ServiceManager {
    fn advertise(&self, name: &str, topic: SIBinder) -> BinderResult<()> {
        hub::add_service(name, topic)
    }

    fn watch(
        &self,
        name: &str,
        on_registered: Box<dyn Fn(SIBinder) + Send + Sync>,
    ) -> BinderResult<Watch> {
        let callback = BnServiceCallback::new_binder(Notice(on_registered));
        hub::register_for_notifications(name, &callback)?;
        Ok(Watch::new(Registered {
            name: name.to_owned(),
            callback,
        }))
    }
}

/// The service manager's callback for one watch.
struct Notice(Box<dyn Fn(SIBinder) + Send + Sync>);

impl Interface for Notice {}

impl IServiceCallback for Notice {
    fn r#onRegistration(&self, _name: &str, service: &SIBinder) -> BinderResult<()> {
        (self.0)(service.clone());
        Ok(())
    }
}

/// Unregisters the callback when the watch is dropped.
struct Registered {
    name: String,
    callback: Strong<dyn IServiceCallback>,
}

impl Drop for Registered {
    fn drop(&mut self) {
        // A failure leaves a callback whose calls find the subscriber gone; nothing else to do.
        let _ = hub::unregister_for_notifications(&self.name, &self.callback);
    }
}
