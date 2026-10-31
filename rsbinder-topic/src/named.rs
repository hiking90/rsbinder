// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The name layer (plan 15 §5.7c): a subscriber of a name, connected to whichever publisher
//! holds it.
//!
//! A [`NamedSubscriber`] watches its name through a [`Resolver`]. The resolver's callback only
//! records the binder and starts a connection thread; the thread makes the subscription call
//! (`Subscriber::request`) and hands the connection over. The receiving thread never makes the
//! call, so a publisher that does not answer holds no receive, and dropping the subscriber does
//! not wait for the thread: a connection the thread makes after the drop is dropped by it.
//!
//! A notice of the watch can be lost (plan 15 §12.5): the thread also asks the watch's lookup,
//! once when a publisher the lookup did not name refuses as ended and once when a connection's
//! publisher ends with no binder after it, and takes the answer only when no notice came while
//! it asked; a notice of a binder it cannot use makes it ask once more.

use std::collections::VecDeque;
use std::fmt;
use std::os::fd::BorrowedFd;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use rsbinder::{BinderResult, ExceptionCode, SIBinder, Status, StatusCode};

use crate::aidl::ITopic::{ERROR_CLOSED, ERROR_VERSION};
use crate::connect::status_of;
use crate::layout;
use crate::poll::{Sealed, Wait};
use crate::resolve::{Lookup, Resolver, ServiceManager, Watch};
use crate::subscriber::Interrupt;
use crate::wake::NamedFds;
use crate::{
    Closed, Disconnected, Interrupter, Payload, RecvError, Start, Subscriber, SubscriberPolicy,
    SubscriberStats, Wake,
};

/// The first wait before a refused subscription is asked again, doubled up to [`LAST_RETRY`].
pub(crate) const FIRST_RETRY: Duration = Duration::from_secs(1);
pub(crate) const LAST_RETRY: Duration = Duration::from_secs(30);

/// Looks of a drain that may give up lapped before it ends: the most one of its receives makes.
const DRAIN_GIVE_UPS: u32 = 2;
/// Drain receives in a row that may leave it where it was before it ends; a conforming one, none.
const DRAIN_STALLS: u32 = 2;

/// A subscriber of a topic name: it connects to the publisher the name is given to, and to the
/// next one when that one ends or another takes the name.
///
/// [`subscribe`](Self::subscribe) watches the name and returns at once, with or without a
/// publisher. Each connection is a [`Subscriber`] made by a thread of its own, so receiving
/// never makes a binder call. Between connections a receive waits for the next one;
/// [`wait_connected`](Self::wait_connected) waits for one without receiving.
///
/// A receive returns what a [`Subscriber`]'s does, except that the end of a connection is
/// [`RecvError::Disconnected`], returned once, after which the next receive goes on with the next
/// connection. Two of them leave no next connection while the name stays where it is: a refusal a
/// retry cannot change ([`Disconnected::Rejected`] says which) and a corrupt ring
/// ([`Disconnected::Ended`] with [`Closed::Corrupted`]); the receives after them wait until the
/// name is registered anew, so a loop logs the `Disconnected` it goes on past, as the example
/// below does. The one [`RecvError::Closed`] a `NamedSubscriber` returns is
/// [`Closed::Evicted`]: a publisher with backpressure took the subscription out, and since a
/// subscriber as slow as before would be taken out again, no connection is made after it, to that
/// publisher or the next; it is returned from then on. Code that only wants the values goes on
/// past the rest:
///
/// ```no_run
/// use rsbinder::ProcessState;
/// use rsbinder_topic::{Bytes, NamedSubscriber, RecvError, SubscriberPolicy};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// ProcessState::init_default()?;
/// ProcessState::start_thread_pool();
/// let policy = SubscriberPolicy::default();
/// let mut subscriber = NamedSubscriber::<Bytes>::subscribe("example.topic", &policy)?;
/// loop {
///     match subscriber.recv() {
///         Ok(Bytes(value)) => println!("{value:?}"),
///         Err(RecvError::Lagged(_) | RecvError::Decode(_)) => continue,
///         Err(RecvError::Disconnected(why)) => eprintln!("example.topic: {why:?}"),
///         Err(_) => break,
///     }
/// }
/// # Ok(()) }
/// ```
///
/// Records do not carry over from one connection to the next: the index starts again, and what
/// was published between the two is not counted as lost (the `Disconnected` marks the gap). The
/// first connection starts where the policy's [`start`](SubscriberPolicy::start) says. A later
/// connection to a publisher this subscriber was not connected to before starts at the oldest
/// record of the new ring, unless that `start` is [`Start::Last`]: the subscription is older than
/// that publisher, so all it published is news to it. A connection to a publisher it was
/// connected to before, as when the name comes back to that publisher after another held it,
/// starts at [`Start::Latest`], so that no record the earlier connection returned is returned
/// again. A connection counts from the receive that takes it; one whose fd the subscriber's
/// epoll instance refuses ([`Disconnected::Rejected`]) does not.
///
/// The subscriber remembers a publisher it was connected to by a reference to its binder, and
/// forgets it only on what a connection to it saw: the connection read the end record of its
/// topic (what [`Closed::End`] and [`Closed::Status`] report), after which the publisher refuses
/// every later subscription, or the notice of its binder's death arrived while the connection
/// was held. How the connection ended does not decide it, so a publisher no connection saw end
/// stays remembered, as it may take the name again, until a later connection to it sees its end
/// or the subscriber drops. A connection sees the records its receives read, those of the drain
/// before a [`Disconnected::Replaced`] included, and the death notice that arrives while it is
/// held; an end it read no record of, or a notice that arrives once it is over, goes to none.
///
/// A notice of the name is a oneway call, which fails when this process has no room left for
/// one, so a subscriber can miss the binder a name was given. With a watch that has a lookup
/// ([`Watch::with_lookup`]; the service manager's has one), the subscriber asks it for the
/// binder the name has now, once each time it would otherwise wait for a notice that may not
/// come: when a publisher refuses it as ended ([`ITopic::ERROR_CLOSED`] or `DeadObject`),
/// unless that publisher was the lookup's own answer, and when the publisher of its connection
/// ends and the name has been given no other binder. The answer is used only when no notice
/// came while it asked, and not when it is the binder that ended or refused; a notice that came
/// meanwhile is used instead, and one of a binder that ended or refused makes it ask once more.
/// Notices of one name are not ordered against the answer: with the service manager, a notice
/// older than the answer that arrives after it moves the subscriber back to that binder, and
/// from there, as it refuses or ends, to the binder the name has, through a
/// [`Disconnected::Replaced`] and a reconnection.
///
/// A subscriber with no connection and no subscription call under way, as while the name has
/// no publisher or after a lookup found none it could use, learns of a binder only by a notice:
/// one lost then is not made up, and the subscriber waits for the next registration of the
/// name. [`wait_connected`](Self::wait_connected) tells that state; making the subscriber
/// again watches the name again, and the watch's first notice is the binder the name has.
///
/// With [`Wake::Fd`] the subscriber is a [`Wait`]: an event loop or
/// [`wait_any`](crate::wait_any) polls the same fd through every reconnection.
/// `recv_async` waits on the connection's `recv_async`, and between connections without a
/// thread.
///
/// **The process has to run the binder thread pool**
/// (`rsbinder::ProcessState::start_thread_pool`): the service manager's notice of the name is a
/// call into it, and without the pool the subscriber never connects.
///
/// [`ITopic::ERROR_CLOSED`]: crate::aidl::ITopic::ERROR_CLOSED
#[must_use = "dropping the subscriber at once ends the subscription at once"]
pub struct NamedSubscriber<T: Payload> {
    named: Arc<Named<T>>,
    current: Option<Connection<T>>,
    /// What the connections that ended counted.
    past: SubscriberStats,
    _watch: Watch,
}

/// The connection the receiving thread holds.
struct Connection<T: Payload> {
    subscriber: Subscriber<T>,
    binder: SIBinder,
    /// The records to return before the end, once another publisher took the name.
    drain: Option<Drain>,
}

impl<T: Payload> Connection<T> {
    /// The name came back to this connection's publisher: nothing replaced, no drain, read live.
    fn keep(&mut self, state: &mut State<T>) {
        state.replaced = false;
        self.drain = None;
        self.subscriber.stop_at(None);
    }
}

/// A drain of the records a replaced publisher had committed when the replacement was seen.
#[derive(Debug, PartialEq)]
struct Drain {
    /// Where those records end: the reader is [stopped at](Subscriber::stop_at) it.
    end: u64,
    /// Looks of this drain that gave up, up to [`DRAIN_GIVE_UPS`].
    gave_up: u32,
    /// The furthest this drain has read to.
    reach: u64,
    /// Receives in a row that did not read past `reach`, up to [`DRAIN_STALLS`].
    stalls: u32,
    /// The losses before `end` were counted and returned: the next receive ends the drain.
    settled: bool,
}

/// What a drain does after one of its looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterLook {
    /// Look once more within this receive.
    Again,
    /// Count what is left before the end as lost, and end the drain.
    Settle,
    /// Return what the look found.
    Return,
}

impl Drain {
    /// A drain to `end` of a reader at `position`.
    fn new(end: u64, position: u64) -> Self {
        Self {
            end,
            gave_up: 0,
            reach: position,
            stalls: 0,
            settled: false,
        }
    }

    /// After a look that ended at `position`; `gave_up` is the subscriber's, as its waits see it.
    fn after_look(&mut self, gave_up: bool, position: u64) -> AfterLook {
        if !gave_up {
            return AfterLook::Return;
        }
        self.gave_up += 1;
        // Only `try_latest`, which does not look at `end`, gives up at it or past it: drained.
        if position >= self.end || self.gave_up >= DRAIN_GIVE_UPS {
            AfterLook::Settle
        } else {
            AfterLook::Again
        }
    }

    /// Count a receive that returned a result and left the reader at `position`.
    fn count_stall(&mut self, position: u64) {
        if position > self.reach {
            self.reach = position;
            self.stalls = 0;
        } else {
            self.stalls += 1;
        }
    }
}

/// What the subscriber, the resolver's callback and the connection thread share.
struct Named<T: Payload> {
    policy: SubscriberPolicy,
    state: Mutex<State<T>>,
    /// Each change of `state` is signalled to every kind of wait: a thread's, a future's, a poll's.
    changed: Condvar,
    #[cfg(feature = "tokio")]
    notify: tokio::sync::Notify,
    /// With [`Wake::Fd`]: the fds handed out, which hold the connection's.
    fds: Option<NamedFds>,
}

struct State<T: Payload> {
    dropped: bool,
    /// A publisher took the subscription out (plan 15 §6.5): no connection is made again.
    evicted: bool,
    /// An interrupt no receive has returned, the only one kept: the connection gets a `NOTICE`.
    interrupted: bool,
    /// That interrupt has not ended a poll yet; a poll takes it, a receive with `interrupted`.
    kicked: bool,
    /// The binder the name was last given.
    latest: Option<SIBinder>,
    /// Notices `registered` took, each one counted though it repeats `latest`.
    registrations: u64,
    /// A connection the thread made that the subscriber has not taken yet.
    ready: Option<(Subscriber<T>, SIBinder)>,
    /// The binder of the connection in `ready` or the subscriber's hands.
    connected: Option<SIBinder>,
    /// A binder no subscription call goes to again: the refusal would not change.
    refused: Option<SIBinder>,
    connecting: bool,
    /// The name went to another binder while connected.
    replaced: bool,
    notices: VecDeque<Disconnected>,
    /// Ends the wait of the subscriber's connection when the state changes.
    wake: Option<Interrupter>,
    /// Binders joined, each once; one leaves when its connection saw the topic's end or its death.
    joined: Vec<SIBinder>,
    /// A connection has joined: a later one to a binder not joined starts at `Oldest`, or `Last`.
    later: bool,
    /// The wait before asking again a publisher whose connection failed to join; zero after a join.
    rejoin: Duration,
    find: Find,
}

/// The watch's lookup, as far as the subscriber knows it.
enum Find {
    /// The watch has not been returned yet; `true` once a refusal wanted its lookup.
    Unknown(bool),
    Lookup(Lookup),
    Absent,
}

/// Where a connection thread starts.
enum Begin {
    /// Subscribe to this binder.
    At(SIBinder),
    /// Ask the watch's lookup first; the binder whose end led here, if one did.
    Look(Option<SIBinder>),
}

impl<T: Payload> State<T> {
    fn new() -> Self {
        Self {
            dropped: false,
            evicted: false,
            interrupted: false,
            kicked: false,
            latest: None,
            registrations: 0,
            ready: None,
            connected: None,
            refused: None,
            connecting: false,
            replaced: false,
            notices: VecDeque::new(),
            wake: None,
            joined: Vec::new(),
            later: false,
            rejoin: Duration::ZERO,
            find: Find::Unknown(false),
        }
    }

    /// Take a pending interrupt for a receive to return, its kick with it; whether one was.
    fn take_interrupt(&mut self) -> bool {
        self.kicked = false;
        std::mem::take(&mut self.interrupted)
    }
}

/// What a refused subscription call leads to.
enum Refusal {
    /// The same publisher would refuse again: wait for the name to go elsewhere.
    Final { tell: bool },
    /// Ask the same publisher again later.
    Retry,
}

fn refusal(status: &Status) -> Refusal {
    match status.exception_code() {
        ExceptionCode::ServiceSpecific => match status.service_specific_error() {
            ERROR_VERSION => Refusal::Final { tell: true },
            // A dropped or closed publisher whose name stays registered.
            ERROR_CLOSED => Refusal::Final { tell: false },
            _ => Refusal::Retry,
        },
        ExceptionCode::TransactionFailed => match status.transaction_error() {
            StatusCode::BadType | StatusCode::InvalidOperation => Refusal::Final { tell: true },
            // The publisher's process is gone; its successor registers the name anew.
            StatusCode::DeadObject => Refusal::Final { tell: false },
            _ => Refusal::Retry,
        },
        ExceptionCode::Security | ExceptionCode::UnsupportedOperation => {
            Refusal::Final { tell: true }
        }
        _ => Refusal::Retry,
    }
}

impl<T: Payload + 'static> NamedSubscriber<T> {
    /// Subscribe to the topic the service manager gives `name` to, now or later.
    ///
    /// Returns once the name is watched (`rsbinder::hub::register_for_notifications`), without
    /// waiting for a publisher. Fails when the watch does: a name the service manager refuses,
    /// or Android 10's service manager, which has no registration notices. `policy` is used for
    /// every connection, but for where each connection after the first starts (see the type's
    /// documentation).
    pub fn subscribe(name: &str, policy: &SubscriberPolicy) -> BinderResult<Self> {
        Self::subscribe_with(name, policy, &ServiceManager)
    }

    /// [`subscribe`](Self::subscribe) through `resolver` instead of the service manager.
    pub fn subscribe_with(
        name: &str,
        policy: &SubscriberPolicy,
        resolver: &dyn Resolver,
    ) -> BinderResult<Self> {
        layout::check_target().map_err(status_of)?;
        let fds = match policy.wake {
            Wake::Futex => None,
            Wake::Fd => Some(NamedFds::new().map_err(StatusCode::from)?),
        };
        let named = Arc::new(Named {
            policy: policy.clone(),
            state: Mutex::new(State::new()),
            changed: Condvar::new(),
            #[cfg(feature = "tokio")]
            notify: tokio::sync::Notify::new(),
            fds,
        });
        let weak = Arc::downgrade(&named);
        let watch = resolver.watch(
            name,
            Box::new(move |binder| {
                if let Some(named) = weak.upgrade() {
                    named.registered(binder);
                }
            }),
        );
        let watch = match watch {
            Ok(watch) => watch,
            Err(status) => {
                // A call before the error may have started a connection thread: it ends on this.
                named.lock().dropped = true;
                named.signal();
                return Err(status);
            }
        };
        let mut state = named.lock();
        let wanted = matches!(state.find, Find::Unknown(true));
        state.find = match watch.lookup() {
            Some(lookup) => Find::Lookup(lookup),
            None => Find::Absent,
        };
        // A refusal before the watch was returned wanted the lookup it did not have yet.
        let look = wanted
            && matches!(state.find, Find::Lookup(_))
            && !state.connecting
            && state.connected.is_none();
        state.connecting |= look;
        drop(state);
        if look {
            named.connect(Begin::Look(None), Duration::ZERO);
        }
        Ok(Self {
            named,
            current: None,
            past: SubscriberStats::default(),
            _watch: watch,
        })
    }

    /// What this subscriber has received over all its connections since it was made (plan 15
    /// §5.9a); see [`SubscriberStats`] for what that counts and what it does not.
    pub fn stats(&self) -> SubscriberStats {
        let mut stats = self.past;
        if let Some(connection) = &self.current {
            stats += connection.subscriber.stats();
        }
        stats
    }

    /// The next value, without waiting: [`Subscriber::try_recv`] on the connection.
    /// [`RecvError::Empty`] also when there is none.
    pub fn try_recv(&mut self) -> Result<T, RecvError> {
        self.receive(None, false)
    }

    /// The newest value, without waiting: [`Subscriber::try_latest`] on the connection.
    pub fn try_latest(&mut self) -> Result<T, RecvError> {
        self.receive(None, true)
    }

    /// The next value, waiting for one, and for a connection when there is none.
    /// [`RecvError::Cancelled`] when an [`Interrupter`] ends the wait.
    pub fn recv(&mut self) -> Result<T, RecvError> {
        self.receive(Some(None), false)
    }

    /// [`recv`](Self::recv) with a bound: [`RecvError::Timeout`] once `timeout` passes with no
    /// record. A `timeout` too long for an `Instant` waits without one.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<T, RecvError> {
        self.receive(Some(Instant::now().checked_add(timeout)), false)
    }

    /// The newest value, waiting until some record is unread, as [`Subscriber::recv_latest`].
    ///
    /// After another publisher took the name, it may return one record the previous publisher
    /// committed after that was seen, the newest when it looked, and then
    /// [`Disconnected::Replaced`].
    pub fn recv_latest(&mut self) -> Result<T, RecvError> {
        self.receive(Some(None), true)
    }

    /// A handle that ends this subscriber's wait from another thread, also a wait for a
    /// connection. The connection, or the search for one, goes on.
    pub fn interrupter(&self) -> Interrupter {
        Interrupter::named(Arc::downgrade(&self.named) as Weak<dyn Interrupt>)
    }

    /// Wait until the subscriber has a connection, or `timeout` passes: whether it has one. A
    /// connection made and not yet received from counts. A `timeout` too long for an `Instant`
    /// waits without one.
    pub fn wait_connected(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.named.lock();
        loop {
            if self.current.is_some() || state.connected.is_some() {
                return true;
            }
            state = match deadline {
                None => self.named.changed.wait(state).expect("lock poisoned"),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return false;
                    }
                    let waited = self.named.changed.wait_timeout(state, deadline - now);
                    waited.expect("lock poisoned").0
                }
            };
        }
    }

    /// A receive: without waiting (`wait` is `None`), or waiting until a deadline or without one.
    fn receive(&mut self, wait: Option<Option<Instant>>, latest: bool) -> Result<T, RecvError> {
        loop {
            let named = self.named.clone();
            let mut state = match self.step(named.lock(), latest) {
                Step::Done(received) => return received,
                Step::Idle(state) => state,
                Step::Connected => {
                    let connection = self.current.as_mut().expect("a connection");
                    let received = match wait {
                        None if latest => connection.subscriber.try_latest(),
                        None => connection.subscriber.try_recv(),
                        Some(deadline) => connection.subscriber.wait(deadline, latest),
                    };
                    match self.settle(received) {
                        Some(received) => return received,
                        None => continue,
                    }
                }
            };
            let Some(deadline) = wait else {
                return Err(RecvError::Empty);
            };
            if state.take_interrupt() {
                return Err(RecvError::Cancelled);
            }
            match deadline {
                None => drop(named.changed.wait(state).expect("lock poisoned")),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(RecvError::Timeout);
                    }
                    let waited = named.changed.wait_timeout(state, deadline - now);
                    drop(waited.expect("lock poisoned"));
                }
            }
        }
    }

    /// [`recv`](Self::recv) for an async caller: the connection's
    /// [`Subscriber::recv_async`], and between connections a wait that holds no thread.
    ///
    /// Cancel-safe, as `Subscriber::recv_async` is, and with its panics.
    #[cfg(feature = "tokio")]
    pub async fn recv_async(&mut self) -> Result<T, RecvError> {
        loop {
            let named = self.named.clone();
            // Made before the look: a `Notified` gets `notify_waiters` from its creation on.
            let notified = named.notify.notified();
            // Decided before any await: the lock, and the type that holds it, end here.
            let connected = match self.step(named.lock(), false) {
                Step::Done(received) => return received,
                Step::Idle(state) => {
                    if take_interrupt(state) {
                        return Err(RecvError::Cancelled);
                    }
                    false
                }
                Step::Connected => true,
            };
            if !connected {
                notified.await;
                continue;
            }
            let connection = self.current.as_mut().expect("a connection");
            let received = connection.subscriber.recv_async().await;
            if let Some(received) = self.settle(received) {
                return received;
            }
        }
    }

    /// A notice, a connection, or its next record before a replacement; `Idle` keeps the lock.
    fn step<'n>(&mut self, mut state: MutexGuard<'n, State<T>>, latest: bool) -> Step<'n, T> {
        if state.evicted {
            return Step::Done(Err(RecvError::Closed(Closed::Evicted)));
        }
        if let Some(notice) = state.notices.pop_front() {
            return Step::Done(Err(RecvError::Disconnected(notice)));
        }
        if self.current.is_none() {
            if let Some((subscriber, binder)) = state.ready.take() {
                if let Err(error) = self.join(&subscriber) {
                    state.connected = None;
                    state.replaced = false;
                    // Asked again as a transaction error is: a newer binder now, this one later.
                    let next = state.latest.clone().filter(|latest| {
                        Some(latest) != state.refused.as_ref() && !state.connecting
                    });
                    let after = match &next {
                        Some(next) if *next == binder => {
                            state.rejoin = (state.rejoin * 2).clamp(FIRST_RETRY, LAST_RETRY);
                            state.rejoin
                        }
                        _ => {
                            state.rejoin = Duration::ZERO;
                            Duration::ZERO
                        }
                    };
                    state.connecting |= next.is_some();
                    drop(state);
                    drop(subscriber);
                    if let Some(next) = next {
                        self.named.connect(Begin::At(next), after);
                    }
                    let status = Status::from(StatusCode::from(error));
                    return Step::Done(Err(RecvError::Disconnected(Disconnected::Rejected(
                        status,
                    ))));
                }
                state.rejoin = Duration::ZERO;
                state.later = true;
                if !state.joined.contains(&binder) {
                    state.joined.push(binder.clone());
                }
                let wake = subscriber.interrupter();
                if state.interrupted {
                    wake.notice();
                }
                state.wake = Some(wake);
                self.current = Some(Connection {
                    subscriber,
                    binder,
                    drain: None,
                });
            }
        }
        let Some(connection) = self.current.as_mut() else {
            return Step::Idle(state);
        };
        if state.replaced && state.latest.as_ref() == Some(&connection.binder) {
            // The name came back to this publisher: nothing to drain to; `settle` takes its wake.
            connection.keep(&mut state);
        }
        if state.replaced && connection.drain.is_none() {
            let end = connection.subscriber.committed();
            connection.subscriber.stop_at(Some(end));
            connection.drain = Some(Drain::new(end, connection.subscriber.position()));
        }
        drop(state);
        let Connection {
            subscriber, drain, ..
        } = connection;
        let Some(drain) = drain else {
            return Step::Connected;
        };
        if !drain.settled && subscriber.position() < drain.end {
            // Held in place, as only a publisher that breaks the protocol holds it: drained.
            let mut settle = drain.stalls == DRAIN_STALLS;
            let mut received = Err(RecvError::Empty);
            while !settle {
                received = if latest {
                    subscriber.try_latest()
                } else {
                    subscriber.try_recv()
                };
                // The give-up as a wait sees it: also when a death turned its `Empty` to `Closed`.
                match drain.after_look(subscriber.gave_up(), subscriber.position()) {
                    AfterLook::Return => break,
                    AfterLook::Settle => settle = true,
                    AfterLook::Again => {
                        #[cfg(test)]
                        BETWEEN_LOOKS.with_borrow_mut(|hook| hook.iter_mut().for_each(|h| h()));
                    }
                }
            }
            if settle {
                // `try_latest` skips what it does not return; `try_recv` counts it as lost.
                let lost = if latest {
                    0
                } else {
                    subscriber.count_lost_before()
                };
                if lost > 0 {
                    drain.settled = true;
                    return Step::Done(Err(RecvError::Lagged(lost)));
                }
                return self.end_drain();
            }
            match received {
                Err(RecvError::Closed(Closed::Evicted)) => return Step::Done(Err(self.evict())),
                Err(RecvError::Empty | RecvError::Closed(_)) => {}
                other => {
                    drain.count_stall(subscriber.position());
                    return Step::Done(other);
                }
            }
        }
        self.end_drain()
    }

    /// End the drain, unless the name came back to its publisher: decided in `finish`'s lock.
    fn end_drain<'n>(&mut self) -> Step<'n, T> {
        #[cfg(test)]
        BEFORE_DRAIN_ENDS.with_borrow_mut(|hook| hook.iter_mut().for_each(|hook| hook()));
        let named = self.named.clone();
        let mut state = named.lock();
        let connection = self.current.as_mut().expect("a connection");
        // `registered` for the connected binder starts no connection: kept here, or by none.
        if state.latest.as_ref() == Some(&connection.binder) {
            // The losses the drain counted stay counted: the live reader goes on past them.
            connection.keep(&mut state);
            return Step::Connected;
        }
        Step::Done(Err(self.finish_locked(state, Disconnected::Replaced)))
    }

    /// What a receive on the connection leads to: a result, or `None` to look again.
    fn settle(&mut self, received: Result<T, RecvError>) -> Option<Result<T, RecvError>> {
        match received {
            Err(RecvError::Cancelled) => {
                let state = &mut *self.named.lock();
                if state.take_interrupt() {
                    return Some(Err(RecvError::Cancelled));
                }
                let current = self.current.as_ref();
                let noticed = current.is_some_and(|c| c.subscriber.take_notice());
                // A notice ended the wait to be looked at; else the runtime could not run it.
                (!noticed && !state.replaced).then_some(Err(RecvError::Cancelled))
            }
            Err(RecvError::Closed(Closed::Evicted)) => Some(Err(self.evict())),
            Err(RecvError::Closed(closed)) => Some(Err(self.finish(Disconnected::Ended(closed)))),
            other => Some(other),
        }
    }

    /// Taken out: drop the connection and make none, as one as slow would be too (plan 15 §6.5).
    fn evict(&mut self) -> RecvError {
        let gone = self.current.take();
        if let Some(connection) = &gone {
            self.past += connection.subscriber.stats();
        }
        let mut state = self.named.lock();
        state.evicted = true;
        state.connected = None;
        state.wake = None;
        state.replaced = false;
        let ready = state.ready.take();
        drop(state);
        // A connection thread under way looks at `evicted` and ends.
        self.named.signal();
        if let (Some(fds), Some(connection)) = (&self.named.fds, &gone) {
            if let Some(theirs) = connection.subscriber.wait_fd() {
                fds.leave(theirs);
            }
        }
        // Unmapped outside the lock.
        drop(gone);
        drop(ready);
        RecvError::Closed(Closed::Evicted)
    }

    /// Put a connection's fd in the subscriber's epoll instance, with [`Wake::Fd`].
    fn join(&self, subscriber: &Subscriber<T>) -> std::io::Result<()> {
        match (&self.named.fds, subscriber.wait_fd()) {
            (Some(fds), Some(theirs)) => {
                #[cfg(test)]
                if NO_JOIN.get() {
                    return Err(std::io::ErrorKind::OutOfMemory.into());
                }
                fds.join(theirs)
            }
            _ => Ok(()),
        }
    }

    /// Drop the connection, look for the next publisher, and say why.
    fn finish(&mut self, why: Disconnected) -> RecvError {
        let named = self.named.clone();
        self.finish_locked(named.lock(), why)
    }

    /// [`finish`](Self::finish) under the lock in which the caller decided to.
    fn finish_locked(
        &mut self,
        mut state: MutexGuard<'_, State<T>>,
        why: Disconnected,
    ) -> RecvError {
        let gone = self.current.take();
        if let Some(connection) = &gone {
            self.past += connection.subscriber.stats();
        }
        state.connected = None;
        state.wake = None;
        state.replaced = false;
        let old = gone.as_ref().map(|connection| &connection.binder);
        // Not by `why`: an ended topic refuses every later subscription, a dead binder every call.
        let ended = gone.as_ref().filter(|c| c.subscriber.saw_publisher_end());
        let forgotten = ended.and_then(|connection| {
            let at = state.joined.iter().position(|j| *j == connection.binder);
            at.map(|at| state.joined.swap_remove(at))
        });
        let next = state
            .latest
            .clone()
            .filter(|latest| Some(latest) != old && Some(latest) != state.refused.as_ref());
        let begin = match next {
            _ if state.connecting => None,
            Some(next) => Some(Begin::At(next)),
            // The notice of the binder after the one that ended may have been lost.
            None if ended.is_some() && matches!(state.find, Find::Lookup(_)) => {
                Some(Begin::Look(old.cloned()))
            }
            None => None,
        };
        state.connecting |= begin.is_some();
        drop(state);
        if let Some(begin) = begin {
            #[cfg(test)]
            BEFORE_LOOKING.with_borrow_mut(|hook| hook.iter_mut().for_each(|hook| hook()));
            self.named.connect(begin, Duration::ZERO);
        }
        if let (Some(fds), Some(connection)) = (&self.named.fds, &gone) {
            if let Some(theirs) = connection.subscriber.wait_fd() {
                fds.leave(theirs);
            }
        }
        // Unmapped outside the lock.
        drop(gone);
        drop(forgotten);
        RecvError::Disconnected(why)
    }

    /// What ends the wait of the connection taken, as `registered` holds it, for a test.
    #[cfg(all(
        test,
        not(loom),
        not(miri),
        any(target_os = "linux", target_os = "android"),
        any(
            target_pointer_width = "64",
            all(target_arch = "x86", target_feature = "sse2"),
            all(target_arch = "arm", target_has_atomic = "64")
        )
    ))]
    pub(crate) fn connection_wake(&self) -> Option<Interrupter> {
        self.named.lock().wake.clone()
    }

    /// Receives in a row of the connection's drain that did not move it, for a test.
    #[cfg(all(
        test,
        not(loom),
        not(miri),
        any(target_os = "linux", target_os = "android"),
        any(
            target_pointer_width = "64",
            all(target_arch = "x86", target_feature = "sse2"),
            all(target_arch = "arm", target_has_atomic = "64")
        )
    ))]
    pub(crate) fn drain_stalls(&self) -> u32 {
        let drain = self.current.as_ref().and_then(|c| c.drain.as_ref());
        drain.map_or(0, |drain| drain.stalls)
    }

    /// The publishers remembered as connected to before, for a test.
    #[cfg(all(
        test,
        not(loom),
        not(miri),
        any(target_os = "linux", target_os = "android"),
        any(
            target_pointer_width = "64",
            all(target_arch = "x86", target_feature = "sse2"),
            all(target_arch = "arm", target_has_atomic = "64")
        )
    ))]
    pub(crate) fn remembered(&self) -> usize {
        self.named.lock().joined.len()
    }

    /// The connection's position and the end of its drain, for a test.
    #[cfg(all(
        test,
        not(loom),
        not(miri),
        any(target_os = "linux", target_os = "android"),
        any(
            target_pointer_width = "64",
            all(target_arch = "x86", target_feature = "sse2"),
            all(target_arch = "arm", target_has_atomic = "64")
        )
    ))]
    pub(crate) fn drain_position(&self) -> Option<(u64, u64)> {
        let connection = self.current.as_ref()?;
        let end = connection.drain.as_ref()?.end;
        Some((connection.subscriber.position(), end))
    }

    /// The waiting state of the connection taken, for a test that raises a bit on it.
    #[cfg(all(
        test,
        not(loom),
        not(miri),
        any(target_os = "linux", target_os = "android"),
        any(
            target_pointer_width = "64",
            all(target_arch = "x86", target_feature = "sse2"),
            all(target_arch = "arm", target_has_atomic = "64")
        )
    ))]
    pub(crate) fn connection_local(&self) -> Option<Arc<crate::wait::Local<crate::wait::Parts>>> {
        let connection = self.current.as_ref()?;
        Some(connection.subscriber.local().clone())
    }

    /// The death recipient of the connection taken, for a test that runs it.
    #[cfg(all(
        test,
        not(loom),
        not(miri),
        any(target_os = "linux", target_os = "android"),
        any(
            target_pointer_width = "64",
            all(target_arch = "x86", target_feature = "sse2"),
            all(target_arch = "arm", target_has_atomic = "64")
        )
    ))]
    pub(crate) fn connection_death(&self) -> Option<crate::wait::Died> {
        Some(self.current.as_ref()?.subscriber.death_notice())
    }
}

#[cfg(test)]
thread_local! {
    /// Set by a test: `join` then fails as `epoll_ctl` does with `ENOSPC` or `ENOMEM`.
    #[cfg_attr(
        target_os = "android",
        expect(
            clippy::missing_const_for_thread_local,
            reason = "the initializer is `const`; Android's expansion of the macro hides that"
        )
    )]
    pub(crate) static NO_JOIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set by a test: run between a drain's look that gave up lapped and the look after it.
    pub(crate) static BETWEEN_LOOKS: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
    /// Set by a test: run by a drain that ends, before it takes the lock that decides how.
    pub(crate) static BEFORE_DRAIN_ENDS: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
    /// Set by a test: run by `registered` past its unlock, before it wakes the connection's wait.
    pub(crate) static BEFORE_REPLACE: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
    /// Set by a test: run by `interrupt` past its unlock, before it wakes the connection's wait.
    pub(crate) static BEFORE_INTERRUPT: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
    /// Set by a test: run by `finish_locked` past its unlock, before it starts a connection thread.
    pub(crate) static BEFORE_LOOKING: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Clear a pending interrupt, whether one was; the lock is released on return.
#[cfg(feature = "tokio")]
fn take_interrupt<T: Payload>(mut state: MutexGuard<'_, State<T>>) -> bool {
    state.take_interrupt()
}

/// Where a receive stands after [`NamedSubscriber::step`].
enum Step<'n, T: Payload> {
    Done(Result<T, RecvError>),
    /// No connection, nothing to return: wait for a change, the lock held until then.
    Idle(MutexGuard<'n, State<T>>),
    /// Receive from the connection.
    Connected,
}

impl<T: Payload> Sealed for NamedSubscriber<T> {}

/// A [`Wake::Fd`] name subscriber's fd is an epoll instance that holds the current connection's
/// fd and an eventfd its changes of state add to: the same fd for the subscriber's life, through
/// every reconnection.
impl<T: Payload + 'static> Wait for NamedSubscriber<T> {
    fn wait_fd(&self) -> Option<BorrowedFd<'_>> {
        self.named.fds.as_ref().map(NamedFds::fd)
    }

    fn prepare_wait(&mut self) -> bool {
        assert!(
            self.named.fds.is_some(),
            "prepare_wait: a subscription made with `Wake::Futex` has no file descriptor to poll"
        );
        let state = self.named.lock();
        // A notice, a connection, a replacement's records or an eviction: a receive returns now.
        if state.evicted || !state.notices.is_empty() || state.ready.is_some() || state.replaced {
            return false;
        }
        drop(state);
        let armed = match self.current.as_mut() {
            Some(connection) => connection.subscriber.prepare_wait(),
            None => true,
        };
        // After every other reason: the interrupt's kick ends one poll, and is taken by it.
        if !armed || !std::mem::take(&mut self.named.lock().kicked) {
            return armed;
        }
        if let Some(connection) = self.current.as_mut() {
            connection.subscriber.finish_wait();
        }
        false
    }

    fn finish_wait(&mut self) {
        assert!(
            self.named.fds.is_some(),
            "finish_wait: a subscription made with `Wake::Futex` has no file descriptor to poll"
        );
        if let Some(fds) = &self.named.fds {
            fds.drain();
        }
        if let Some(connection) = self.current.as_mut() {
            connection.subscriber.finish_wait();
        }
    }
}

impl<T: Payload> Drop for NamedSubscriber<T> {
    fn drop(&mut self) {
        let mut state = self.named.lock();
        state.dropped = true;
        state.wake = None;
        let ready = state.ready.take();
        drop(state);
        // A connection thread asleep before a retry looks at `dropped` and ends.
        self.named.signal();
        drop(ready);
    }
}

impl<T: Payload> fmt::Debug for NamedSubscriber<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NamedSubscriber")
            .field("type", &T::type_name())
            .field("connected", &self.current.is_some())
            .finish_non_exhaustive()
    }
}

impl<T: Payload> Named<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().expect("lock poisoned")
    }

    /// Tell every wait that `state` changed; called after the change, with the lock released.
    fn signal(&self) {
        Condvar::notify_all(&self.changed);
        #[cfg(feature = "tokio")]
        self.notify.notify_waiters();
        if let Some(fds) = &self.fds {
            fds.poke();
        }
    }

    /// Where a connection to `binder` starts, by the rule of [`NamedSubscriber`]'s documentation.
    fn policy(&self, state: &State<T>, binder: &SIBinder) -> SubscriberPolicy {
        let mut policy = self.policy.clone();
        if state.joined.contains(binder) {
            policy.start = Start::Latest;
        } else if state.later && policy.start != Start::Last {
            policy.start = Start::Oldest;
        }
        policy
    }
}

impl<T: Payload + 'static> Named<T> {
    /// The resolver's callback: keep the binder, and connect to it if nothing else is under way.
    fn registered(self: &Arc<Self>, binder: SIBinder) {
        let mut state = self.lock();
        if state.dropped || state.evicted {
            return;
        }
        state.latest = Some(binder.clone());
        state.registrations += 1;
        if let Some(connected) = &state.connected {
            // The same binder registered again is the same publisher.
            if *connected == binder || state.replaced {
                return;
            }
            state.replaced = true;
            let wake = state.wake.clone();
            drop(state);
            self.signal();
            #[cfg(test)]
            BEFORE_REPLACE.with_borrow_mut(|hook| hook.iter_mut().for_each(|hook| hook()));
            if let Some(wake) = wake {
                wake.notice();
            }
            return;
        }
        if state.connecting {
            drop(state);
            // A thread waiting to retry moves to this binder.
            self.signal();
            return;
        }
        if state.refused.as_ref() == Some(&binder) {
            return;
        }
        state.connecting = true;
        drop(state);
        self.connect(Begin::At(binder), Duration::ZERO);
    }

    /// Start the connection thread at `begin` after `after`; the caller set `connecting`.
    fn connect(self: &Arc<Self>, begin: Begin, after: Duration) {
        let named = self.clone();
        let spawned = std::thread::Builder::new()
            .name("rsbinder-topic-connect".into())
            .spawn(move || named.run(begin, after));
        if spawned.is_err() {
            // The next registration of the name tries again.
            self.lock().connecting = false;
        }
    }

    /// The connection thread: subscribe from `begin` after `after`, retry, move to a newer binder.
    fn run(self: Arc<Self>, begin: Begin, after: Duration) {
        // Whether `binder` is the lookup's answer: its refusal asks no more.
        let (mut binder, mut looked) = match begin {
            Begin::At(binder) => (binder, false),
            Begin::Look(ended) => match self.look(ended.as_ref()) {
                Some(next) => next,
                None => return,
            },
        };
        let mut delay = FIRST_RETRY;
        let mut told = false;
        if !after.is_zero() {
            let state = self.hold(self.lock(), &binder, Instant::now() + after);
            if let Some(newer) = state.latest.clone().filter(|latest| *latest != binder) {
                binder = newer;
            }
        }
        loop {
            let policy = {
                let mut state = self.lock();
                if state.dropped || state.evicted {
                    state.connecting = false;
                    return;
                }
                self.policy(&state, &binder)
            };
            let result = Subscriber::<T>::request(&binder, &policy);
            let mut state = self.lock();
            let newer = state.latest.clone().filter(|latest| *latest != binder);
            if state.dropped || state.evicted {
                state.connecting = false;
                drop(state);
                // Its control page is marked closed as it drops; the publisher takes it out.
                drop(result);
                return;
            }
            let status = match result {
                Ok(subscriber) => {
                    if let Some(newer) = newer {
                        drop(state);
                        drop(subscriber);
                        (binder, looked, delay, told) = (newer, false, FIRST_RETRY, false);
                        continue;
                    }
                    state.ready = Some((subscriber, binder.clone()));
                    state.connected = Some(binder);
                    state.connecting = false;
                    drop(state);
                    self.signal();
                    return;
                }
                Err(status) => status,
            };
            match refusal(&status) {
                Refusal::Final { tell } => {
                    if tell {
                        state.notices.push_back(Disconnected::Rejected(status));
                    }
                    state.refused = Some(binder);
                    let next = newer.filter(|newer| state.refused.as_ref() != Some(newer));
                    let next = match next {
                        Some(next) => {
                            drop(state);
                            (next, false)
                        }
                        None if tell || looked => {
                            state.connecting = false;
                            drop(state);
                            self.signal();
                            return;
                        }
                        // Refused as ended: the notice of the binder after it may have been lost.
                        None => {
                            drop(state);
                            match self.look(None) {
                                Some(next) => next,
                                None => return,
                            }
                        }
                    };
                    self.signal();
                    ((binder, looked), delay, told) = (next, FIRST_RETRY, false);
                }
                Refusal::Retry => {
                    if !told {
                        state.notices.push_back(Disconnected::Rejected(status));
                        told = true;
                        drop(state);
                        self.signal();
                        // `hold` looks at `latest`, `dropped` and `evicted` again under this lock.
                        state = self.lock();
                    }
                    state = self.hold(state, &binder, Instant::now() + delay);
                    match state.latest.clone().filter(|latest| *latest != binder) {
                        Some(newer) => {
                            (binder, looked, delay, told) = (newer, false, FIRST_RETRY, false);
                        }
                        None => delay = (delay * 2).min(LAST_RETRY),
                    }
                    drop(state);
                }
            }
        }
    }

    /// Wait until `until`, the subscriber's drop or eviction, or another binder for the name.
    fn hold<'s>(
        &'s self,
        mut state: MutexGuard<'s, State<T>>,
        binder: &SIBinder,
        until: Instant,
    ) -> MutexGuard<'s, State<T>> {
        while !state.dropped && !state.evicted && state.latest.as_ref() == Some(binder) {
            let now = Instant::now();
            if now >= until {
                break;
            }
            let waited = self.changed.wait_timeout(state, until - now);
            state = waited.expect("lock poisoned").0;
        }
        state
    }

    /// The binder to subscribe to and whether the lookup gave it; `None` clears `connecting`.
    fn look(&self, ended: Option<&SIBinder>) -> Option<(SIBinder, bool)> {
        let mut state = self.lock();
        if state.dropped || state.evicted {
            state.connecting = false;
            return None;
        }
        // The caller saw no usable `latest`: one now came by a notice since, newer than any answer.
        if let Some(latest) = state.latest.clone().filter(|l| usable(l, &state, ended)) {
            return Some((latest, false));
        }
        let lookup = match &mut state.find {
            Find::Lookup(lookup) => Some(lookup.clone()),
            Find::Unknown(wanted) => {
                *wanted = true;
                None
            }
            Find::Absent => None,
        };
        let Some(lookup) = lookup else {
            state.connecting = false;
            return None;
        };
        let mut again = true;
        let next = loop {
            // Counted, not compared: a notice of the binder `latest` already holds is a notice too.
            let before = state.registrations;
            drop(state);
            let found = lookup().ok().flatten();
            state = self.lock();
            if state.dropped || state.evicted {
                break None;
            }
            if state.registrations == before {
                let found = found.filter(|found| usable(found, &state, ended));
                if let Some(found) = &found {
                    state.latest = Some(found.clone());
                }
                break found.map(|found| (found, true));
            }
            let latest = state.latest.clone();
            let latest = latest.filter(|latest| usable(latest, &state, ended));
            if latest.is_some() || !std::mem::take(&mut again) {
                break latest.map(|latest| (latest, false));
            }
            // A notice gave the name a binder it cannot use while it asked: the answer is older.
        };
        state.connecting = next.is_some();
        next
    }
}

/// Whether a connection thread may subscribe to `binder`: neither `ended` nor refused.
fn usable<T: Payload>(binder: &SIBinder, state: &State<T>, ended: Option<&SIBinder>) -> bool {
    Some(binder) != state.refused.as_ref() && Some(binder) != ended
}

impl<T: Payload> Interrupt for Named<T> {
    fn interrupt(&self) {
        let mut state = self.lock();
        state.interrupted = true;
        state.kicked = true;
        let wake = state.wake.clone();
        drop(state);
        self.signal();
        #[cfg(test)]
        BEFORE_INTERRUPT.with_borrow_mut(|hook| hook.iter_mut().for_each(|hook| hook()));
        // A notice, not an interrupt: `interrupted` alone decides the one `Cancelled` it makes.
        if let Some(wake) = wake {
            wake.notice();
        }
    }
}

#[cfg(all(test, not(loom)))]
mod drain_tests {
    use super::{AfterLook, Drain};

    /// What a drain to 32 decides after each of `looks`, given as (gave up, position).
    fn decisions(looks: &[(bool, u64)]) -> Vec<AfterLook> {
        let mut drain = Drain::new(32, 8);
        let decided = looks
            .iter()
            .map(|&(gave_up, at)| drain.after_look(gave_up, at));
        decided.collect()
    }

    #[test]
    fn a_look_that_did_not_give_up_is_returned_and_not_counted() {
        use AfterLook::*;
        assert_eq!(decisions(&[(false, 8)]), [Return]);
        assert_eq!(decisions(&[(false, 40)]), [Return]);
        assert_eq!(decisions(&[(false, 8), (true, 8)]), [Return, Again]);
        assert_eq!(
            decisions(&[(true, 8), (false, 16), (true, 16)]),
            [Again, Return, Settle]
        );
    }

    #[test]
    fn a_drain_settles_at_its_second_look_that_gave_up_short_of_its_end() {
        use AfterLook::*;
        assert_eq!(decisions(&[(true, 8), (true, 8)]), [Again, Settle]);
        assert_eq!(decisions(&[(true, 8), (true, 24)]), [Again, Settle]);
    }

    #[test]
    fn a_look_that_gave_up_at_the_end_or_past_it_settles_at_once() {
        use AfterLook::*;
        assert_eq!(decisions(&[(true, 32)]), [Settle]);
        assert_eq!(decisions(&[(true, 40)]), [Settle]);
        assert_eq!(decisions(&[(true, 24)]), [Again]);
    }

    #[test]
    fn a_receive_that_reads_past_the_furthest_position_counts_no_stall() {
        let mut drain = Drain::new(32, 8);
        drain.count_stall(8);
        drain.count_stall(8);
        assert_eq!(drain.stalls, 2);
        drain.count_stall(16);
        assert_eq!((drain.stalls, drain.reach), (0, 16));
        drain.count_stall(16);
        assert_eq!(drain.stalls, 1);
    }
}
