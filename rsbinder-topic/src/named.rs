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

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use rsbinder::{BinderResult, ExceptionCode, SIBinder, Status, StatusCode};

use crate::aidl::ITopic::{ERROR_CLOSED, ERROR_VERSION};
use crate::resolve::{Resolver, ServiceManager, Watch};
use crate::subscriber::Interrupt;
use crate::{
    Disconnected, Interrupter, Payload, RecvError, Start, Subscriber, SubscriberPolicy,
    SubscriberStats,
};

/// The first wait before a refused subscription is asked again, doubled up to [`LAST_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(30);

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
/// connection; a `NamedSubscriber` never returns [`RecvError::Closed`]. Code that only wants the
/// values goes on past it:
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
///         Err(RecvError::Disconnected(_)) => continue,
///         Err(_) => break,
///     }
/// }
/// # Ok(()) }
/// ```
///
/// Records do not carry over from one connection to the next: the index starts again, and what
/// was published between the two is not counted as lost (the `Disconnected` marks the gap). A
/// connection after the first starts at the oldest record of the new ring, unless the policy's
/// [`start`](SubscriberPolicy::start) is [`Start::Last`]: the subscription is older than that
/// publisher, so all it published is news to it.
///
/// **The process has to run the binder thread pool**
/// (`rsbinder::ProcessState::start_thread_pool`): the service manager's notice of the name is a
/// call into it, and without the pool the subscriber never connects.
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
    /// Where the records to return end, once another publisher took the name.
    drain_to: Option<u64>,
}

/// What the subscriber, the resolver's callback and the connection thread share.
struct Named<T: Payload> {
    policy: SubscriberPolicy,
    state: Mutex<State<T>>,
    changed: Condvar,
}

struct State<T: Payload> {
    dropped: bool,
    /// An interrupt not yet returned; the connection's own interrupt is raised with it.
    interrupted: bool,
    /// The binder the name was last given.
    latest: Option<SIBinder>,
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
    /// A connection was made before: the next starts at the new ring's oldest record.
    reconnect: bool,
}

impl<T: Payload> State<T> {
    fn new() -> Self {
        Self {
            dropped: false,
            interrupted: false,
            latest: None,
            ready: None,
            connected: None,
            refused: None,
            connecting: false,
            replaced: false,
            notices: VecDeque::new(),
            wake: None,
            reconnect: false,
        }
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
        let named = Arc::new(Named {
            policy: policy.clone(),
            state: Mutex::new(State::new()),
            changed: Condvar::new(),
        });
        let weak = Arc::downgrade(&named);
        let watch = resolver.watch(
            name,
            Box::new(move |binder| {
                if let Some(named) = weak.upgrade() {
                    named.registered(binder);
                }
            }),
        )?;
        Ok(Self {
            named,
            current: None,
            past: SubscriberStats::default(),
            _watch: watch,
        })
    }

    /// What this subscriber has received over all its connections, counted in this process
    /// (plan 15 §5.9a).
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
            let mut state = self.named.lock();
            if let Some(notice) = state.notices.pop_front() {
                return Err(RecvError::Disconnected(notice));
            }
            if self.current.is_none() {
                if let Some((subscriber, binder)) = state.ready.take() {
                    let wake = subscriber.interrupter();
                    if state.interrupted {
                        wake.interrupt();
                    }
                    state.wake = Some(wake);
                    self.current = Some(Connection {
                        subscriber,
                        binder,
                        drain_to: None,
                    });
                }
            }
            let Some(connection) = self.current.as_mut() else {
                let Some(deadline) = wait else {
                    return Err(RecvError::Empty);
                };
                if std::mem::take(&mut state.interrupted) {
                    return Err(RecvError::Cancelled);
                }
                match deadline {
                    None => drop(self.named.changed.wait(state).expect("lock poisoned")),
                    Some(deadline) => {
                        let now = Instant::now();
                        if now >= deadline {
                            return Err(RecvError::Timeout);
                        }
                        let waited = self.named.changed.wait_timeout(state, deadline - now);
                        drop(waited.expect("lock poisoned"));
                    }
                }
                continue;
            };
            if state.replaced && connection.drain_to.is_none() {
                connection.drain_to = Some(connection.subscriber.committed());
            }
            drop(state);
            if let Some(end) = connection.drain_to {
                if connection.subscriber.position() < end {
                    let received = if latest {
                        connection.subscriber.try_latest()
                    } else {
                        connection.subscriber.try_recv()
                    };
                    match received {
                        Err(RecvError::Empty | RecvError::Closed(_)) => {}
                        other => return other,
                    }
                }
                return Err(self.finish(Disconnected::Replaced));
            }
            let received = match wait {
                None if latest => connection.subscriber.try_latest(),
                None => connection.subscriber.try_recv(),
                Some(deadline) => connection.subscriber.wait(deadline, latest),
            };
            match received {
                Err(RecvError::Cancelled) => {
                    if std::mem::take(&mut self.named.lock().interrupted) {
                        return Err(RecvError::Cancelled);
                    }
                    // A change of state ended the wait: look at it.
                }
                Err(RecvError::Closed(closed)) => {
                    return Err(self.finish(Disconnected::Ended(closed)));
                }
                other => return other,
            }
        }
    }

    /// Drop the connection, look for the next publisher, and say why.
    fn finish(&mut self, why: Disconnected) -> RecvError {
        let gone = self.current.take();
        if let Some(connection) = &gone {
            self.past += connection.subscriber.stats();
        }
        let mut state = self.named.lock();
        state.connected = None;
        state.wake = None;
        state.replaced = false;
        let old = gone.as_ref().map(|connection| &connection.binder);
        let next = state
            .latest
            .clone()
            .filter(|latest| Some(latest) != old && Some(latest) != state.refused.as_ref());
        if let Some(next) = next.filter(|_| !state.connecting) {
            state.connecting = true;
            drop(state);
            self.named.connect(next);
        } else {
            drop(state);
        }
        // Unmapped outside the lock.
        drop(gone);
        RecvError::Disconnected(why)
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
        self.named.changed.notify_all();
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

    /// Where a connection starts: the policy's place first, the new ring's oldest record after.
    fn policy(&self, reconnect: bool) -> SubscriberPolicy {
        let mut policy = self.policy.clone();
        if reconnect && policy.start != Start::Last {
            policy.start = Start::Oldest;
        }
        policy
    }
}

impl<T: Payload + 'static> Named<T> {
    /// The resolver's callback: keep the binder, and connect to it if nothing else is under way.
    fn registered(self: &Arc<Self>, binder: SIBinder) {
        let mut state = self.lock();
        if state.dropped {
            return;
        }
        state.latest = Some(binder.clone());
        if let Some(connected) = &state.connected {
            // The same binder registered again is the same publisher.
            if *connected == binder || state.replaced {
                return;
            }
            state.replaced = true;
            let wake = state.wake.clone();
            drop(state);
            self.changed.notify_all();
            if let Some(wake) = wake {
                wake.interrupt();
            }
            return;
        }
        if state.connecting {
            drop(state);
            // A thread waiting to retry moves to this binder.
            self.changed.notify_all();
            return;
        }
        if state.refused.as_ref() == Some(&binder) {
            return;
        }
        state.connecting = true;
        drop(state);
        self.connect(binder);
    }

    /// Start the connection thread; the caller has set `connecting`.
    fn connect(self: &Arc<Self>, binder: SIBinder) {
        let named = self.clone();
        let spawned = std::thread::Builder::new()
            .name("rsbinder-topic-connect".into())
            .spawn(move || named.run(binder));
        if spawned.is_err() {
            // The next registration of the name tries again.
            self.lock().connecting = false;
        }
    }

    /// The connection thread: subscribe to `binder`, retrying while the refusal may change, and
    /// moving to a newer binder of the name.
    fn run(self: Arc<Self>, mut binder: SIBinder) {
        let mut delay = FIRST_RETRY;
        let mut told = false;
        loop {
            let policy = {
                let mut state = self.lock();
                if state.dropped {
                    state.connecting = false;
                    return;
                }
                self.policy(state.reconnect)
            };
            let result = Subscriber::<T>::request(&binder, &policy);
            let mut state = self.lock();
            let newer = state.latest.clone().filter(|latest| *latest != binder);
            if state.dropped {
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
                        (binder, delay, told) = (newer, FIRST_RETRY, false);
                        continue;
                    }
                    state.ready = Some((subscriber, binder.clone()));
                    state.connected = Some(binder);
                    state.reconnect = true;
                    state.connecting = false;
                    drop(state);
                    self.changed.notify_all();
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
                    let Some(next) = next else {
                        state.connecting = false;
                        drop(state);
                        self.changed.notify_all();
                        return;
                    };
                    drop(state);
                    self.changed.notify_all();
                    (binder, delay, told) = (next, FIRST_RETRY, false);
                }
                Refusal::Retry => {
                    if !told {
                        state.notices.push_back(Disconnected::Rejected(status));
                        told = true;
                        self.changed.notify_all();
                    }
                    let until = Instant::now() + delay;
                    while !state.dropped && state.latest.as_ref() == Some(&binder) {
                        let now = Instant::now();
                        if now >= until {
                            break;
                        }
                        let waited = self.changed.wait_timeout(state, until - now);
                        state = waited.expect("lock poisoned").0;
                    }
                    match state.latest.clone().filter(|latest| *latest != binder) {
                        Some(newer) => (binder, delay, told) = (newer, FIRST_RETRY, false),
                        None => delay = (delay * 2).min(LAST_RETRY),
                    }
                    drop(state);
                }
            }
        }
    }
}

impl<T: Payload> Interrupt for Named<T> {
    fn interrupt(&self) {
        let mut state = self.lock();
        state.interrupted = true;
        let wake = state.wake.clone();
        drop(state);
        self.changed.notify_all();
        if let Some(wake) = wake {
            wake.interrupt();
        }
    }
}
