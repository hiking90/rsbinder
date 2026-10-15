// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The receiving end of a topic (plan 15 §5.1a, §5.4a, §5.8).
//!
//! [`Subscriber::request`] takes the steps SUB1–SUB8 of the plan: it checks the binder, makes
//! the control page and the token, links to the publisher's death, makes the call, checks the
//! reply, maps the topic memory and sets the read position. From the call on, every way out
//! that is not a subscriber drops the control page with `CLOSED` set in it, so a publisher that
//! added the subscription before its reply was lost takes it out at its next publish.

use std::fmt;
use std::os::fd::AsFd;
use std::sync::atomic::Ordering;

use rsbinder::fmq::shm::Mapping;
use rsbinder::fmq::Descriptor;
use rsbinder::{
    BinderResult, Interface, ParcelFileDescriptor, SIBinder, Status, StatusCode, Strong,
};

use crate::aidl::ITopic::{ITopic, ERROR_VERSION};
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::aidl::TopicEndpoint::TopicEndpoint;
use crate::connect::{refuse, status_of, Asking, Token};
use crate::layout::{self, ControlPage, Loaded, CLOSED, HEADER_OFFSET, VERSION};
use crate::link::{Dead, DeathLink};
use crate::receive::TypedReader;
use crate::ring::{self, Loads, Reader};
use crate::{Closed, Payload, RecvError};

/// Where a subscription starts reading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Start {
    /// At the first record published after the publisher took the subscription.
    #[default]
    Latest,
    /// At the newest record, so that the first receive returns the current value, then on from
    /// there. On a topic with no record yet, as `Latest`.
    Last,
    /// At the oldest record still in the ring: as much history as the ring holds.
    Oldest,
}

/// How a subscriber subscribes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriberPolicy {
    /// Where to start reading. Default [`Start::Latest`].
    pub start: Start,
    /// The largest ring this subscriber maps, in bytes. Default 16 MiB.
    ///
    /// The publisher chooses the ring's size and the subscriber maps all of it read-only, so
    /// this bounds the address space a publisher can make the subscriber's process give up. A
    /// larger ring is refused with `BadValue`.
    pub max_ring_bytes: usize,
}

impl Default for SubscriberPolicy {
    fn default() -> Self {
        Self {
            start: Start::Latest,
            max_ring_bytes: 16 << 20,
        }
    }
}

/// The receiving end of a topic of `T`, made by [`request`](Self::request).
///
/// It maps the topic's memory read-only and reads records at its own pace; nothing it does
/// reaches the publisher or another subscriber. Dropping it ends the subscription: the publisher
/// takes it out at its next publish or subscription.
pub struct Subscriber<T: Payload> {
    reader: TypedReader<T, Loaded<Mapping>>,
    control: Closing,
    /// The token the publisher links to; alive as long as the subscription.
    _token: SIBinder,
    /// On the publisher's binder; `None` when the publisher is in this process.
    death: Option<DeathLink<Dead>>,
}

/// The control page, marked `CLOSED` when dropped.
struct Closing(ControlPage);

impl Drop for Closing {
    fn drop(&mut self) {
        self.0.state().fetch_or(CLOSED, Ordering::Release);
    }
}

impl<T: Payload> Subscriber<T> {
    /// Subscribe to the topic behind `topic`, an `ITopic` binder: a kernel binder proxy, or a
    /// binder of this process.
    ///
    /// One twoway call; after it, receiving makes none. The refusals of the publisher come back
    /// as it sent them ([`Acceptor::accept`](crate::Acceptor::accept) lists them, and an
    /// application's `ITopic` may refuse with `EX_SECURITY`). Of its own, `request` returns
    /// `InvalidOperation` for an RPC binder, `EX_UNSUPPORTED_OPERATION` where topics cannot work
    /// (off Linux and Android, or on a kernel without `F_SEAL_FUTURE_WRITE`),
    /// [`ITopic::ERROR_VERSION`](crate::aidl::ITopic::ERROR_VERSION) for a publisher of another
    /// layout version, and `BadValue` for a reply it cannot use: topic memory that is not a
    /// sealed memfd of the expected layout, or a ring larger than
    /// [`max_ring_bytes`](SubscriberPolicy::max_ring_bytes).
    ///
    /// **The process has to run the binder thread pool**
    /// (`rsbinder::ProcessState::start_thread_pool`) for a publisher in another process: the
    /// notice of its death arrives on a pool thread, and without it a subscriber never learns
    /// that its publisher died.
    pub fn request(topic: &SIBinder, policy: &SubscriberPolicy) -> BinderResult<Self> {
        layout::check_target().map_err(status_of)?;
        if topic.as_proxy().is_none() && topic.is_remote() {
            return Err(StatusCode::InvalidOperation.into());
        }
        let (fd, page) = ControlPage::create(0).map_err(status_of)?;
        let control = Closing(page);
        let token = rsbinder::Binder::new(Token).as_binder();
        let death = DeathLink::new(topic, Dead::default())?;
        let service: Strong<dyn ITopic> = topic.clone().into_interface()?;
        let request = SubscribeRequest {
            version: VERSION as i32,
            r#type: T::type_name().to_owned(),
            token: Some(token.clone()),
            control: Some(ParcelFileDescriptor::from(fd)),
            pins: 0,
        };
        let endpoint = {
            let _asking = Asking::new(&token);
            service.r#subscribe(&request)?
        };
        // The control page's fd: the publisher has its mapping, and this side keeps its own.
        drop(request);
        let memory = map_endpoint(&endpoint, policy.max_ring_bytes)?;
        let start = match policy.start {
            // The bits of the position and the index, as the publisher wrote them.
            Start::Latest => ring::Start::Latest {
                at: endpoint.start as u64,
                index: endpoint.startIndex as u32,
            },
            Start::Last => ring::Start::Last,
            Start::Oldest => ring::Start::Oldest,
        };
        let reader = Reader::attach(memory, start).map_err(|_| StatusCode::BadValue)?;
        Ok(Self {
            reader: TypedReader::new(reader),
            control,
            _token: token,
            death,
        })
    }

    /// The next value, without waiting.
    ///
    /// [`RecvError::Empty`] when no record is unread. After the publisher's process died, every
    /// record it committed is still returned, and then
    /// [`Closed::PublisherDied`](crate::Closed::PublisherDied).
    pub fn try_recv(&mut self) -> Result<T, RecvError> {
        let died = self.died();
        let received = self.reader.try_recv();
        self.settle(received, died)
    }

    /// The newest value, without waiting; the records before it are skipped and not reported as
    /// lost. [`RecvError::Empty`] when no record is unread.
    pub fn try_latest(&mut self) -> Result<T, RecvError> {
        let died = self.died();
        let received = self.reader.try_latest();
        self.settle(received, died)
    }

    /// Whether the publisher died; loaded before the ring, so the read finds its last commits.
    fn died(&self) -> bool {
        self.death
            .as_ref()
            .is_some_and(|link| link.recipient().get())
    }

    fn settle(&self, received: Result<T, RecvError>, died: bool) -> Result<T, RecvError> {
        let page = &self.control.0;
        page.read().store(self.reader.position(), Ordering::Relaxed);
        page.lost().store(self.reader.lost(), Ordering::Relaxed);
        match received {
            Err(RecvError::Empty) if died => Err(RecvError::Closed(Closed::PublisherDied)),
            other => other,
        }
    }
}

impl<T: Payload> fmt::Debug for Subscriber<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscriber")
            .field("type", &T::type_name())
            .finish_non_exhaustive()
    }
}

/// SUB6 and SUB7: the reply's version and descriptor, then the topic memory it names, mapped.
fn map_endpoint(endpoint: &TopicEndpoint, max_ring: usize) -> BinderResult<Loaded<Mapping>> {
    if endpoint.version != VERSION as i32 {
        return Err(refuse(ERROR_VERSION, "publisher of another layout version"));
    }
    let bad_value = || Status::from(StatusCode::BadValue);
    if endpoint.headerOffset != HEADER_OFFSET as i32 {
        return Err(bad_value());
    }
    let ring = Descriptor::try_from(&endpoint.ring)?;
    let [fd] = ring.fds.as_slice() else {
        return Err(bad_value());
    };
    if ring.quantum != 1 {
        return Err(bad_value());
    }
    let memory = layout::map_topic(fd.as_fd(), max_ring).map_err(status_of)?;
    if ring.grantors != layout::grantors(memory.capacity()) {
        return Err(bad_value());
    }
    Ok(memory)
}
