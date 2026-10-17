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
use std::os::fd::{AsFd, BorrowedFd};
#[cfg(feature = "tokio")]
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
#[cfg(feature = "tokio")]
use std::task::Poll;
use std::time::{Duration, Instant};

use rsbinder::fmq::shm::spin::{spin_until, SpinGauge};
use rsbinder::fmq::shm::Mapping;
use rsbinder::fmq::Descriptor;
use rsbinder::{
    BinderResult, Interface, ParcelFileDescriptor, SIBinder, Status, StatusCode, Strong,
};
#[cfg(feature = "tokio")]
use tokio::io::{unix::AsyncFd, Interest};

use crate::aidl::ITopic::{ITopic, ERROR_VERSION, WAKE_FD, WAKE_FUTEX};
use crate::aidl::Pool::Pool;
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::aidl::TopicEndpoint::TopicEndpoint;
use crate::connect::{refuse, status_of, Asking, Token};
use crate::layout::{self, ControlPage, Loaded, LoadedPool, CLOSED, HEADER_OFFSET, VERSION};
use crate::link::DeathLink;
use crate::pool::MAX_PINS;
use crate::receive::TypedReader;
use crate::ring::{self, Loads, Reader};
#[cfg(feature = "tokio")]
use crate::wait::{on_pool, Leftover};
use crate::wait::{Died, Local, Parts, Slept, Words, ABANDON, DIED, INTERRUPT};
use crate::wake::{self, FdWake};
use crate::{poll, Closed, Payload, RecvError, SubscriberStats};

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

/// How the publisher wakes a subscriber that waits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Wake {
    /// A futex on a word of the topic's memory: one wake system call per commit serves every
    /// subscriber of this kind that sleeps, however many. A wait holds a thread: `recv` the
    /// calling one, `recv_async` one of the blocking pool.
    #[default]
    Futex,
    /// A byte on a socket of this subscription alone, which the publisher sends without waiting
    /// when a commit finds the subscriber waiting: one system call per such subscriber. The
    /// subscription has file descriptors to poll ([`Wait`](crate::Wait)), so one thread can wait
    /// for several with [`wait_any`](crate::wait_any) or an event loop of its own, and
    /// `recv_async` waits on the runtime's reactor without holding a thread. One file descriptor
    /// to poll; the subscription holds two in this process (both ends of a socket pair it makes,
    /// one to poll and one to wake its own waits with) and the publisher one.
    Fd,
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
    /// The largest pool this subscriber maps, in bytes. Default 256 MiB.
    ///
    /// A publisher that lends slots ([`PoolPolicy`](crate::PoolPolicy)) sends a pool besides the
    /// ring, and the subscriber maps all of it read-only, so this bounds the address space it
    /// can make the subscriber's process give up as `max_ring_bytes` does. A larger pool is
    /// refused with `BadValue`.
    pub max_pool_bytes: usize,
    /// How the publisher wakes this subscriber. Default [`Wake::Futex`].
    pub wake: Wake,
}

impl Default for SubscriberPolicy {
    fn default() -> Self {
        Self {
            start: Start::Latest,
            max_ring_bytes: 16 << 20,
            max_pool_bytes: 256 << 20,
            wake: Wake::Futex,
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
/// | `recv_async` (`tokio` feature) | on the blocking pool; with [`Wake::Fd`], the reactor |
/// | [`Wait`](crate::Wait), [`wait_any`](crate::wait_any) ([`Wake::Fd`]) | the caller polls |
///
/// A wait first looks at the ring for up to 20 µs, the policy of
/// [`rsbinder::fmq::shm::spin`], then sleeps until the publisher commits, as
/// [`SubscriberPolicy::wake`] says: on a word of the topic's memory, for which the publisher
/// makes one wake system call per commit when some subscriber sleeps, whatever their number, and
/// none when none does; or on file descriptors of the subscription. An [`Interrupter`] ends a
/// wait from another thread.
pub struct Subscriber<T: Payload> {
    reader: TypedReader<T, Loaded<Arc<Mapping>>>,
    control: Closing,
    /// The publisher's pool, for a topic that lends slots.
    #[allow(dead_code, reason = "plan 15 P3-3 reads loaned values out of it")]
    pool: Option<LoadedPool>,
    /// [`Wake::Fd`]'s fds on the reactor; dropped before `local` closes them.
    #[cfg(feature = "tokio")]
    reactor: Option<Reactor>,
    local: Arc<Local<Parts>>,
    spin: SpinGauge,
    /// Values returned and records that failed to decode, for [`SubscriberStats`].
    received: u64,
    decode_errors: u64,
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
    /// What the last sync wait did, for a test that reports a slow one.
    #[cfg(all(test, feature = "tokio"))]
    pub(crate) trace: Trace,
}

/// The steps of one sync wait: how long it took to join the pool's, and each sleep.
#[cfg(all(test, feature = "tokio"))]
#[derive(Default)]
#[allow(
    dead_code,
    reason = "read by `wait_tests`, which runs on Linux and Android only"
)]
pub(crate) struct Trace {
    pub(crate) join: Duration,
    /// How each sleep ended, how long it lasted, and the `seq` it slept on.
    pub(crate) parks: Vec<(Slept, Duration, u32)>,
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
    /// sealed memfd of the expected layout, a ring larger than
    /// [`max_ring_bytes`](SubscriberPolicy::max_ring_bytes), a pool that is not a sealed memfd of
    /// the slots it states or is larger than
    /// [`max_pool_bytes`](SubscriberPolicy::max_pool_bytes), or a wake fd that is not what
    /// [`wake`](SubscriberPolicy::wake) asked for (a unix stream socket for [`Wake::Fd`], none
    /// for [`Wake::Futex`]).
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
        let (fd, page) = ControlPage::create(MAX_PINS).map_err(status_of)?;
        let control = Closing(Arc::new(page));
        let token = rsbinder::Binder::new(Token).as_binder();
        let local = Arc::new(Local::new());
        let death = DeathLink::new(topic, Died(local.clone()))?;
        let service: Strong<dyn ITopic> = topic.clone().into_interface()?;
        // With `Wake::Fd`, a pair of this side's own: the publisher sends on one end, as does this
        // process on a duplicate of it, and the subscriber polls the other.
        let (fd_wake, sent) = match policy.wake {
            Wake::Futex => (None, None),
            Wake::Fd => {
                let (theirs, polled) = wake::pair().map_err(StatusCode::from)?;
                let local = theirs.try_clone().map_err(StatusCode::from)?;
                let sent = ParcelFileDescriptor::new(theirs);
                let fd_wake = FdWake::new(polled, local).map_err(StatusCode::from)?;
                (Some(fd_wake), Some(sent))
            }
        };
        let request = SubscribeRequest {
            version: VERSION as i32,
            r#type: T::type_name().to_owned(),
            token: Some(token.clone()),
            control: Some(ParcelFileDescriptor::from(fd)),
            pins: MAX_PINS as i32,
            wake: match policy.wake {
                Wake::Futex => WAKE_FUTEX,
                Wake::Fd => WAKE_FD,
            },
            wakeFd: sent,
        };
        let endpoint = {
            let _asking = Asking::new(&token);
            service.r#subscribe(&request)?
        };
        // The control page's fd and the publisher's end: it has its own, and this side keeps
        // the mapping and a duplicate.
        drop(request);
        let memory = map_endpoint(&endpoint, policy.max_ring_bytes)?;
        let pool = match &endpoint.pool {
            Some(pool) => Some(map_pool(pool, policy.max_pool_bytes)?),
            None => None,
        };
        local.set_words(Parts {
            seq: memory.seq(),
            control: control.0.clone(),
            fd: fd_wake,
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
            pool,
            #[cfg(feature = "tokio")]
            reactor: None,
            local,
            spin: SpinGauge::default(),
            received: 0,
            decode_errors: 0,
            #[cfg(feature = "tokio")]
            leftover: Arc::default(),
            _token: token,
            _death: death,
            #[cfg(test)]
            about_to_park: None,
            #[cfg(all(test, feature = "tokio"))]
            trace: Trace::default(),
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

    /// [`recv`](Self::recv) for an async caller.
    ///
    /// With [`Wake::Futex`] the wait sleeps on a thread of the blocking pool
    /// (`tokio::task::spawn_blocking`, or the calling thread inside a binder transaction), so
    /// it holds that thread, not an executor thread, for as long as it lasts. With [`Wake::Fd`]
    /// it waits on the runtime's reactor (`tokio::io::unix::AsyncFd`) and holds no thread.
    ///
    /// Cancel-safe: dropping the future loses no record. A wait it left on the pool is woken,
    /// and the next receive that may wait joins it before looking at the ring. Besides `recv`'s
    /// results, [`RecvError::Cancelled`] when the pool cannot run the wait, or the reactor
    /// cannot take the subscription's file descriptors.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime, as `tokio::task::spawn_blocking` does, and with [`Wake::Fd`] in a
    /// runtime whose IO driver is off, as `AsyncFd::register` does.
    #[cfg(feature = "tokio")]
    pub async fn recv_async(&mut self) -> Result<T, RecvError> {
        if self.local.words().fd.is_some() {
            return self.recv_on_reactor().await;
        }
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

    /// `recv_async` of a [`Wake::Fd`] subscription: its fd on the runtime's reactor.
    #[cfg(feature = "tokio")]
    async fn recv_on_reactor(&mut self) -> Result<T, RecvError> {
        loop {
            match self.look(false) {
                Err(RecvError::Empty) => {}
                other => return other,
            }
            if self.local.take(INTERRUPT) {
                return Err(RecvError::Cancelled);
            }
            let seen = self.local.words().seq();
            // No `ABANDON` here: only a futex wake's pool wait and a dropped subscriber raise it.
            if self.reader.pending() || self.local.stopped() {
                continue;
            }
            if self.reactor.is_none() {
                let fd = self.local.words().fd.as_ref().map(FdWake::fd);
                let made = fd.map(Reactor::new).transpose();
                self.reactor = made.map_err(|_| RecvError::Cancelled)?;
            }
            let Some(reactor) = &self.reactor else {
                return Err(RecvError::Cancelled);
            };
            #[cfg(test)]
            if let Some(hook) = self.about_to_park.as_mut() {
                hook();
            }
            let armed = Armed(&self.local);
            if self.local.arm(seen) != 0 || self.local.words().seq() != seen {
                continue;
            }
            let woke = reactor.readable().await;
            drop(armed);
            if woke.is_err() {
                // A reactor that went away with its runtime: the next call takes the current one.
                self.reactor = None;
                return Err(RecvError::Cancelled);
            }
        }
    }

    /// A handle that ends this subscriber's wait from another thread.
    pub fn interrupter(&self) -> Interrupter {
        Interrupter {
            target: Target::Subscriber(Arc::downgrade(&self.local)),
        }
    }

    /// What this subscription has received, counted in this process (plan 15 §5.9a).
    pub fn stats(&self) -> SubscriberStats {
        SubscriberStats {
            received: self.received,
            lost: self.reader.lost(),
            decode_errors: self.decode_errors,
            connections: 1,
        }
    }

    /// Where the next record to return starts.
    pub(crate) fn position(&self) -> u64 {
        self.reader.position()
    }

    /// Where the publisher's committed records end now.
    pub(crate) fn committed(&self) -> u64 {
        self.reader.committed()
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
    pub(crate) fn wait(&mut self, deadline: Option<Instant>, latest: bool) -> Result<T, RecvError> {
        #[cfg(feature = "tokio")]
        {
            #[cfg(test)]
            let joining = Instant::now();
            // Before the first look: a wait left on the pool would take the next wake.
            let joined = self.leftover.join(deadline);
            #[cfg(test)]
            {
                self.trace = Trace {
                    join: joining.elapsed(),
                    parks: Vec::new(),
                };
            }
            if !joined {
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
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(RecvError::Timeout);
            }
            #[cfg(test)]
            if let Some(hook) = self.about_to_park.as_mut() {
                hook();
            }
            let parked_at = Instant::now();
            let slept = self.local.park(seen, deadline);
            #[cfg(all(test, feature = "tokio"))]
            self.trace.parks.push((slept, parked_at.elapsed(), seen));
            match slept {
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

    fn settle(&mut self, received: Result<T, RecvError>, died: bool) -> Result<T, RecvError> {
        let page = &self.control.0;
        page.read().store(self.reader.position(), Ordering::Relaxed);
        page.lost().store(self.reader.lost(), Ordering::Relaxed);
        match &received {
            Ok(_) => self.received += 1,
            Err(RecvError::Decode(_)) => self.decode_errors += 1,
            Err(_) => {}
        }
        match received {
            Err(RecvError::Empty) if died => Err(RecvError::Closed(Closed::PublisherDied)),
            other => other,
        }
    }

    /// The publisher's pool as this subscriber mapped it, for tests of the subscription.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn pool(&self) -> Option<&LoadedPool> {
        self.pool.as_ref()
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

impl<T: Payload> poll::Sealed for Subscriber<T> {}

impl<T: Payload> poll::Wait for Subscriber<T> {
    fn wait_fd(&self) -> Option<BorrowedFd<'_>> {
        self.local.words().fd.as_ref().map(FdWake::fd)
    }

    fn prepare_wait(&mut self) -> bool {
        assert!(
            self.local.words().fd.is_some(),
            "prepare_wait: a subscription made with `Wake::Futex` has no file descriptor to poll"
        );
        let seen = self.local.words().seq();
        if self.reader.pending() || self.local.died() {
            return false;
        }
        #[cfg(test)]
        if let Some(hook) = self.about_to_park.as_mut() {
            hook();
        }
        // An interrupt stays up for the next receive that waits; its byte ends this poll.
        if self.local.arm(seen) & DIED != 0 || self.local.words().seq() != seen {
            self.local.disarm();
            return false;
        }
        true
    }

    fn finish_wait(&mut self) {
        self.local.disarm();
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

/// Ends the wait [`Local::arm`] started when dropped, also with the future it lives in.
#[cfg(feature = "tokio")]
struct Armed<'a>(&'a Local<Parts>);

#[cfg(feature = "tokio")]
impl Drop for Armed<'_> {
    fn drop(&mut self) {
        self.0.disarm();
    }
}

/// A [`Wake::Fd`] subscription's fd, registered with the runtime's reactor.
#[cfg(feature = "tokio")]
struct Reactor(AsyncFd<Unowned>);

/// An fd the subscriber owns, as the reactor takes it.
#[cfg(feature = "tokio")]
struct Unowned(RawFd);

#[cfg(feature = "tokio")]
impl AsRawFd for Unowned {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[cfg(feature = "tokio")]
impl Reactor {
    fn new(fd: BorrowedFd<'_>) -> std::io::Result<Self> {
        let unowned = Unowned(fd.as_raw_fd());
        // SAFETY: `local` owns the fd and outlives `reactor`, the field declared before it.
        let registered = unsafe { AsyncFd::register_with_interest(unowned, Interest::READABLE) };
        Ok(Self(registered?))
    }

    /// Until the fd is readable; its readiness is cleared, as the wait's end drains it.
    async fn readable(&self) -> std::io::Result<()> {
        std::future::poll_fn(|cx| match self.0.poll_read_ready(cx) {
            Poll::Ready(Ok(mut ready)) => {
                ready.clear_ready();
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        })
        .await
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
    target: Target,
}

/// What an [`Interrupter`] ends the wait of.
#[derive(Clone)]
enum Target {
    Subscriber(Weak<Local<Parts>>),
    Named(Weak<dyn Interrupt>),
}

/// A subscriber whose waits an [`Interrupter`] ends.
pub(crate) trait Interrupt: Send + Sync {
    fn interrupt(&self);
}

impl Interrupter {
    pub(crate) fn named(named: Weak<dyn Interrupt>) -> Self {
        Self {
            target: Target::Named(named),
        }
    }

    /// End the wait in progress, or the next one. Returns once that wait is out of its sleep, or
    /// at once when none sleeps; does nothing after the subscriber has been dropped.
    pub fn interrupt(&self) {
        match &self.target {
            Target::Subscriber(local) => {
                if let Some(local) = local.upgrade() {
                    local.raise(INTERRUPT);
                }
            }
            Target::Named(named) => {
                if let Some(named) = named.upgrade() {
                    named.interrupt();
                }
            }
        }
    }

    /// `interrupt`, and the wakes it sent.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn interrupt_counting(&self) -> usize {
        match &self.target {
            Target::Subscriber(local) => local.upgrade().map_or(0, |local| local.raise(INTERRUPT)),
            Target::Named(_) => {
                self.interrupt();
                0
            }
        }
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

/// Map the pool an endpoint names, no larger than `max_pool`.
fn map_pool(pool: &Pool, max_pool: usize) -> BinderResult<LoadedPool> {
    let (Ok(slots), Ok(slot_bytes)) = (u32::try_from(pool.slots), u32::try_from(pool.slotBytes))
    else {
        return Err(StatusCode::BadValue.into());
    };
    let memory = pool.memory.as_ref().ok_or(StatusCode::UnexpectedNull)?;
    layout::map_pool(memory.as_fd(), slots, slot_bytes, max_pool).map_err(status_of)
}
