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
use std::marker::PhantomData;
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

use crate::aidl::ISubscription::BnSubscription;
use crate::aidl::ITopic::{ITopic, ERROR_VERSION, WAKE_FD, WAKE_FUTEX};
use crate::aidl::Pool::Pool;
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::aidl::TopicEndpoint::TopicEndpoint;
use crate::connect::{refuse, status_of, Asking, Token};
use crate::layout::{self, ControlPage, Loaded, LoadedPool, CLOSED, HEADER_OFFSET, VERSION};
use crate::link::{Dead, DeathLink};
use crate::pool::{PoolReader, MAX_PINS};
use crate::receive::{Held, TypedReader};
use crate::ring::{self, Loads, Reader};
use crate::wait::{
    decide, Died, Local, Looked, Next, Parts, Slept, Words, ABANDON, ALL, INTERRUPTED, NOTICE,
};
#[cfg(feature = "tokio")]
use crate::wait::{on_pool, Armed, Leftover, REACTOR_ENDS};
use crate::wake::{self, FdWake};
use crate::{
    poll, Bytes, Closed, Payload, RecvError, Sample, SubscriberStats, Trust, Trusted, Untrusted,
};

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
///
/// # Vouching for the publisher
///
/// `M` says whether this subscriber vouches for its publisher, which decides how a value the
/// publisher lent is read ([`Sample`]). `request` makes an [`Untrusted`] subscriber; on a topic of
/// [`Bytes`], `unsafe` [`trust_publisher`](Subscriber::trust_publisher) makes it
/// [`Trusted`](crate::Trusted).
pub struct Subscriber<T: Payload, M: Trust = Untrusted> {
    /// Raises `ABANDON` on drop, first of the fields: a pool wait lets go of the memory now.
    _abandon: Abandon,
    reader: TypedReader<T, Loaded<Arc<Mapping>>>,
    control: Closing,
    /// [`Wake::Fd`]'s fds on the reactor; dropped before `local` closes them.
    #[cfg(feature = "tokio")]
    reactor: Option<Reactor>,
    local: Arc<Local<Parts>>,
    spin: SpinGauge,
    /// What the latest look found, which every look sets; a wait decides on it.
    looked: Looked,
    /// Values returned and records that failed to decode, for [`SubscriberStats`].
    received: u64,
    decode_errors: u64,
    /// The wait a `recv_async` handed to the blocking pool, which the next wait joins.
    #[cfg(feature = "tokio")]
    leftover: Arc<Leftover>,
    /// The token the publisher links to; alive as long as the subscription.
    _token: SIBinder,
    /// On the publisher's binder; `None` when the publisher is in this process.
    _death: Option<DeathLink<Died>>,
    /// Set by the death notice of the publisher's binder, which `DIED` alone does not tell.
    dead: Arc<Dead>,
    /// A look read the topic's end record: the publisher refuses every later subscription.
    read_end: bool,
    /// A test's pause before a wait loads the `seq` it decides on.
    #[cfg(test)]
    pub(crate) before_seen: Option<Box<dyn FnMut() + Send + Sync>>,
    /// A test's pause before a wait sleeps.
    #[cfg(test)]
    pub(crate) about_to_park: Option<Box<dyn FnMut() + Send + Sync>>,
    /// What the last sync wait did, for a test that reports a slow one.
    #[cfg(all(test, feature = "tokio"))]
    pub(crate) trace: Trace,
    _trust: PhantomData<fn() -> M>,
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
    /// binder of this process. The subscriber does not vouch for the publisher ([`Untrusted`]).
    ///
    /// One twoway call; after it, receiving makes none. The refusals of the publisher come back
    /// as it sent them ([`Acceptor::accept`](crate::Acceptor::accept) lists them, and an
    /// application's `ITopic` may refuse with `EX_SECURITY`). Of its own, `request` returns
    /// `InvalidOperation` for an RPC binder, `EX_UNSUPPORTED_OPERATION` where topics cannot work
    /// (off Linux and Android, or on a kernel without `F_SEAL_FUTURE_WRITE`),
    /// [`ITopic::ERROR_VERSION`](crate::aidl::ITopic::ERROR_VERSION) for a publisher of another
    /// layout version, and `BadValue` for a reply it cannot use: topic memory that is not a
    /// sealed memfd of the expected layout, a ring larger than
    /// [`max_ring_bytes`](SubscriberPolicy::max_ring_bytes), or a pool that is not a sealed memfd
    /// of the slots it states or is larger than
    /// [`max_pool_bytes`](SubscriberPolicy::max_pool_bytes).
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
        let local = Arc::new(Local::new());
        let token = Token {
            local: Arc::downgrade(&local),
        };
        let token = BnSubscription::new_binder(token).as_binder();
        let dead = Arc::new(Dead::default());
        let died = Died {
            local: local.clone(),
            dead: dead.clone(),
        };
        let death = DeathLink::new(topic, died)?;
        let service: Strong<dyn ITopic> = topic.clone().into_interface()?;
        // `Wake::Fd`: a pair of this side's own; the publisher and this process send, it polls.
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
            let _asking = Asking::new(&request);
            service.r#subscribe(&request)?
        };
        // The publisher holds its own fds now; this side keeps the mapping and a duplicate.
        drop(request);
        let memory = map_endpoint(&endpoint, policy.max_ring_bytes)?;
        let pool = match &endpoint.pool {
            Some(pool) => {
                let pool = map_pool(pool, policy.max_pool_bytes)?;
                Some(PoolReader::new(pool, control.0.clone()))
            }
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
            _abandon: Abandon {
                local: local.clone(),
            },
            reader: TypedReader::new(reader, pool),
            control,
            #[cfg(feature = "tokio")]
            reactor: None,
            local,
            spin: SpinGauge::default(),
            looked: Looked::Exact,
            received: 0,
            decode_errors: 0,
            #[cfg(feature = "tokio")]
            leftover: Arc::default(),
            _token: token,
            _death: death,
            dead,
            read_end: false,
            #[cfg(test)]
            before_seen: None,
            #[cfg(test)]
            about_to_park: None,
            #[cfg(all(test, feature = "tokio"))]
            trace: Trace::default(),
            _trust: PhantomData,
        })
    }
}

impl<T: Payload, M: Trust> Subscriber<T, M> {
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
    ///
    /// A lent value ([`Publisher::loan_slice`](crate::Publisher::loan_slice)) that the publisher
    /// took back before this call pinned its slot was replaced by a newer value, which the call
    /// goes on to, as to any newer record. One call goes past at most four such values and
    /// reports a fifth as lost, [`RecvError::Lagged`]`(1)`, so a publisher that keeps publishing
    /// holds no call. A call that finds no newer record after such a value reports it lost as well.
    pub fn try_latest(&mut self) -> Result<T, RecvError> {
        self.look(true)
    }

    /// The next value, waiting for one.
    ///
    /// Returns what [`try_recv`](Self::try_recv) does once a record is unread, or
    /// [`RecvError::Cancelled`] when an [`Interrupter`] ends the wait. After the publisher's
    /// process died, it returns the records committed before the death and then
    /// [`Closed::PublisherDied`](crate::Closed::PublisherDied) without waiting.
    ///
    /// Before it sleeps the wait looks at the ring, and for a while watches the publisher's
    /// committed position, so a record committed meanwhile is returned without a sleep. A sleep
    /// ends at the wake the publisher sends after a commit and the bump of the topic's `seq` that
    /// announces it (a bump before the sleep keeps it from sleeping, a bump alone ends none), at
    /// an event of this process (an interrupt, the publisher's death notice, an eviction), or
    /// when the futex wait returns early, as at a signal (`EINTR`); the wait then looks again. A
    /// publisher that breaks the protocol and commits a record with no wake after it may leave a
    /// wait without a bound asleep: the next receive returns that record, as each looks before it
    /// waits, and [`recv_timeout`](Self::recv_timeout) returns [`RecvError::Timeout`] at its bound.
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
    /// it waits on the runtime's reactor (`tokio::io::unix::AsyncFd`) and holds no thread. That
    /// reactor is the one of the runtime the first such call ran on, and later calls wait on it
    /// from any runtime: a `current_thread` runtime that no longer runs `block_on` wakes none of
    /// them until it is dropped; a wait whose reactor's runtime is dropped registers again with
    /// the runtime it runs on.
    ///
    /// Cancel-safe: dropping the future loses no record. A wait the pool has not started is taken
    /// back at the drop, and one it has started is woken, so the next receive waits only for that
    /// thread to leave it. A future leaked rather than dropped leaves its wait to the pool: the
    /// next receive that may wait returns a record already unread, and else joins that wait
    /// before it waits itself. The join lasts until a pool thread has run that wait, which ends
    /// as any wait does, at a commit, an interrupt or a death; while it is still queued nothing
    /// ends the join, however long the pool takes. A [`recv_timeout`](Self::recv_timeout) whose
    /// `timeout` passes first looks at the ring once more and does not wait. Besides `recv`'s
    /// results, [`RecvError::Cancelled`] when the pool cannot run the wait, or the reactor cannot
    /// take the subscription's file descriptors.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime, as `tokio::task::spawn_blocking` does, and with [`Wake::Fd`] in a
    /// runtime whose IO driver is off, as `AsyncFd::register` does.
    #[cfg(feature = "tokio")]
    pub async fn recv_async(&mut self) -> Result<T, RecvError> {
        self.wait_async_with(|subscriber| subscriber.look(false))
            .await
    }

    /// An async wait until `look` finds other than `Empty`; `look` holds no borrow.
    #[cfg(feature = "tokio")]
    async fn wait_async_with<V>(
        &mut self,
        mut look: impl FnMut(&mut Self) -> Result<V, RecvError>,
    ) -> Result<V, RecvError> {
        if self.local.words().fd.is_some() {
            return self.recv_on_reactor(look).await;
        }
        loop {
            if self.leftover.busy() {
                // Before the join, as `wait_with` looks; it leaves `ABANDON` to the wait still out.
                match look(self) {
                    Err(RecvError::Empty) => {}
                    other => return other,
                }
                self.leftover.join_async().await;
            }
            self.local.take(ABANDON);
            match look(self) {
                Err(RecvError::Empty) => {}
                other => return other,
            }
            if self.cancelled() {
                return Err(RecvError::Cancelled);
            }
            let seen = self.seen();
            if self.next(seen, ALL) == Next::Look {
                yield_once().await;
                continue;
            }
            #[cfg(test)]
            if let Some(hook) = self.about_to_park.as_mut() {
                hook();
            }
            let ran = on_pool(&self.local, &self.leftover, seen).await;
            if ran.is_err() {
                return Err(RecvError::Cancelled);
            }
        }
    }

    /// [`wait_async_with`](Self::wait_async_with) of a [`Wake::Fd`] subscription: on the reactor.
    #[cfg(feature = "tokio")]
    async fn recv_on_reactor<V>(
        &mut self,
        mut look: impl FnMut(&mut Self) -> Result<V, RecvError>,
    ) -> Result<V, RecvError> {
        loop {
            match look(self) {
                Err(RecvError::Empty) => {}
                other => return other,
            }
            if self.cancelled() {
                return Err(RecvError::Cancelled);
            }
            let seen = self.seen();
            if self.next(seen, REACTOR_ENDS) == Next::Look {
                yield_once().await;
                continue;
            }
            if self.reactor.is_none() {
                let fd = self.local.words().fd.as_ref().map(FdWake::fd);
                // SAFETY: `reactor` is declared before `local`, whose last `Arc` closes the fd.
                let made = fd.map(|fd| unsafe { Reactor::new(fd) }).transpose();
                self.reactor = made.map_err(|_| RecvError::Cancelled)?;
            }
            let Some(reactor) = &self.reactor else {
                return Err(RecvError::Cancelled);
            };
            #[cfg(test)]
            if let Some(hook) = self.about_to_park.as_mut() {
                hook();
            }
            // Bytes a drain left make no new edge, so the reactor would not report them.
            let unread = self.local.words().fd.as_ref().is_some_and(FdWake::unread);
            let armed = Armed(&self.local);
            if !self.local.arm_reactor(seen, unread) {
                drop(armed);
                yield_once().await;
                continue;
            }
            let woke = reactor.readable().await;
            drop(armed);
            if woke.is_err() {
                // A reactor that went away with its runtime: the wait goes on with the current one.
                self.reactor = None;
                continue;
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

    /// Whether the latest look gave up short of the publisher's `write` ([`Looked::GaveUp`]).
    pub(crate) fn gave_up(&self) -> bool {
        matches!(self.looked, Looked::GaveUp(_))
    }

    /// [`Reader::count_lost_before`](crate::ring::Reader::count_lost_before), for a drain that
    /// ends at the end it was [stopped at](Self::stop_at) after a look that gave up, or after
    /// receives that did not move it: the records it counted lost.
    pub(crate) fn count_lost_before(&mut self) -> u64 {
        // The reader may have moved: a wait looks again before it decides to sleep.
        self.looked = Looked::Exact;
        self.reader.count_lost_before()
    }

    /// [`Reader::stop_at`](crate::ring::Reader::stop_at), for a drain that ends at `end`.
    pub(crate) fn stop_at(&mut self, end: Option<u64>) {
        self.reader.stop_at(end);
    }

    /// Whether this subscription saw its publisher end for good: a look read the topic's end
    /// record, or the death notice of the publisher's binder ran. A `DIED` raised for a wake
    /// socket that hung up is neither, nor is a result that failed a check.
    pub(crate) fn saw_publisher_end(&self) -> bool {
        self.read_end || self.dead.get()
    }

    /// Clear [`NOTICE`], which a wait ends at and leaves up; whether it was up.
    pub(crate) fn take_notice(&self) -> bool {
        self.local.take(NOTICE)
    }

    /// The next record, or the newest with `latest`, without waiting.
    fn look(&mut self, latest: bool) -> Result<T, RecvError> {
        if self.local.evicted() {
            self.looked = Looked::Exact;
            return Err(RecvError::Closed(Closed::Evicted));
        }
        let before = self.before_look();
        let died = self.local.died();
        let received = match (M::TRUSTED, latest) {
            // SAFETY: `Trusted`, which only `trust_publisher` makes, on its caller's word.
            (true, latest) => unsafe { self.reader.recv_vouched(latest) },
            (false, true) => self.reader.try_latest(),
            (false, false) => self.reader.try_recv(),
        };
        let read = self.reader.position();
        self.settle(received, died, read, before)
    }

    /// `seq` before a look, loaded only after one that gave up: [`decide`] needs it then alone.
    fn before_look(&self) -> Option<u32> {
        match self.looked {
            Looked::GaveUp(_) => Some(self.local.words().seq()),
            Looked::Exact => None,
        }
    }

    /// A receive that waits until `deadline`, or without a bound.
    pub(crate) fn wait(&mut self, deadline: Option<Instant>, latest: bool) -> Result<T, RecvError> {
        self.wait_with(deadline, |subscriber| subscriber.look(latest))
    }

    /// Wait until `look` finds other than `Empty`, or `deadline` passes; `look` holds no borrow.
    fn wait_with<V>(
        &mut self,
        deadline: Option<Instant>,
        mut look: impl FnMut(&mut Self) -> Result<V, RecvError>,
    ) -> Result<V, RecvError> {
        #[cfg(feature = "tokio")]
        {
            #[cfg(test)]
            let joining = Instant::now();
            // Before this wait arms: a wait left on the pool would take the next wake.
            let joined = !self.leftover.busy()
                || match look(self) {
                    // A look arms nothing: a record already unread waits for no turn of the pool.
                    Err(RecvError::Empty) => self.leftover.join(deadline),
                    other => return other,
                };
            #[cfg(test)]
            {
                self.trace = Trace {
                    join: joining.elapsed(),
                    parks: Vec::new(),
                };
            }
            if !joined {
                // One look, which arms nothing: the wait still out keeps `WAITING` and `ABANDON`.
                return match look(self) {
                    Err(RecvError::Empty) => Err(RecvError::Timeout),
                    other => other,
                };
            }
            self.local.take(ABANDON);
        }
        // After a wake, look without spinning: the publisher has just committed or a bit is up.
        let mut woke = false;
        loop {
            match look(self) {
                Err(RecvError::Empty) => {}
                other => return other,
            }
            if self.cancelled() {
                return Err(RecvError::Cancelled);
            }
            // Stale: past the join no wait of this subscriber is out. Left up, it ends every park.
            self.local.take(ABANDON);
            // At every turn: a look lapped each time finds nothing while `pending` holds.
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(RecvError::Timeout);
            }
            if !woke && self.spun(deadline) {
                // Once: on a ring lapped at every look `pending` holds, so every spin ends at once.
                woke = true;
                continue;
            }
            let seen = self.seen();
            if self.next(seen, ALL) == Next::Look {
                woke = true;
                continue;
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

    /// `seq`, as a wait loads it to decide on and sleep on.
    fn seen(&mut self) -> u32 {
        #[cfg(test)]
        if let Some(hook) = self.before_seen.as_mut() {
            hook();
        }
        self.local.words().seq()
    }

    /// [`decide`] on the latest look, for a wait that ends at or takes the bits in `honors`.
    fn next(&self, seen: u32, honors: u32) -> Next {
        decide(self.looked, seen, self.local.stop_bits(), honors, || {
            self.reader.pending()
        })
    }

    /// [`Local::cancelled`]: at an interrupt, which it takes, or at [`NOTICE`].
    fn cancelled(&self) -> bool {
        self.local.cancelled()
    }

    /// Report `read` and the losses, record what the look found, and count what `received` is.
    fn settle<V>(
        &mut self,
        received: Result<V, RecvError>,
        died: bool,
        read: u64,
        before: Option<u32>,
    ) -> Result<V, RecvError> {
        let page = &self.control.0;
        // After the copy: a publisher that waits for this `read` writes over the record only then.
        page.read().store(read, Ordering::Release);
        page.lost().store(self.reader.lost(), Ordering::Relaxed);
        self.looked = match &received {
            Err(RecvError::Empty) if self.reader.gave_up() => Looked::GaveUp(before),
            _ => Looked::Exact,
        };
        match &received {
            Ok(_) => self.received += 1,
            Err(RecvError::Decode(_)) => self.decode_errors += 1,
            // Made of the end record alone (`not_a_value`).
            Err(RecvError::Closed(Closed::End | Closed::Status(_))) => self.read_end = true,
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
        self.reader.pool().map(PoolReader::pool)
    }

    /// This subscriber's waiting state, for tests that pause or watch a wait.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn local(&self) -> &Arc<Local<Parts>> {
        &self.local
    }

    /// The death recipient of this subscription, for tests that run it on a publisher of this
    /// process, which has none.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn death_notice(&self) -> Died {
        Died {
            local: self.local.clone(),
            dead: self.dead.clone(),
        }
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

impl Subscriber<Bytes> {
    /// This subscriber, vouching for its publisher from now on ([`Trusted`]): its samples deref
    /// to `[u8]` in place, and every copy of a lent value, a sample's or [`recv`](Self::recv)'s,
    /// is a plain memory copy into a buffer not zeroed first.
    ///
    /// The subscription goes on as it was: the same read position, waits and statistics. Values
    /// that came through the ring are copied as before, since the ring lets the publisher write
    /// over a record that is being read; only a slot the pin protects is read in place. There is
    /// no way back: a subscriber that stops vouching subscribes again.
    ///
    /// # Safety
    ///
    /// For as long as the subscription lives, the publisher behind the topic keeps the pin
    /// protocol of plan 15 §6.2: it never writes a slot a subscriber has pinned. An
    /// `rsbinder-topic` [`Publisher`](crate::Publisher) does. Nothing in this process can check
    /// which process published the topic, so the caller answers for it: a process that
    /// subscribes to its own system's service, say, and not one less privileged than itself. A
    /// publisher that breaks the protocol can change the bytes under a live `&[u8]`, which is
    /// undefined behavior.
    ///
    /// ```no_run
    /// use rsbinder_topic::{Bytes, Subscriber, SubscriberPolicy, Trusted};
    ///
    /// fn checksum(topic: &rsbinder::SIBinder) -> rsbinder::BinderResult<u32> {
    ///     let subscriber = Subscriber::<Bytes>::request(topic, &SubscriberPolicy::default())?;
    ///     // SAFETY: the topic is this system's own service, an rsbinder-topic Publisher.
    ///     let mut subscriber: Subscriber<Bytes, Trusted> = unsafe { subscriber.trust_publisher() };
    ///     let sample = subscriber.recv_sample().expect("a value");
    ///     Ok(sample.iter().map(|&byte| u32::from(byte)).sum())
    /// }
    /// ```
    pub unsafe fn trust_publisher(self) -> Subscriber<Bytes, Trusted> {
        let Self {
            _abandon,
            reader,
            control,
            #[cfg(feature = "tokio")]
            reactor,
            local,
            spin,
            looked,
            received,
            decode_errors,
            #[cfg(feature = "tokio")]
            leftover,
            _token,
            _death,
            dead,
            read_end,
            #[cfg(test)]
            before_seen,
            #[cfg(test)]
            about_to_park,
            #[cfg(all(test, feature = "tokio"))]
            trace,
            _trust,
        } = self;
        Subscriber {
            _abandon,
            reader,
            control,
            #[cfg(feature = "tokio")]
            reactor,
            local,
            spin,
            looked,
            received,
            decode_errors,
            #[cfg(feature = "tokio")]
            leftover,
            _token,
            _death,
            dead,
            read_end,
            #[cfg(test)]
            before_seen,
            #[cfg(test)]
            about_to_park,
            #[cfg(all(test, feature = "tokio"))]
            trace,
            _trust: PhantomData,
        }
    }
}

impl<M: Trust> Subscriber<Bytes, M> {
    /// The next value where it lies, without waiting: [`try_recv`](Self::try_recv) with no copy
    /// of a value the publisher lent ([`Publisher::loan_slice`](crate::Publisher::loan_slice)).
    ///
    /// The [`Sample`] pins the value's slot until it is dropped, and borrows the subscriber, so
    /// nothing else is received meanwhile. Until then the subscriber has not read the value as a
    /// publisher with [`Backpressure`](crate::Backpressure) counts it: a publish that waits for
    /// this subscriber waits for the drop. A value that came through the ring is returned in
    /// the subscriber's buffer. The errors are `try_recv`'s; a lent slot the publisher took back
    /// before this call pinned it is a loss of one record, [`RecvError::Lagged`]`(1)`.
    pub fn try_recv_sample(&mut self) -> Result<Sample<'_, M>, RecvError> {
        let held = self.look_held(false)?;
        Ok(self.reader.sample(held, self.control.0.read()))
    }

    /// The newest value where it lies, without waiting: [`try_latest`](Self::try_latest) with no
    /// copy of a value the publisher lent, as [`try_recv_sample`](Self::try_recv_sample) is.
    ///
    /// The records before the newest are skipped and not reported as lost, lent values among them
    /// too: their slots are not pinned. Until the [`Sample`] drops the subscriber has read up to
    /// the start of the newest record, so a publisher with
    /// [`Backpressure`](crate::Backpressure) waits for that record alone, not for those skipped. A
    /// lent value taken back before this call pinned its slot is gone past as `try_latest` goes
    /// past one, and the errors are `try_latest`'s.
    pub fn try_latest_sample(&mut self) -> Result<Sample<'_, M>, RecvError> {
        let held = self.look_held(true)?;
        Ok(self.reader.sample(held, self.control.0.read()))
    }

    /// The next value where it lies, waiting for one: [`try_recv_sample`](Self::try_recv_sample)
    /// after the wait [`recv`](Self::recv) makes.
    pub fn recv_sample(&mut self) -> Result<Sample<'_, M>, RecvError> {
        let held = self.wait_with(None, |subscriber| subscriber.look_held(false))?;
        Ok(self.reader.sample(held, self.control.0.read()))
    }

    /// [`recv_sample`](Self::recv_sample) with a bound: [`RecvError::Timeout`] once `timeout`
    /// passes with no record unread.
    pub fn recv_sample_timeout(&mut self, timeout: Duration) -> Result<Sample<'_, M>, RecvError> {
        let deadline = Instant::now().checked_add(timeout);
        let held = self.wait_with(deadline, |subscriber| subscriber.look_held(false))?;
        Ok(self.reader.sample(held, self.control.0.read()))
    }

    /// [`recv_sample`](Self::recv_sample) for an async caller: the value where it lies, waiting
    /// for one as [`recv_async`](Self::recv_async) waits, on the blocking pool or the reactor.
    ///
    /// Cancel-safe, as `recv_async` is: a slot is pinned only by the look that finds its value,
    /// and the future returns the [`Sample`] in that same poll, so a future dropped while it
    /// waits holds no pin and loses no record. The results are `recv_sample`'s, and `recv_async`'s
    /// [`RecvError::Cancelled`].
    ///
    /// # Panics
    ///
    /// As `recv_async` does: outside a Tokio runtime, and with [`Wake::Fd`] in a runtime whose IO
    /// driver is off.
    #[cfg(feature = "tokio")]
    pub async fn recv_sample_async(&mut self) -> Result<Sample<'_, M>, RecvError> {
        let held = self
            .wait_async_with(|subscriber| subscriber.look_held(false))
            .await?;
        Ok(self.reader.sample(held, self.control.0.read()))
    }

    fn look_held(&mut self, latest: bool) -> Result<Held, RecvError> {
        if self.local.evicted() {
            self.looked = Looked::Exact;
            return Err(RecvError::Closed(Closed::Evicted));
        }
        let before = self.before_look();
        let died = self.local.died();
        let held = self.reader.take_held(latest);
        // A pinned slot is still being read: its record counts as read once the sample drops.
        let read = match held {
            Ok(Held::Slot { at, .. }) => at,
            _ => self.reader.position(),
        };
        self.settle(held, died, read, before)
    }
}

impl<T: Payload, M: Trust> poll::Sealed for Subscriber<T, M> {}

impl<T: Payload, M: Trust> poll::Wait for Subscriber<T, M> {
    fn wait_fd(&self) -> Option<BorrowedFd<'_>> {
        self.local.words().fd.as_ref().map(FdWake::fd)
    }

    fn prepare_wait(&mut self) -> bool {
        assert!(
            self.local.words().fd.is_some(),
            "prepare_wait: a subscription made with `Wake::Futex` has no file descriptor to poll"
        );
        let seen = self.seen();
        let (looked, local, reader) = (self.looked, &self.local, &self.reader);
        #[cfg(test)]
        let hook = &mut self.about_to_park;
        let next = |ends| decide(looked, seen, local.stop_bits(), ends, || reader.pending());
        let unread = || {
            #[cfg(test)]
            if let Some(hook) = hook.as_mut() {
                hook();
            }
            // Bytes a drain left: an edge-triggered poll would not report them, so `disarm` drains.
            local.words().fd.as_ref().is_some_and(FdWake::unread)
        };
        local.prepare_poll(seen, next, unread)
    }

    fn finish_wait(&mut self) {
        assert!(
            self.local.words().fd.is_some(),
            "finish_wait: a subscription made with `Wake::Futex` has no file descriptor to poll"
        );
        self.local.disarm();
    }
}

impl<T: Payload, M: Trust> fmt::Debug for Subscriber<T, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscriber")
            .field("type", &T::type_name())
            .field("trusted", &M::TRUSTED)
            .finish_non_exhaustive()
    }
}

/// Raises `ABANDON` when the subscriber drops: a wait left on the pool lets go of the memory.
struct Abandon {
    local: Arc<Local<Parts>>,
}

impl Drop for Abandon {
    fn drop(&mut self) {
        self.local.raise(ABANDON);
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
    /// # Safety: `fd` stays open, as the same file description, until the `Reactor` drops.
    unsafe fn new(fd: BorrowedFd<'_>) -> std::io::Result<Self> {
        let unowned = Unowned(fd.as_raw_fd());
        // SAFETY: the caller keeps `fd` open, as the same file description, while `Self` lives.
        let registered = unsafe { AsyncFd::register_with_interest(unowned, Interest::READABLE) };
        Ok(Self(registered?))
    }

    /// Until the fd is readable; its readiness is cleared: a drain that leaves bytes sets `unread`.
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

/// One `Pending`, woken at once: lets the runtime run timers, so a `timeout` around it can fire.
#[cfg(feature = "tokio")]
async fn yield_once() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            return Poll::Ready(());
        }
        yielded = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await
}

/// Ends a [`Subscriber`]'s wait from another thread; made by
/// [`Subscriber::interrupter`].
///
/// A receive that waits returns [`RecvError::Cancelled`]: the one waiting when
/// [`interrupt`](Self::interrupt) is called, or else the next one to wait. A receive that finds a
/// record does not wait and does not consume the interrupt. With [`Wake::Fd`] a poll
/// ([`Wait`](crate::Wait)) returns for it too and leaves it to that receive: the next
/// [`prepare_wait`](crate::Wait::prepare_wait) before such a receive returns `false`, and the
/// interrupt's wake ends the poll in progress or, landing after that poll drained the fd, a later
/// one at once. One interrupt makes at most two calls of [`wait_any`](crate::wait_any) return,
/// three for a `NamedSubscriber`, as `wait_any` counts. The subscription goes on either way.
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
    /// at once when none sleeps; does nothing after the subscriber has been dropped. With
    /// [`Wake::Fd`] it returns, while a wait sleeps or polls, once a wake is queued on the socket
    /// or the socket is found shut down, which the wait reads as an end of file, and the wait may
    /// still be polling; or once the wait is out, or a `prepare_wait` returned `false` for the
    /// interrupt. With none, it sends nothing: the next wait sees the interrupt as it starts.
    pub fn interrupt(&self) {
        match &self.target {
            Target::Subscriber(local) => {
                if let Some(local) = local.upgrade() {
                    local.raise(INTERRUPTED);
                }
            }
            Target::Named(named) => {
                if let Some(named) = named.upgrade() {
                    named.interrupt();
                }
            }
        }
    }

    /// End the wait in progress, or the next one, for the name layer of a `NamedSubscriber` to
    /// look at its state, as another publisher taking the name or an interrupt makes it: with
    /// [`NOTICE`], a bit apart from the caller's interrupts, which the name layer clears. Does
    /// nothing to a `NamedSubscriber`'s own.
    pub(crate) fn notice(&self) {
        if let Target::Subscriber(local) = &self.target {
            if let Some(local) = local.upgrade() {
                local.raise(NOTICE);
            }
        }
    }

    /// `interrupt`, and the wakes it tried, those that failed included.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn interrupt_counting(&self) -> usize {
        match &self.target {
            Target::Subscriber(local) => {
                local.upgrade().map_or(0, |local| local.raise(INTERRUPTED))
            }
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
