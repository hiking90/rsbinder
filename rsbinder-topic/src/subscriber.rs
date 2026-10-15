// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The receiving end of a topic (plan 15 §5.1a, §5.4a, §5.5, §5.8).
//!
//! [`Subscriber::request`] takes the steps SUB1–SUB8 of the plan: it checks the binder, makes
//! the control page and the token, links to the publisher's death, makes the call, checks the
//! reply, maps the topic memory and sets the read position. From the call on, every way out
//! that is not a subscriber drops the control page with `CLOSED` set in it, so a publisher that
//! added the subscription before its reply was lost takes it out at its next publish.
//!
//! A receive that waits looks at the ring, spins a little ([`rsbinder::fmq::shm::spin`]), and
//! sleeps on the topic's `seq` by the protocol in `wait.rs`.

use std::fmt;
use std::os::fd::AsFd;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use rsbinder::fmq::shm::spin::{spin_until, SpinGauge};
use rsbinder::fmq::shm::{Deadline, Mapping};
use rsbinder::fmq::Descriptor;
use rsbinder::{
    BinderResult, Interface, ParcelFileDescriptor, SIBinder, Status, StatusCode, Strong,
};

use crate::aidl::ITopic::{ITopic, ERROR_VERSION};
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::aidl::TopicEndpoint::TopicEndpoint;
use crate::connect::{refuse, status_of, Asking, Token};
use crate::layout::{self, ControlPage, Loaded, CLOSED, HEADER_OFFSET, VERSION};
use crate::link::DeathLink;
use crate::receive::TypedReader;
use crate::ring::{self, Loads, Reader};
#[cfg(feature = "tokio")]
use crate::wait::{on_pool, Leftover};
use crate::wait::{Died, Local, Parts, Slept, Words, ABANDON, INTERRUPT};
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
///
/// # Receiving
///
/// | Call | No record unread |
/// |---|---|
/// | [`try_recv`](Self::try_recv), [`try_latest`](Self::try_latest) | [`RecvError::Empty`] at once |
/// | [`recv`](Self::recv), [`recv_latest`](Self::recv_latest) | waits for one |
/// | [`recv_timeout`](Self::recv_timeout) | waits, then [`RecvError::Timeout`] |
/// | `recv_async` (`tokio` feature) | waits on the blocking pool |
///
/// A wait first looks at the ring for up to 20 µs, the policy of
/// [`rsbinder::fmq::shm::spin`], then sleeps on a word of the topic's memory until the publisher
/// commits. The publisher makes one wake system call per commit when some subscriber sleeps,
/// whatever their number, and none when none does. An [`Interrupter`] ends a wait from another
/// thread.
pub struct Subscriber<T: Payload> {
    reader: TypedReader<T, Loaded<Arc<Mapping>>>,
    control: Closing,
    local: Arc<Local<Parts>>,
    spin: SpinGauge,
    /// The wait a dropped `recv_async` left on the blocking pool, which the next wait joins.
    #[cfg(feature = "tokio")]
    leftover: Arc<Leftover>,
    /// The token the publisher links to; alive as long as the subscription.
    _token: SIBinder,
    /// On the publisher's binder; `None` when the publisher is in this process.
    _death: Option<DeathLink<Died>>,
    /// A test's pause before a wait sleeps.
    #[cfg(test)]
    pub(crate) about_to_park: Option<Box<dyn FnMut() + Send + Sync>>,
}

/// The control page, marked `CLOSED` when dropped. A wait left on the pool shares it.
struct Closing(Arc<ControlPage>);

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
    /// that its publisher died, and a wait for a record never ends.
    pub fn request(topic: &SIBinder, policy: &SubscriberPolicy) -> BinderResult<Self> {
        layout::check_target().map_err(status_of)?;
        if topic.as_proxy().is_none() && topic.is_remote() {
            return Err(StatusCode::InvalidOperation.into());
        }
        let (fd, page) = ControlPage::create(0).map_err(status_of)?;
        let control = Closing(Arc::new(page));
        let token = rsbinder::Binder::new(Token).as_binder();
        let local = Arc::new(Local::new());
        let death = DeathLink::new(topic, Died(local.clone()))?;
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
        local.set_words(Parts {
            seq: memory.seq(),
            control: control.0.clone(),
        });
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
            local,
            spin: SpinGauge::default(),
            #[cfg(feature = "tokio")]
            leftover: Arc::default(),
            _token: token,
            _death: death,
            #[cfg(test)]
            about_to_park: None,
        })
    }

    /// The next value, without waiting.
    ///
    /// [`RecvError::Empty`] when no record is unread. After the publisher's process died, every
    /// record it committed is still returned, and then
    /// [`Closed::PublisherDied`](crate::Closed::PublisherDied).
    pub fn try_recv(&mut self) -> Result<T, RecvError> {
        self.look(false)
    }

    /// The newest value, without waiting; the records before it are skipped and not reported as
    /// lost. [`RecvError::Empty`] when no record is unread.
    pub fn try_latest(&mut self) -> Result<T, RecvError> {
        self.look(true)
    }

    /// The next value, waiting for one.
    ///
    /// Returns what [`try_recv`](Self::try_recv) does once a record is unread, or
    /// [`RecvError::Cancelled`] when an [`Interrupter`] ends the wait. After the publisher's
    /// process died, it returns the records committed before the death and then
    /// [`Closed::PublisherDied`](crate::Closed::PublisherDied) without waiting.
    pub fn recv(&mut self) -> Result<T, RecvError> {
        self.wait(None, false)
    }

    /// [`recv`](Self::recv) with a bound: [`RecvError::Timeout`] once `timeout` passes with no
    /// record unread. A `timeout` too long for an `Instant` waits without one.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<T, RecvError> {
        let deadline = Instant::now().checked_add(timeout);
        match deadline {
            Some(deadline) => self.wait(Some(deadline), false),
            None => self.wait(None, false),
        }
    }

    /// The newest value, waiting until some record is unread: [`try_latest`](Self::try_latest)
    /// after the wait [`recv`](Self::recv) makes.
    pub fn recv_latest(&mut self) -> Result<T, RecvError> {
        self.wait(None, true)
    }

    /// [`recv`](Self::recv) for an async caller: the wait sleeps on a thread of the blocking pool
    /// (`tokio::task::spawn_blocking`, or the calling thread inside a binder transaction), so
    /// it holds that thread, not an executor thread, for as long as it lasts.
    ///
    /// Cancel-safe: dropping the future loses no record. The wait it left on the pool is woken,
    /// and the next receive that may wait joins it before looking at the ring. Besides `recv`'s
    /// results, [`RecvError::Cancelled`] when the pool cannot run the wait.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime, as `tokio::task::spawn_blocking` does.
    #[cfg(feature = "tokio")]
    pub async fn recv_async(&mut self) -> Result<T, RecvError> {
        loop {
            if self.leftover.busy() {
                self.leftover.join_async().await;
            }
            self.local.take(ABANDON);
            match self.look(false) {
                Err(RecvError::Empty) => {}
                other => return other,
            }
            if self.local.take(INTERRUPT) {
                return Err(RecvError::Cancelled);
            }
            let seen = self.local.words().seq();
            if self.reader.pending() || self.local.stopped() {
                continue;
            }
            #[cfg(test)]
            if let Some(hook) = self.about_to_park.as_mut() {
                hook();
            }
            let mut abandon = Abandon {
                local: self.local.clone(),
                armed: true,
            };
            let ran = on_pool(&self.local, &self.leftover, seen).await;
            abandon.armed = false;
            if ran.is_err() {
                return Err(RecvError::Cancelled);
            }
        }
    }

    /// A handle that ends this subscriber's wait from another thread.
    pub fn interrupter(&self) -> Interrupter {
        Interrupter {
            local: Arc::downgrade(&self.local),
        }
    }

    /// The next record, or the newest with `latest`, without waiting.
    fn look(&mut self, latest: bool) -> Result<T, RecvError> {
        let died = self.local.died();
        let received = if latest {
            self.reader.try_latest()
        } else {
            self.reader.try_recv()
        };
        self.settle(received, died)
    }

    /// A receive that waits until `deadline`, or without a bound.
    fn wait(&mut self, deadline: Option<Instant>, latest: bool) -> Result<T, RecvError> {
        #[cfg(feature = "tokio")]
        {
            // Before the first look: a wait left on the pool would take the next wake.
            if !self.leftover.join(deadline) {
                return Err(RecvError::Timeout);
            }
            self.local.take(ABANDON);
        }
        // After a wake, look without spinning: the publisher has just committed or a bit is up.
        let mut woke = false;
        loop {
            match self.look(latest) {
                Err(RecvError::Empty) => {}
                other => return other,
            }
            if self.local.take(INTERRUPT) {
                return Err(RecvError::Cancelled);
            }
            // Stale: past the join no wait of this subscriber is out. Left up, it ends every park.
            self.local.take(ABANDON);
            if !woke && self.spun(deadline) {
                continue;
            }
            let seen = self.local.words().seq();
            if self.reader.pending() || self.local.stopped() {
                woke = true;
                continue;
            }
            let sleep_until = match deadline {
                None => None,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(RecvError::Timeout);
                    }
                    Deadline::after(left)
                }
            };
            #[cfg(test)]
            if let Some(hook) = self.about_to_park.as_mut() {
                hook();
            }
            let parked_at = Instant::now();
            match self.local.park(seen, sleep_until.as_ref()) {
                Slept::Woke => self.spin.parked(parked_at.elapsed()),
                Slept::TimedOut | Slept::Stopped => {}
            }
            woke = true;
        }
    }

    /// Look at the ring for a spin's budget; whether a record or a bit in `stop` turned up.
    fn spun(&self, deadline: Option<Instant>) -> bool {
        let budget = self.spin.budget(deadline);
        let (found, spun) = spin_until(budget, || self.reader.pending() || self.local.stopped());
        self.spin.spun(spun, found);
        found
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

    /// This subscriber's waiting state, for tests that pause or watch a wait.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn local(&self) -> &Arc<Local<Parts>> {
        &self.local
    }

    /// The record of a wait left on the pool, for tests that see it end.
    #[cfg(all(
        test,
        not(loom),
        feature = "tokio",
        any(target_os = "linux", target_os = "android")
    ))]
    pub(crate) fn leftover(&self) -> &Arc<Leftover> {
        &self.leftover
    }
}

impl<T: Payload> Drop for Subscriber<T> {
    fn drop(&mut self) {
        // A wait left on the pool: woken, so it lets go of the memory now rather than at a commit.
        self.local.raise(ABANDON);
    }
}

impl<T: Payload> fmt::Debug for Subscriber<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscriber")
            .field("type", &T::type_name())
            .finish_non_exhaustive()
    }
}

/// Raises `ABANDON` when the `recv_async` future it lives in is dropped mid-wait.
#[cfg(feature = "tokio")]
struct Abandon {
    local: Arc<Local<Parts>>,
    armed: bool,
}

#[cfg(feature = "tokio")]
impl Drop for Abandon {
    fn drop(&mut self) {
        if self.armed {
            self.local.raise(ABANDON);
        }
    }
}

/// Ends a [`Subscriber`]'s wait from another thread; made by
/// [`Subscriber::interrupter`].
///
/// A receive that waits returns [`RecvError::Cancelled`]: the one waiting when
/// [`interrupt`](Self::interrupt) is called, or else the next one to wait. A receive that finds a
/// record does not wait and does not consume the interrupt. The subscription goes on either way.
#[derive(Clone)]
pub struct Interrupter {
    local: Weak<Local<Parts>>,
}

impl Interrupter {
    /// End the wait in progress, or the next one. Returns once that wait is out of its sleep, or
    /// at once when none sleeps; does nothing after the subscriber has been dropped.
    pub fn interrupt(&self) {
        if let Some(local) = self.local.upgrade() {
            local.raise(INTERRUPT);
        }
    }

    /// `interrupt`, and the wakes it sent.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn interrupt_counting(&self) -> usize {
        self.local
            .upgrade()
            .map_or(0, |local| local.raise(INTERRUPT))
    }
}

impl fmt::Debug for Interrupter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interrupter").finish_non_exhaustive()
    }
}

/// SUB6 and SUB7: the reply's version and descriptor, then the topic memory it names, mapped.
fn map_endpoint(endpoint: &TopicEndpoint, max_ring: usize) -> BinderResult<Loaded<Arc<Mapping>>> {
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
