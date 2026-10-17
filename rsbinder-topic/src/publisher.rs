// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The sending end of a topic, and the subscription call it answers (plan 15 §5.1a, §5.4a).
//!
//! A subscription is taken in this order, the steps PUB1–PUB7 of the plan. The application's own
//! `ITopic`, if it has one, checks the caller (PUB1). [`Acceptor::accept`] then checks the
//! request, takes the wake socket of a subscription woken through an fd, maps the control page,
//! duplicates the topic's fd and links to the token's death (PUB2–PUB5), all outside the
//! publisher's lock; under it, it takes out the subscriptions that ended, refuses or adds this
//! one, and reads where it starts (PUB6). Nothing after that can fail on this side (PUB7), so a
//! subscription in the list is one whose subscriber got the reply or will learn that it did not.

use std::fmt;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Weak};

use rsbinder::fmq::shm::Mapping;
use rsbinder::{
    BinderResult, CallingContext, ExceptionCode, Interface, SIBinder, Status, StatusCode,
};

use crate::aidl::ITopic::{
    BnTopic, ITopic, ERROR_CLOSED, ERROR_FULL, ERROR_VERSION, WAKE_FD, WAKE_FUTEX,
};
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::aidl::TopicEndpoint::TopicEndpoint;
use crate::connect::{asked_here, refuse, status_of};
use crate::layout::{self, ControlView, Stored, HEADER_OFFSET, VERSION};
use crate::link::{Dead, Forget, Link};
use crate::publish::TypedWriter;
use crate::resolve::{Resolver, ServiceManager};
use crate::{wake, Authorize, Payload, PublishError, PublisherPolicy, PublisherStats};

/// The sending end of a topic of `T`: it writes values into the topic's memory and takes
/// subscriptions to it.
///
/// A publish never waits for a subscriber. When the ring is full the oldest records are
/// overwritten, and a subscriber that had not read them is told how many it lost. Any thread may
/// publish (`publish` takes `&self`): share the publisher in an `Arc` to publish from several.
///
/// Subscribers reach the topic through a binder: [`as_binder`](Self::as_binder), which takes
/// every subscription the [policy](PublisherPolicy) has room for, or an `ITopic` of the
/// application's own that checks the caller first and hands the request to an
/// [`acceptor`](Self::acceptor). Register the binder with the service manager, or return it from
/// a method of another interface as an `IBinder`. Neither holds the publisher: dropping the
/// publisher ends the topic even while the service manager or a subscriber still holds the
/// binder, and later subscriptions are refused with [`ITopic::ERROR_CLOSED`].
///
/// Dropping the publisher, like [`close`](Self::close), writes the topic's end record: a
/// subscriber receives every record before it and then
/// [`Closed::End`](crate::Closed::End). A publisher whose process dies writes nothing; its
/// subscribers learn of it through binder and read what was written before.
///
/// [`ITopic::ERROR_CLOSED`]: crate::aidl::ITopic::ERROR_CLOSED
pub struct Publisher<T: Payload> {
    shared: Arc<Shared<T>>,
    binder: SIBinder,
}

/// What the publisher, the acceptors and the death notifications share.
struct Shared<T> {
    writer: TypedWriter<T, Stored<Mapping>>,
    /// The topic memory, duplicated into every reply.
    fd: OwnedFd,
    capacity: u64,
    max_subscribers: usize,
    max_per_uid: usize,
    authorize: Option<Authorize>,
}

impl<T: Payload + 'static> Publisher<T> {
    /// Make a topic with `policy`.
    ///
    /// `BadValue` when `policy.ring_bytes` is outside 4 KiB to 1 GiB. `Unsupported` off Linux and
    /// Android, on a target where a 64-bit word of a read-only mapping cannot be loaded, and on a
    /// kernel without `F_SEAL_FUTURE_WRITE`. An OS error when the memory cannot be made.
    pub fn new(policy: &PublisherPolicy) -> rsbinder::fmq::Result<Self> {
        let (fd, writer) = TypedWriter::create(policy)?;
        let capacity = writer.lock().writer.capacity();
        let shared = Arc::new(Shared {
            writer,
            fd,
            capacity,
            max_subscribers: policy.max_subscribers,
            max_per_uid: policy.max_per_uid,
            authorize: policy.authorize.clone(),
        });
        let acceptor = Acceptor {
            topic: Arc::downgrade(&shared) as Weak<dyn Accept>,
        };
        let binder = BnTopic::new_binder(Service { acceptor }).as_binder();
        Ok(Self { shared, binder })
    }

    /// Make a topic with `policy` and register its binder with the service manager under
    /// `name`, for [`NamedSubscriber`](crate::NamedSubscriber)s of the name to find.
    ///
    /// The name follows the service manager's rules for a service name; a name it refuses, or a
    /// caller it does not let add a service, fails here. The errors of [`new`](Self::new) come
    /// back as a status: `EX_UNSUPPORTED_OPERATION` for `Unsupported`, `BadValue`, or an OS
    /// error's code. Who may subscribe is [`PublisherPolicy::authorize`].
    ///
    /// Dropping the publisher ends the topic but leaves the name registered until the process
    /// ends; a subscriber that reaches it after that is refused with
    /// [`ITopic::ERROR_CLOSED`](crate::aidl::ITopic::ERROR_CLOSED) and waits for the next
    /// registration of the name.
    pub fn advertise(name: &str, policy: &PublisherPolicy) -> BinderResult<Self> {
        Self::advertise_with(name, policy, &ServiceManager)
    }

    /// [`advertise`](Self::advertise) through `resolver` instead of the service manager.
    pub fn advertise_with(
        name: &str,
        policy: &PublisherPolicy,
        resolver: &dyn Resolver,
    ) -> BinderResult<Self> {
        let publisher = Self::new(policy).map_err(status_of)?;
        resolver.advertise(name, publisher.as_binder())?;
        Ok(publisher)
    }
}

impl<T: Payload + 'static> Publisher<T> {
    /// Write `value` as one record. It is encoded before the publisher's lock is taken.
    pub fn publish(&self, value: &T) -> Result<(), PublishError> {
        self.shared.writer.publish(value)
    }

    /// Write each value as one record, all of them or, on any error, none; the count written.
    ///
    /// Every value is encoded first, outside the publisher's lock, then the records are written
    /// under one lock and announced to the subscribers once.
    pub fn publish_all<'a, I>(&self, values: I) -> Result<usize, PublishError>
    where
        T: 'a,
        I: IntoIterator<Item = &'a T>,
    {
        self.shared.writer.publish_all(values)
    }

    /// The futex wakes the topic has sent.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn wakes(&self) -> u64 {
        self.shared.writer.wakes()
    }

    /// The wake bytes the topic has sent.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn sends(&self) -> u64 {
        self.shared.writer.sends()
    }

    /// Run `pause` in every commit between its `seq` bump and its look at the subscribers.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn pause_after_bump(&self, pause: Box<dyn FnMut() + Send>) {
        self.shared.writer.lock().after_bump = Some(pause);
    }

    /// What the topic has published, and each subscription it holds after taking out those that
    /// ended: who made it and what its control page reports (plan 15 §5.9a).
    pub fn stats(&self) -> PublisherStats {
        self.shared.writer.stats()
    }

    /// The subscriptions the topic holds, after taking out those that ended.
    ///
    /// For skipping the work of a value no one would receive; it changes as subscribers come
    /// and go.
    pub fn subscriber_count(&self) -> usize {
        let mut state = self.shared.writer.lock();
        let gone = state.links.prune();
        let count = state.links.len();
        drop(state);
        drop(gone);
        count
    }

    /// End the topic with `status`: subscribers receive the records before it, then
    /// [`Closed::Status`](crate::Closed::Status) with this status, or
    /// [`Closed::End`](crate::Closed::End) for an ok one. Later publishes are
    /// [`PublishError::Closed`] and later subscriptions are refused with
    /// [`ITopic::ERROR_CLOSED`]. Closing again does nothing.
    ///
    /// The status's message is cut to 1 KiB.
    ///
    /// [`ITopic::ERROR_CLOSED`]: crate::aidl::ITopic::ERROR_CLOSED
    pub fn close(&self, status: &Status) {
        self.shared.writer.close(status);
    }

    /// A handle for an `ITopic` of the application's own, which decides who may subscribe.
    pub fn acceptor(&self) -> Acceptor {
        Acceptor {
            topic: Arc::downgrade(&self.shared) as Weak<dyn Accept>,
        }
    }

    /// An `ITopic` binder that takes every subscription the policy has room for.
    ///
    /// The same binder every time. It refers to the publisher without keeping it.
    pub fn as_binder(&self) -> SIBinder {
        self.binder.clone()
    }
}

impl<T: Payload> Drop for Publisher<T> {
    fn drop(&mut self) {
        self.shared.writer.close(&Status::from(ExceptionCode::None));
    }
}

impl<T: Payload> fmt::Debug for Publisher<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Publisher")
            .field("type", &T::type_name())
            .finish_non_exhaustive()
    }
}

/// Takes subscriptions to one [`Publisher`] on behalf of an `ITopic` the application implements.
///
/// An application that decides who may subscribe implements
/// [`ITopic`](crate::aidl::ITopic::ITopic) itself: its `subscribe` checks the caller with
/// `rsbinder::CallingContext` (uid, pid, SELinux context), returns an `EX_SECURITY` status to
/// refuse, and calls [`accept`](Self::accept) to admit.
///
/// ```no_run
/// use rsbinder::{BinderResult, CallingContext, ExceptionCode, Status};
/// use rsbinder_topic::aidl::ITopic::{BnTopic, ITopic};
/// use rsbinder_topic::aidl::SubscribeRequest::SubscribeRequest;
/// use rsbinder_topic::aidl::TopicEndpoint::TopicEndpoint;
/// use rsbinder_topic::{Acceptor, Bytes, Publisher, PublisherPolicy};
///
/// struct Guarded {
///     acceptor: Acceptor,
/// }
///
/// impl rsbinder::Interface for Guarded {}
///
/// impl ITopic for Guarded {
///     fn r#subscribe(&self, request: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
///         if CallingContext::default().uid != 1000 {
///             return Err(Status::from(ExceptionCode::Security));
///         }
///         self.acceptor.accept(request)
///     }
/// }
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default())?;
/// let topic = BnTopic::new_binder(Guarded { acceptor: publisher.acceptor() });
/// // Register `topic.as_binder()`, then publish.
/// # Ok(()) }
/// ```
///
/// The acceptor refers to the publisher without keeping it: once the publisher is dropped,
/// `accept` refuses with [`ITopic::ERROR_CLOSED`].
///
/// [`ITopic::ERROR_CLOSED`]: crate::aidl::ITopic::ERROR_CLOSED
#[derive(Clone)]
pub struct Acceptor {
    topic: Weak<dyn Accept>,
}

impl Acceptor {
    /// Take the subscription `request` asks for, or say why not.
    ///
    /// Refusals: `EX_SECURITY` when [`PublisherPolicy::authorize`] does not admit the caller,
    /// checked first; [`ITopic::ERROR_VERSION`] for another layout version; `BadType` when the
    /// request names another payload type; `BadValue` for a token of this process that came
    /// through binder (its death would say nothing about the caller), a nonzero pin count, a
    /// `wake` that is neither [`ITopic::WAKE_FUTEX`] nor [`ITopic::WAKE_FD`], a `wakeFd` that is
    /// not a unix stream socket for `WAKE_FD` or not null for `WAKE_FUTEX`, or a control page
    /// that is not a sealed memfd of the size, magic, version and pin count the request states;
    /// [`ITopic::ERROR_FULL`] when the publisher holds as many subscriptions as its policy allows,
    /// in all or from the caller's uid; [`ITopic::ERROR_CLOSED`] once the publisher is closed or
    /// dropped; `DeadObject` when the caller died during the call.
    ///
    /// A token of a third process cannot be told from the caller's own: its subscription lasts
    /// until that process dies or the publisher does, and `max_per_uid` bounds how many a caller
    /// holds that way.
    ///
    /// Call it from the handler of the subscription call, on the thread the call arrived on:
    /// the uid it counts against `max_per_uid` is that call's, and a subscriber of this process
    /// is recognized by the thread its call is made on.
    ///
    /// [`ITopic::ERROR_VERSION`]: crate::aidl::ITopic::ERROR_VERSION
    /// [`ITopic::ERROR_FULL`]: crate::aidl::ITopic::ERROR_FULL
    /// [`ITopic::ERROR_CLOSED`]: crate::aidl::ITopic::ERROR_CLOSED
    /// [`ITopic::WAKE_FUTEX`]: crate::aidl::ITopic::WAKE_FUTEX
    /// [`ITopic::WAKE_FD`]: crate::aidl::ITopic::WAKE_FD
    pub fn accept(&self, request: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        match self.topic.upgrade() {
            Some(topic) => topic.accept(request),
            None => Err(refuse(ERROR_CLOSED, "the publisher has been dropped")),
        }
    }
}

impl fmt::Debug for Acceptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Acceptor").finish_non_exhaustive()
    }
}

/// [`Shared`] without its payload type, which the acceptor does not have.
trait Accept: Send + Sync {
    fn accept(self: Arc<Self>, request: &SubscribeRequest) -> BinderResult<TopicEndpoint>;
}

impl<T: Payload + 'static> Accept for Shared<T> {
    fn accept(self: Arc<Self>, request: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        if let Some(authorize) = &self.authorize {
            if !authorize.admits(&CallingContext::default()) {
                return Err(Status::from((
                    ExceptionCode::Security,
                    "the publisher's policy does not admit the caller",
                )));
            }
        }
        if request.version != VERSION as i32 {
            return Err(refuse(
                ERROR_VERSION,
                "subscriber of another layout version",
            ));
        }
        if request.r#type != T::type_name() {
            return Err(StatusCode::BadType.into());
        }
        if request.pins != 0 || ![WAKE_FUTEX, WAKE_FD].contains(&request.wake) {
            return Err(StatusCode::BadValue.into());
        }
        let token = request.token.as_ref().ok_or(StatusCode::UnexpectedNull)?;
        let control = request.control.as_ref().ok_or(StatusCode::UnexpectedNull)?;
        if token.as_proxy().is_none() && !asked_here(token) {
            return Err(StatusCode::BadValue.into());
        }
        // The subscriber's socket: sent on with per-call flags only, which it cannot change.
        let socket = match (request.wake, &request.wakeFd) {
            (WAKE_FUTEX, None) => None,
            (WAKE_FD, Some(socket)) if wake::is_stream_socket(socket.as_fd()) => {
                let socket = socket.as_fd().try_clone_to_owned();
                let socket = socket.map_err(StatusCode::from)?;
                wake::limit_send_buffer(&socket).map_err(StatusCode::from)?;
                Some(socket)
            }
            _ => return Err(StatusCode::BadValue.into()),
        };
        let control = ControlView::map(control.as_fd(), 0).map_err(status_of)?;
        let fd = self.fd.try_clone().map_err(StatusCode::from)?;
        let ring = layout::descriptor(fd, self.capacity).try_into()?;
        let caller = CallingContext::default();
        let uid = caller.uid;
        let list = Arc::downgrade(&self) as Weak<dyn Forget>;
        let link = Link::new(control, (uid, caller.pid), socket, token, list)?;
        let link = Arc::new(link);

        let mut state = self.writer.lock();
        let gone = state.links.prune();
        let taken = if link.dead() {
            Err(StatusCode::DeadObject.into())
        } else if state.writer.ended() {
            Err(refuse(ERROR_CLOSED, "the publisher has been closed"))
        } else if state.links.len() >= self.max_subscribers
            || state.links.of_uid(uid) >= self.max_per_uid
        {
            Err(refuse(
                ERROR_FULL,
                "the publisher has no room for another subscription",
            ))
        } else {
            state.links.push(link.clone());
            Ok((state.writer.position(), state.writer.next_index()))
        };
        drop(state);
        // Unmapped and unlinked here, after the lock: `link` too, when it was refused.
        drop(gone);
        drop(link);
        let (start, index) = taken?;
        Ok(TopicEndpoint {
            version: VERSION as i32,
            ring,
            headerOffset: HEADER_OFFSET as i32,
            // The bits of the `u64` position and `u32` index; the subscriber reads them back so.
            start: start as i64,
            startIndex: index as i32,
        })
    }
}

impl<T: Payload> Forget for Shared<T> {
    fn forget(&self, dead: &Arc<Dead>) {
        let mut state = self.writer.lock();
        let gone = state.links.remove(dead);
        drop(state);
        drop(gone);
    }
}

/// The binder [`Publisher::as_binder`] returns.
struct Service {
    acceptor: Acceptor,
}

impl Interface for Service {}

impl ITopic for Service {
    fn r#subscribe(&self, request: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        self.acceptor.accept(request)
    }
}
