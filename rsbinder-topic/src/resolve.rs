// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Names for topics (plan 15 §5.7c): how a publisher makes its topic findable by a name, and how
//! a subscriber hears of every binder the name is given.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

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
    /// Give `name` to `topic`, an `ITopic` binder, until the returned [`Advertisement`] is
    /// dropped.
    ///
    /// A [`Publisher`](crate::Publisher) holds the advertisement and drops it as it ends, before
    /// it writes the topic's end record, so a resolver that takes the name back on the drop has
    /// been asked to before any subscriber can read that end. A resolver that keeps the name, as
    /// the service manager does, returns [`Advertisement::none`].
    fn advertise(&self, name: &str, topic: SIBinder) -> BinderResult<Advertisement>;

    /// Call `on_registered` with every binder `name` is given from now on, and at once with the
    /// one it has, if any. The calls stop when the returned [`Watch`] is dropped.
    ///
    /// `on_registered` may run on any thread, a binder thread among them, and must return
    /// without waiting: a [`NamedSubscriber`](crate::NamedSubscriber)'s keeps the binder and
    /// hands the subscription call to a thread of its own. Calls for the same binder may repeat.
    ///
    /// A watch made with [`Watch::with_lookup`] can also be asked for the binder the name has
    /// now, which a `NamedSubscriber` does when a call it needed may not have come (see
    /// [`Watch::with_lookup`]).
    fn watch(
        &self,
        name: &str,
        on_registered: Box<dyn Fn(SIBinder) + Send + Sync>,
    ) -> BinderResult<Watch>;
}

/// A name [`Resolver::advertise`] gave: dropping it takes the name back, as far as the resolver
/// does.
#[must_use = "dropping the advertisement at once takes the name back at once"]
pub struct Advertisement {
    _guard: Option<Box<dyn Any + Send + Sync>>,
}

impl Advertisement {
    /// An advertisement that ends when `guard` is dropped, for a [`Resolver`] of the
    /// application's own.
    pub fn new(guard: impl Any + Send + Sync) -> Self {
        Self {
            _guard: Some(Box::new(guard)),
        }
    }

    /// An advertisement whose drop does nothing: the resolver keeps the name.
    pub fn none() -> Self {
        Self { _guard: None }
    }
}

impl fmt::Debug for Advertisement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Advertisement").finish_non_exhaustive()
    }
}

/// The binder a name has now, or `None` when it has none; what [`Watch::with_lookup`] holds.
pub(crate) type Lookup = Arc<dyn Fn() -> BinderResult<Option<SIBinder>> + Send + Sync>;

/// The calls of one [`Resolver::watch`]: dropping it ends them.
#[must_use = "dropping the watch at once ends its calls at once"]
pub struct Watch {
    _guard: Box<dyn Any + Send + Sync>,
    lookup: Option<Lookup>,
}

impl Watch {
    /// A watch that ends when `guard` is dropped, for a [`Resolver`] of the application's own.
    pub fn new(guard: impl Any + Send + Sync) -> Self {
        Self {
            _guard: Box::new(guard),
            lookup: None,
        }
    }

    /// A watch that can also be asked for the binder the name has now: `lookup` returns it, or
    /// `None` when the name has none.
    ///
    /// A [`NamedSubscriber`](crate::NamedSubscriber) asks when a call of the watch it needed
    /// may not have come, as a call that is a oneway transaction fails when the receiving
    /// process has no room for it: once when a subscription is refused as the publisher ended
    /// ([`ITopic::ERROR_CLOSED`] or `DeadObject`), unless that publisher was the lookup's own
    /// answer, and once when the publisher of its connection ends and no other binder has been
    /// given the name. It asks from a thread of its own, never one that receives, and uses the
    /// answer only when no call of the watch came while it asked; after a call of a binder it
    /// cannot use, it asks once more. An `Err` counts as `None`. Without a lookup
    /// ([`Watch::new`]) the subscriber waits for the next call.
    ///
    /// [`ITopic::ERROR_CLOSED`]: crate::aidl::ITopic::ERROR_CLOSED
    pub fn with_lookup(
        guard: impl Any + Send + Sync,
        lookup: impl Fn() -> BinderResult<Option<SIBinder>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            _guard: Box::new(guard),
            lookup: Some(Arc::new(lookup)),
        }
    }

    /// The watch's lookup, if it has one.
    pub(crate) fn lookup(&self) -> Option<Lookup> {
        self.lookup.clone()
    }
}

impl fmt::Debug for Watch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watch")
            .field("lookup", &self.lookup.is_some())
            .finish_non_exhaustive()
    }
}

/// The service manager as a [`Resolver`]: a topic's name is a service name.
///
/// `advertise` is `rsbinder::hub::add_service`, `watch` is
/// `rsbinder::hub::register_for_notifications`, so the names follow the service manager's rules
/// and its access control: who may add, and who may find, the name. Android 10's service
/// manager has no registration notices, and there `watch` fails.
///
/// The service manager has no way to take a service's name back: the [`Advertisement`] does
/// nothing, and a name stays registered until its process ends. The watch's lookup is
/// `rsbinder::hub::check_service`. `rsb_hub` starts no service on demand for it; on Android 15
/// from `android-15.0.0_r6` on, `check_service` is a `getService` call, which can ask `init` to
/// start a service declared under the name (`ctl.interface_start`).
///
/// A registration notice is a call into this process: it needs the binder thread pool
/// (`rsbinder::ProcessState::start_thread_pool`), without which a watch hears nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct ServiceManager;

impl Resolver for ServiceManager {
    fn advertise(&self, name: &str, topic: SIBinder) -> BinderResult<Advertisement> {
        hub::add_service(name, topic)?;
        Ok(Advertisement::none())
    }

    fn watch(
        &self,
        name: &str,
        on_registered: Box<dyn Fn(SIBinder) + Send + Sync>,
    ) -> BinderResult<Watch> {
        let callback = BnServiceCallback::new_binder(Notice(on_registered));
        hub::register_for_notifications(name, &callback)?;
        let looked_up = name.to_owned();
        Ok(Watch::with_lookup(
            Registered {
                name: name.to_owned(),
                callback,
            },
            move || Ok(hub::check_service(&looked_up)),
        ))
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
