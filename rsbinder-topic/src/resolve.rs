// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Names for topics (plan 15 §5.7c): how a publisher makes its topic findable by a name, and how
//! a subscriber hears of every binder the name is given.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use rsbinder::hub;
use rsbinder::{BinderResult, SIBinder};

/// Turns topic names into topic binders, for [`Publisher::advertise_with`] and
/// [`NamedSubscriber::subscribe_with`].
///
/// [`ServiceManager`] is the implementation [`Publisher::advertise`] and
/// [`NamedSubscriber::subscribe`] use; [`RegistryResolver`] is a registry of topic names, apart
/// from service names; `registry::uds::UdsResolver` (feature `uds`) is one on a Unix socket.
///
/// [`RegistryResolver`]: crate::registry::RegistryResolver
/// [`Publisher::advertise`]: crate::Publisher::advertise
/// [`Publisher::advertise_with`]: crate::Publisher::advertise_with
/// [`NamedSubscriber::subscribe`]: crate::NamedSubscriber::subscribe
/// [`NamedSubscriber::subscribe_with`]: crate::NamedSubscriber::subscribe_with
pub trait Resolver: Send + Sync {
    /// Give `name` to `topic`, an `ITopic` binder, until the returned [`Advertisement`] is
    /// dropped.
    ///
    /// A [`Publisher`](crate::Publisher) holds the advertisement and drops it as it ends, before
    /// it writes the topic's end record, so a drop that waits holds that record back from every
    /// subscriber: taking the name back must not wait for the resolver's host. A resolver that
    /// keeps the name, as the service manager does, returns [`Advertisement::none`].
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

    /// [`watch`](Self::watch) for a resolver whose binders need something kept alive while they
    /// are used, as the RPC session of one over a socket: each call brings it along in a
    /// [`Resolved`], and a [`NamedSubscriber`](crate::NamedSubscriber), which calls this method,
    /// keeps it for as long as it uses the binder. By default the calls of `watch`, with nothing
    /// to keep.
    ///
    /// An `Err` says that the resolver gave up on the publisher the name was given, with no
    /// binder to show for it, and that asking again would not change that: the subscriber
    /// reports it as a refusal by that publisher, until the next `Ok`.
    fn watch_resolved(&self, name: &str, on_resolved: OnResolved) -> BinderResult<Watch> {
        self.watch(
            name,
            Box::new(move |binder| on_resolved(Ok(Resolved::new(binder)))),
        )
    }

    /// The binder of the process that holds the names and the watches, if the resolver knows
    /// one; `None` by default.
    ///
    /// A [`NamedSubscriber`](crate::NamedSubscriber) links to its death: once it dies, no call
    /// of a watch comes again, and the subscriber returns
    /// [`Closed::ResolverDied`](crate::Closed::ResolverDied) once no connection is left.
    /// [`Publisher::advertise_on`](crate::Publisher::advertise_on) asks no resolver of the same
    /// host again for the name and binder it holds: a registry given the same name and binder
    /// twice keeps one entry, which the drop of either advertisement would take back.
    ///
    /// A [`RegistryResolver`](crate::registry::RegistryResolver) returns the registry's binder.
    /// [`ServiceManager`] returns none: a death link on the service manager's binder (handle 0)
    /// marks this process's proxy of it dead, and once a restarted service manager serves
    /// handle 0, every call through that proxy, the next watch of a name among them, fails with
    /// `DeadObject`.
    fn host(&self) -> Option<SIBinder> {
        None
    }
}

/// What [`Resolver::watch_resolved`] calls with each binder the name is given.
pub type OnResolved = Box<dyn Fn(BinderResult<Resolved>) + Send + Sync>;

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

/// A binder a name was given, with what has to stay alive while it is used.
#[derive(Clone)]
pub struct Resolved {
    binder: SIBinder,
    session: Option<Arc<dyn Any + Send + Sync>>,
}

impl Resolved {
    /// `binder`, with nothing to keep alive.
    pub fn new(binder: SIBinder) -> Self {
        Self {
            binder,
            session: None,
        }
    }

    /// `binder`, with `session` kept alive by whoever uses it. Dropping the last clone of
    /// `session` can end the binder's session: a connection in use keeps a clone, so a
    /// resolver that has moved on to another publisher ends none that still drains.
    pub fn with_session(binder: SIBinder, session: Arc<dyn Any + Send + Sync>) -> Self {
        Self {
            binder,
            session: Some(session),
        }
    }

    /// The binder.
    pub fn binder(&self) -> &SIBinder {
        &self.binder
    }

    /// The binder, on its own: its session is no longer kept alive by this value.
    pub fn into_binder(self) -> SIBinder {
        self.binder
    }

    pub(crate) fn into_parts(self) -> (SIBinder, Option<Arc<dyn Any + Send + Sync>>) {
        (self.binder, self.session)
    }
}

impl fmt::Debug for Resolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolved")
            .field("binder", &self.binder)
            .field("session", &self.session.is_some())
            .finish()
    }
}

/// The binder a name has now, or `None` when it has none; what [`Watch::with_lookup`] holds.
pub(crate) type Lookup = Arc<dyn Fn() -> BinderResult<Option<Resolved>> + Send + Sync>;

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
    /// cannot use, it asks once more. An `Err` counts as `None`, and so does a panic of
    /// `lookup`, which is caught and logged; a call that came while it ran is used. Without a
    /// lookup ([`Watch::new`]) the subscriber waits for the next call.
    ///
    /// The catch does not repair `lookup`'s own state, and `lookup` is asked again at the next
    /// occasion (one that panicked over a poisoned lock of its own panics each time), so keep it
    /// panic-free and treat the catch as a safety net. It relies on `panic = "unwind"` (Rust's
    /// default); a `panic = "abort"` build ends the process at the panic.
    ///
    /// [`ITopic::ERROR_CLOSED`]: crate::aidl::ITopic::ERROR_CLOSED
    pub fn with_lookup(
        guard: impl Any + Send + Sync,
        lookup: impl Fn() -> BinderResult<Option<SIBinder>> + Send + Sync + 'static,
    ) -> Self {
        Self::with_resolved_lookup(guard, move || Ok(lookup()?.map(Resolved::new)))
    }

    /// [`with_lookup`](Self::with_lookup) for a resolver of [`Resolved`] binders
    /// ([`Resolver::watch_resolved`]).
    pub fn with_resolved_lookup(
        guard: impl Any + Send + Sync,
        lookup: impl Fn() -> BinderResult<Option<Resolved>> + Send + Sync + 'static,
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
/// `advertise` is `rsbinder::hub::add_service`, `watch` is `rsbinder::hub::watch`, so the names
/// follow the service manager's rules and its access control: who may add, and who may find,
/// the name. Android 10's service manager has no registration notices, and there `watch` fails
/// with `EX_UNSUPPORTED_OPERATION`.
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
        let watch = hub::watch(name, on_registered)?;
        let looked_up = name.to_owned();
        Ok(Watch::with_lookup(watch, move || {
            Ok(hub::check_service(&looked_up))
        }))
    }
}
