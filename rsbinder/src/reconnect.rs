// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A service handle that reconnects after its peer goes away (plan 10-10b).
//!
//! [`Reconnecting`] keeps a proxy to one service named by a URI — a kernel
//! `binder://name`, or an RPC endpoint with `#name` (or none, for the root
//! object of a server such as libbinder's `RpcServer::setRootObject`). When
//! the service's process dies or the RPC session ends, it looks the service
//! up again on a thread of its own, runs your [`on_connect`] hook (register
//! callbacks there) and swaps the new proxy in. Calls go through
//! [`with`](Reconnecting::with), which hands the current proxy to a closure.
//!
//! ```ignore
//! use rsbinder::Reconnecting;
//!
//! let listener = listener.clone();
//! let hello = Reconnecting::<dyn IHello>::builder("unix:///run/hello.sock#hello")
//!     .on_connect(move |conn| {
//!         // Every (re)connection, before the new proxy is handed out.
//!         conn.proxy().register_listener(&listener)?;
//!         Ok(())
//!     })
//!     .build()?;
//! let reply = hello.with(|h| h.echo("hi"))?;
//! ```
//!
//! # What it does not do
//!
//! A failed call is never sent again: whether a call may run twice is the
//! service's business, so `with` returns the closure's result as it is. The
//! connection's shape (incoming connections, timeouts) is the URI's and the
//! [`options`](ReconnectBuilder::options)' — the helper adds nothing. It does
//! not register services: a service that outlives a restarted service manager
//! must add itself again.
//!
//! # How a lost peer is noticed
//!
//! | Looked-up binder | Noticed by |
//! |---|---|
//! | kernel proxy | its death notification (`Client::open` starts the binder thread pool) |
//! | RPC, session with incoming connections or a serve loop | its death notification, as the session ends |
//! | RPC, otherwise (the default `unix://…#name`, an Android 16 accessor) | before each call, `RpcTransport::peer_closed` and `RpcSession::is_ended`; after a failed call, `is_ended` |
//! | a local binder (the service lives in this process) | never: it cannot die on its own |
//!
//! A status code is never the signal: a call that ended a session can return
//! `TimedOut`, and a live server's handler can return `DeadObject`.
//!
//! # Calls while reconnecting
//!
//! A call that arrives while a reconnect *attempt* is in progress waits for
//! that attempt, up to [`ReconnectPolicy::call_wait`]; one that arrives
//! between attempts (the backoff, or a kernel service not registered yet)
//! fails at once with `DeadObject`. A server that is already back answers an
//! attempt within milliseconds, so the call runs on the new connection — the
//! closure has not run yet, so nothing is sent twice. A server that is still
//! down does not hold every call up. Inside a binder transaction, or inside
//! `on_connect`, nothing waits: the reconnect may be waiting on that thread.
//! [`wait_connected`](Reconnecting::wait_connected) waits across attempts.
//!
//! # Lifetime
//!
//! Dropping the [`Reconnecting`] (or [`close`](Reconnecting::close)) ends it:
//! the current RPC session is closed and a reconnect in progress stops after
//! its current step, without being waited for. A callback object that needs
//! the helper holds a [`WeakReconnecting`]; a strong handle there would keep
//! the helper, its session and the callback alive in a cycle.
//!
//! AOSP has no such helper; this one is built from `linkToDeath`,
//! `registerForNotifications` and the session-end rules, and its behaviour
//! is rsbinder's own.
//!
//! [`on_connect`]: ReconnectBuilder::on_connect

use std::any::Any;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use crate::entry::uri::{self, Endpoint, Uri};
use crate::entry::{open_staged, Client, ClientOptions, OpenStage};
use crate::hub::{WaitEnd, WaiterState};
#[cfg(feature = "rpc")]
use crate::rpc::RpcSession;
use crate::{DeathRecipient, FromIBinder, Result, SIBinder, Status, StatusCode, Strong, WIBinder};

/// How a [`Reconnecting`] paces its attempts and how long a call waits for one.
///
/// Built from `ReconnectPolicy::default()` and its public fields. The
/// defaults come from measurements on a Linux host (rsb_hub, release build,
/// plan 10-10b §3.8): a service manager forgot a killed service at most
/// 113 µs after the client's death notification, and a restarted service
/// could be looked up 1.5 ms after its process started.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ReconnectPolicy {
    /// Pause after the first failed attempt. Default 10 ms — ~90× the
    /// window in which a lookup still returns the dead binder, so the second
    /// attempt comes after it.
    pub initial: Duration,
    /// Each further failure multiplies the pause by this. Default 2.
    pub multiplier: u32,
    /// The pause never exceeds this. Default 1 s: the longest a recovered
    /// service waits to be reconnected, and one attempt a second against a
    /// service that stays down.
    pub max: Duration,
    /// Each pause is moved by up to this percentage either way, so clients
    /// of a restarted server do not all reconnect at the same instant.
    /// Default 20.
    pub jitter_percent: u8,
    /// After this many failed attempts in a row the helper closes with the
    /// last error. Default `None`: it keeps trying.
    pub max_attempts: Option<u32>,
    /// The longest a [`with`](Reconnecting::with) waits for a reconnect
    /// attempt in progress, and the age past which an attempt is no longer
    /// waited for at all (an attempt that slow is not getting an answer).
    /// Default 1 s.
    pub call_wait: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(10),
            multiplier: 2,
            max: Duration::from_secs(1),
            jitter_percent: 20,
            max_attempts: None,
            call_wait: Duration::from_secs(1),
        }
    }
}

/// Why an [`on_connect`](ReconnectBuilder::on_connect) hook failed:
/// [`retry`](Self::retry) counts it as a failed attempt and backs off,
/// [`stop`](Self::stop) closes the helper.
///
/// `?` on a `StatusCode` or a `Status` makes a retry. Only the hook knows
/// whether a server's answer means "try later" or "never": the same code
/// comes from a configuration error and from a connection that just broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectError {
    code: StatusCode,
    stop: bool,
}

impl ConnectError {
    /// A failed attempt; the helper backs off and reconnects.
    pub fn retry(code: StatusCode) -> Self {
        Self { code, stop: false }
    }

    /// Give up: the helper closes with `code` as its
    /// [`last_error`](Reconnecting::last_error), and `build` returns it.
    pub fn stop(code: StatusCode) -> Self {
        Self { code, stop: true }
    }

    /// The code this error carries.
    pub fn code(&self) -> StatusCode {
        self.code
    }

    /// Whether this error closes the helper.
    pub fn is_stop(&self) -> bool {
        self.stop
    }
}

impl From<StatusCode> for ConnectError {
    fn from(code: StatusCode) -> Self {
        Self::retry(code)
    }
}

impl From<Status> for ConnectError {
    fn from(status: Status) -> Self {
        Self::retry(status.into())
    }
}

/// What an [`on_connect`](ReconnectBuilder::on_connect) hook sees: the new
/// proxy, before any caller does.
pub struct Connection<'a, T: FromIBinder + ?Sized> {
    proxy: &'a Strong<T>,
    generation: u64,
    #[cfg(feature = "rpc")]
    session: Option<&'a RpcSession>,
}

impl<T: FromIBinder + ?Sized> Connection<'_, T> {
    /// The newly connected proxy. A callback the server makes while the
    /// hook runs should use this, not the helper: the helper hands out the
    /// proxy only after the hook returns.
    pub fn proxy(&self) -> &Strong<T> {
        self.proxy
    }

    /// The [`generation`](Reconnecting::generation) this connection gets
    /// once the hook succeeds.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The RPC session the proxy calls over; `None` for kernel binder and a
    /// local service.
    #[cfg(feature = "rpc")]
    pub fn session(&self) -> Option<&RpcSession> {
        self.session
    }
}

type OptionsFn = dyn Fn(&mut ClientOptions, &Endpoint) + Send + Sync;
type OnConnectFn<T> =
    dyn Fn(&Connection<'_, T>) -> std::result::Result<(), ConnectError> + Send + Sync;

/// Configures a [`Reconnecting`]; from [`Reconnecting::builder`].
pub struct ReconnectBuilder<T: FromIBinder + ?Sized> {
    uri: String,
    options: Option<Box<OptionsFn>>,
    on_connect: Option<Box<OnConnectFn<T>>>,
    policy: ReconnectPolicy,
}

impl<T: FromIBinder + ?Sized + 'static> ReconnectBuilder<T> {
    /// Set [`ClientOptions`] for every connection, as
    /// [`Client::open_with`](crate::Client::open_with) takes them. Run again
    /// for each attempt, since `ClientOptions` is not `Clone`; it should
    /// give the same options each time.
    pub fn options(
        mut self,
        f: impl Fn(&mut ClientOptions, &Endpoint) + Send + Sync + 'static,
    ) -> Self {
        self.options = Some(Box::new(f));
        self
    }

    /// Run `f` on every connection — the first and each reconnect — before
    /// its proxy is handed out: register callbacks, subscribe, restore
    /// state the server lost. It runs on the thread connecting (the
    /// `build` caller first, then the helper's), without the helper's lock.
    ///
    /// An `Err` fails that attempt: a [`ConnectError::retry`] (what `?`
    /// makes) backs off and reconnects, a [`ConnectError::stop`] closes the
    /// helper. A call into this same helper from inside the hook does not
    /// wait (`with` returns `DeadObject`, `wait_connected` and `connected`
    /// `WouldBlock`).
    ///
    /// A panic in the hook or in [`options`](Self::options) comes out of
    /// `build` as the panic; on the reconnect thread it closes the helper
    /// with [`StatusCode::Unknown`] as its
    /// [`last_error`](Reconnecting::last_error).
    pub fn on_connect(
        mut self,
        f: impl Fn(&Connection<'_, T>) -> std::result::Result<(), ConnectError> + Send + Sync + 'static,
    ) -> Self {
        self.on_connect = Some(Box::new(f));
        self
    }

    /// Replace the default [`ReconnectPolicy`].
    pub fn policy(mut self, policy: ReconnectPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Make one connection attempt without waiting, and return the helper.
    ///
    /// `Err` only for what another attempt cannot change: a malformed URI,
    /// options that do not apply to it, a missing feature, a kernel URI
    /// without a service name, no binder device for a kernel URI
    /// ([`StatusCode::NoInit`]), a kernel service of another interface
    /// ([`StatusCode::BadType`]; a cast to another interface over RPC is
    /// reported by the first call), an RPC name lookup on a server without
    /// a directory of names, such as a libbinder root (`BadType`), a name
    /// the service manager refuses to watch ([`StatusCode::BadValue`] for an
    /// invalid name, [`StatusCode::PermissionDenied`] for an SELinux `find`
    /// denial), or [`ConnectError::stop`] from `on_connect`.
    ///
    /// Anything else — the server not up yet, the name not registered yet —
    /// returns a helper that keeps connecting in the background, so a
    /// client may start before its server. Use
    /// [`wait_connected`](Reconnecting::wait_connected) to wait for it.
    ///
    /// "Without waiting" covers a server that is absent, not one that
    /// accepts the connection and never answers: the RPC lookup then waits
    /// for its reply, so set [`ClientOptions::timeout`] through
    /// [`options`](Self::options) to bound it (and every later call).
    pub fn build(self) -> Result<Reconnecting<T>> {
        let mut uri = uri::parse(&self.uri)?;
        let lookup = match uri.service.take() {
            Some(name) => Lookup::Name(name),
            None if matches!(uri.endpoint, Endpoint::Kernel { .. }) => {
                log::error!(
                    "Reconnecting: a kernel URI needs a service name ({:?})",
                    self.uri
                );
                return Err(StatusCode::BadValue);
            }
            #[cfg(feature = "rpc")]
            None => Lookup::Root,
            #[cfg(not(feature = "rpc"))]
            None => {
                log::error!("Reconnecting: {:?} needs the `rpc` feature", self.uri);
                return Err(StatusCode::InvalidOperation);
            }
        };
        let shared = Arc::new(Shared {
            cfg: Config {
                uri,
                lookup,
                options: self.options,
                on_connect: self.on_connect,
                policy: self.policy,
            },
            state: Mutex::new(State::new()),
            cv: Condvar::new(),
            #[cfg(feature = "tokio")]
            watch: tokio::sync::watch::channel(()).0,
        });
        let attempt = shared.begin_attempt(&mut shared.lock());
        match shared.attempt(attempt, Mode::Build) {
            Ok(conn) => {
                // `None` for `build`: published, or handed to the background if it died meanwhile.
                let _ = shared.finish_success(conn, false);
            }
            Err(f) if f.permanent => {
                shared.close();
                return Err(f.code);
            }
            Err(f) => {
                log::info!(
                    "Reconnecting: {} not reachable yet ({:?})",
                    shared.cfg,
                    f.code
                );
                let mut st = shared.lock();
                st.in_flight = None;
                st.attempting = None;
                st.last_error = Some(f.code);
                shared.ensure_thread(&mut st);
            }
        }
        Ok(Reconnecting { shared })
    }
}

/// A proxy to one service that reconnects when the service goes away; see
/// the [module doc](self).
///
/// Not `Clone`: dropping it ends the helper. Share it behind an `Arc`, and
/// give callback objects a [`WeakReconnecting`].
pub struct Reconnecting<T: FromIBinder + ?Sized + 'static> {
    shared: Arc<Shared<T>>,
}

impl<T: FromIBinder + ?Sized + 'static> Reconnecting<T> {
    /// Start configuring a helper for `uri`: `binder://name`, or an RPC URI
    /// with `#name` (or without, for the server's root object).
    pub fn builder(uri: &str) -> ReconnectBuilder<T> {
        ReconnectBuilder {
            uri: uri.to_string(),
            options: None,
            on_connect: None,
            policy: ReconnectPolicy::default(),
        }
    }

    /// Run `f` once on the current proxy and return its result.
    ///
    /// Without a connection this is `DeadObject` (converted into `E`), after
    /// waiting for a reconnect attempt in progress as the module doc's
    /// "Calls while reconnecting" states; `f` has not run then. A failed
    /// `f` is not run again — an `Err` that came from the session ending
    /// makes the helper reconnect for the next call.
    pub fn with<R, E: From<StatusCode>>(
        &self,
        f: impl FnOnce(&Strong<T>) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        self.shared.with(f)
    }

    /// The current proxy, without waiting: `DeadObject` while there is no
    /// connection (a reconnect is started if none is under way). For async
    /// code, which must not block here; `connected` (`tokio`) waits.
    pub fn current(&self) -> Result<Strong<T>> {
        self.shared.current()
    }

    /// Wait until connected, across as many attempts as it takes, up to
    /// `timeout` (`None`: no limit). `TimedOut` when it runs out; the
    /// closing error once the helper is closed (`DeadObject` if there was
    /// none). `WouldBlock` inside a binder transaction or `on_connect`,
    /// where waiting could block the reconnect itself. Outside those, when
    /// the reconnect thread cannot be spawned, `Unknown` at once rather than
    /// a wait nothing would end, whatever the spawn's errno, as AOSP
    /// libutils `Thread::run` returns `UNKNOWN_ERROR` when it cannot create
    /// its thread (`Threads.cpp`, which `ProcessState::spawnPooledThread`
    /// relies on). [`last_error`](Self::last_error) is then `Unknown` too,
    /// and the next call tries to spawn the thread again.
    pub fn wait_connected(&self, timeout: Option<Duration>) -> Result<Strong<T>> {
        self.shared.wait_connected(timeout)
    }

    /// The async [`wait_connected`](Self::wait_connected) without a
    /// deadline: resolves once connected, across as many attempts as it
    /// takes, without blocking the executor. Wrap it in
    /// `tokio::time::timeout` for a bound. The closing error once the helper
    /// is closed (`DeadObject` if there was none); `WouldBlock` inside a
    /// binder transaction or `on_connect`, where waiting could block the
    /// reconnect itself. Outside those, a reconnect thread that cannot be
    /// spawned resolves it to `Unknown`, as in `wait_connected`. Dropping the
    /// future stops the wait, not the reconnect.
    #[cfg(feature = "tokio")]
    pub async fn connected(&self) -> Result<Strong<T>> {
        if crate::is_handling_transaction() {
            return Err(StatusCode::WouldBlock);
        }
        // Subscribed before looking, so a publish in between still wakes `changed`.
        let mut published = self.shared.watch.subscribe();
        loop {
            match self.shared.acquire(Wait::Never) {
                Ok(snap) => return Ok(snap.proxy),
                Err(AcquireError::Closed(code)) => {
                    return Err(code.unwrap_or(StatusCode::DeadObject))
                }
                Err(AcquireError::NoThread) => return Err(StatusCode::Unknown),
                Err(_) => {}
            }
            // Inside `on_connect` the publish this awaits would come from this very thread.
            if self.shared.on_reconnect_thread(&self.shared.lock()) {
                return Err(StatusCode::WouldBlock);
            }
            // The sender lives in `Shared`, which `self` keeps alive.
            let _ = published.changed().await;
        }
    }

    /// How many connections this helper has made: 1 after the first, then
    /// one more per reconnect; 0 before the first.
    pub fn generation(&self) -> u64 {
        self.shared.lock().gen
    }

    /// The error of the last failed attempt, or why the helper closed;
    /// `None` once a connection succeeds again.
    pub fn last_error(&self) -> Option<StatusCode> {
        self.shared.lock().last_error
    }

    /// Whether the helper has closed (dropped, [`close`](Self::close)d, out
    /// of [`max_attempts`](ReconnectPolicy::max_attempts), or stopped by
    /// `on_connect` or a refusal). A closed helper never reconnects.
    pub fn is_closed(&self) -> bool {
        self.shared.lock().phase == Phase::Closed
    }

    /// A handle that does not keep the helper alive, for callback objects.
    pub fn downgrade(&self) -> WeakReconnecting<T> {
        WeakReconnecting {
            shared: Arc::downgrade(&self.shared),
        }
    }

    /// Close now; the same as dropping it.
    pub fn close(self) {}
}

impl<T: FromIBinder + ?Sized + 'static> Drop for Reconnecting<T> {
    fn drop(&mut self) {
        self.shared.close();
    }
}

/// A [`Reconnecting`] that does not keep it alive: every call is
/// `DeadObject` once the helper is dropped or closed.
pub struct WeakReconnecting<T: FromIBinder + ?Sized + 'static> {
    shared: Weak<Shared<T>>,
}

impl<T: FromIBinder + ?Sized + 'static> Clone for WeakReconnecting<T> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<T: FromIBinder + ?Sized + 'static> WeakReconnecting<T> {
    /// [`Reconnecting::with`].
    pub fn with<R, E: From<StatusCode>>(
        &self,
        f: impl FnOnce(&Strong<T>) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        match self.shared.upgrade() {
            Some(shared) => shared.with(f),
            None => Err(StatusCode::DeadObject.into()),
        }
    }

    /// [`Reconnecting::current`].
    pub fn current(&self) -> Result<Strong<T>> {
        self.shared
            .upgrade()
            .ok_or(StatusCode::DeadObject)?
            .current()
    }

    /// [`Reconnecting::generation`]; `None` once the helper is gone.
    pub fn generation(&self) -> Option<u64> {
        self.shared.upgrade().map(|s| s.lock().gen)
    }
}

enum Lookup {
    Name(String),
    #[cfg(feature = "rpc")]
    Root,
}

struct Config<T: FromIBinder + ?Sized> {
    uri: Uri,
    lookup: Lookup,
    options: Option<Box<OptionsFn>>,
    on_connect: Option<Box<OnConnectFn<T>>>,
    policy: ReconnectPolicy,
}

impl<T: FromIBinder + ?Sized> std::fmt::Display for Config<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.lookup {
            Lookup::Name(name) => write!(f, "{name:?} on {:?}", self.uri.endpoint),
            #[cfg(feature = "rpc")]
            Lookup::Root => write!(f, "the root of {:?}", self.uri.endpoint),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Connected,
    /// No connection; the reconnect thread, when alive, is attempting or backing off.
    Stale,
    Closed,
}

/// How a published connection's end is seen (module doc "How a lost peer is noticed").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    Recipient,
    #[cfg(feature = "rpc")]
    Probe,
    Never,
}

struct Conn<T: FromIBinder + ?Sized + 'static> {
    proxy: Strong<T>,
    binder: SIBinder,
    attempt: u64,
    #[cfg(feature = "rpc")]
    watch: Watch,
    recipient: Option<Arc<Recipient<T>>>,
    #[cfg(feature = "rpc")]
    session: Option<RpcSession>,
}

struct State<T: FromIBinder + ?Sized + 'static> {
    phase: Phase,
    conn: Option<Conn<T>>,
    gen: u64,
    next_attempt: u64,
    /// The attempt between its lookup and its publication; its recipient marks it dead here.
    in_flight: Option<u64>,
    in_flight_died: bool,
    thread_alive: bool,
    thread_id: Option<ThreadId>,
    /// When the attempt in progress started; `None` while backing off or waiting for a name.
    attempting: Option<Instant>,
    waiter: Option<Arc<WaiterState>>,
    last_error: Option<StatusCode>,
}

impl<T: FromIBinder + ?Sized + 'static> State<T> {
    fn new() -> Self {
        Self {
            phase: Phase::Stale,
            conn: None,
            gen: 0,
            next_attempt: 0,
            in_flight: None,
            in_flight_died: false,
            thread_alive: false,
            thread_id: None,
            attempting: None,
            waiter: None,
            last_error: None,
        }
    }
}

/// Lock rule K4 (plan 10-10b §1): memory only under `state`; death recipients take it.
struct Shared<T: FromIBinder + ?Sized + 'static> {
    cfg: Config<T>,
    state: Mutex<State<T>>,
    cv: Condvar,
    /// Woken on every publish and close, for `connected()`.
    #[cfg(feature = "tokio")]
    watch: tokio::sync::watch::Sender<()>,
}

struct Recipient<T: FromIBinder + ?Sized + 'static> {
    shared: Weak<Shared<T>>,
    attempt: u64,
}

impl<T: FromIBinder + ?Sized + 'static> DeathRecipient for Recipient<T> {
    fn binder_died(&self, _who: &WIBinder) {
        if let Some(shared) = self.shared.upgrade() {
            shared.on_death(self.attempt);
        }
    }
}

/// A connection taken out of the lock for one call.
struct Snapshot<T: FromIBinder + ?Sized + 'static> {
    proxy: Strong<T>,
    #[cfg(feature = "rpc")]
    attempt: u64,
    #[cfg(feature = "rpc")]
    watch: Watch,
    #[cfg(feature = "rpc")]
    session: Option<RpcSession>,
}

#[derive(Debug)]
struct Failure {
    code: StatusCode,
    permanent: bool,
}

impl Failure {
    fn transient(code: StatusCode) -> Self {
        Self {
            code,
            permanent: false,
        }
    }

    fn permanent(code: StatusCode) -> Self {
        Self {
            code,
            permanent: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `build`: one non-waiting lookup.
    Build,
    /// The reconnect thread: a kernel lookup waits for the name to be registered.
    Thread,
}

/// How long `acquire` may wait for a connection.
#[derive(Clone, Copy)]
enum Wait {
    Never,
    /// For an attempt in progress, up to `call_wait` (`with`).
    Attempt,
    /// Across attempts, until the deadline (`wait_connected`).
    Connected(Option<Instant>),
}

enum AcquireError {
    NoConnection,
    Closed(Option<StatusCode>),
    WouldBlock,
    TimedOut,
    /// The reconnect thread could not be spawned, so nothing would end a wait.
    NoThread,
}

impl AcquireError {
    /// What `with` and `current` return: there is no connection, whatever the reason.
    fn dead(self) -> StatusCode {
        match self {
            AcquireError::WouldBlock => StatusCode::WouldBlock,
            _ => StatusCode::DeadObject,
        }
    }
}

fn spawn_reconnect_thread(body: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::FAIL_SPAWN.with(|fail| fail.get()) {
        return Err(std::io::Error::from_raw_os_error(
            rustix::io::Errno::AGAIN.raw_os_error(),
        ));
    }
    std::thread::Builder::new()
        .name("rsbinder-reconnect".into())
        .spawn(body)
        .map(drop)
}

impl<T: FromIBinder + ?Sized + 'static> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify(&self) {
        self.cv.notify_all();
        #[cfg(feature = "tokio")]
        self.watch.send_replace(());
    }

    fn on_death(self: &Arc<Self>, attempt: u64) {
        let mut st = self.lock();
        if st.phase == Phase::Closed {
            return;
        }
        if st.in_flight == Some(attempt) {
            st.in_flight_died = true;
        }
        if st.phase == Phase::Connected && st.conn.as_ref().is_some_and(|c| c.attempt == attempt) {
            log::info!("Reconnecting: {} died; reconnecting", self.cfg);
            st.phase = Phase::Stale;
            self.ensure_thread(&mut st);
        }
        drop(st);
        self.notify();
    }

    /// Under the lock: start the reconnect thread for a `Stale` helper that has none.
    fn ensure_thread(self: &Arc<Self>, st: &mut State<T>) {
        if st.phase != Phase::Stale || st.thread_alive {
            return;
        }
        st.thread_alive = true;
        // The first attempt starts at once, so a call arriving now waits for it.
        st.attempting = Some(Instant::now());
        let shared = Arc::clone(self);
        if let Err(e) = spawn_reconnect_thread(move || shared.run()) {
            log::error!(
                "Reconnecting: cannot start the reconnect thread ({e}); a later call retries"
            );
            st.thread_alive = false;
            st.attempting = None;
            // As AOSP `Thread::run`: `UNKNOWN_ERROR` whatever the errno (`wait_connected` rustdoc).
            st.last_error = Some(StatusCode::Unknown);
        }
    }

    /// Under the lock: take the next attempt id and mark it in flight.
    fn begin_attempt(&self, st: &mut State<T>) -> u64 {
        st.next_attempt += 1;
        st.in_flight = Some(st.next_attempt);
        st.in_flight_died = false;
        st.next_attempt
    }

    /// The reconnect thread: attempt, back off, repeat until connected or closed.
    fn run(self: Arc<Self>) {
        self.lock().thread_id = Some(std::thread::current().id());
        let mut failures: u32 = 0;
        loop {
            let attempt = {
                let mut st = self.lock();
                if st.phase == Phase::Closed {
                    st.thread_alive = false;
                    st.thread_id = None;
                    st.attempting = None;
                    drop(st);
                    self.notify();
                    return;
                }
                st.attempting.get_or_insert_with(Instant::now);
                self.begin_attempt(&mut st)
            };
            let failure = match self.attempt(attempt, Mode::Thread) {
                Ok(conn) => match self.finish_success(conn, true) {
                    None => return,
                    Some(f) => f,
                },
                Err(f) => f,
            };
            failures = failures.saturating_add(1);
            let delay = {
                let mut st = self.lock();
                st.in_flight = None;
                st.attempting = None;
                st.last_error = Some(failure.code);
                let exhausted = self
                    .cfg
                    .policy
                    .max_attempts
                    .is_some_and(|max| failures >= max);
                if st.phase == Phase::Closed || failure.permanent || exhausted {
                    if st.phase != Phase::Closed {
                        log::warn!(
                            "Reconnecting: giving up on {} after {failures} attempt(s) ({:?})",
                            self.cfg,
                            failure.code
                        );
                        st.phase = Phase::Closed;
                    }
                    st.thread_alive = false;
                    st.thread_id = None;
                    let old = st.conn.take();
                    drop(st);
                    self.notify();
                    if let Some(old) = old {
                        self.discard(old);
                    }
                    return;
                }
                log::warn!(
                    "Reconnecting: attempt {attempt} for {} failed ({:?})",
                    self.cfg,
                    failure.code
                );
                self.backoff(failures)
            };
            // Callers waiting on this attempt see it end and return.
            self.notify();
            let st = self.lock();
            drop(
                self.cv
                    .wait_timeout_while(st, delay, |st| st.phase != Phase::Closed)
                    .unwrap_or_else(|e| e.into_inner()),
            );
        }
    }

    /// Publish `conn` unless closed or dead; `Some` = back off. `thread`: the thread then ends.
    fn finish_success(self: &Arc<Self>, conn: Conn<T>, thread: bool) -> Option<Failure> {
        let mut st = self.lock();
        if st.phase == Phase::Closed {
            st.in_flight = None;
            if thread {
                st.thread_alive = false;
                st.thread_id = None;
            }
            drop(st);
            self.notify();
            self.discard(conn);
            return None;
        }
        if st.in_flight_died {
            st.in_flight = None;
            drop(st);
            self.discard(conn);
            if !thread {
                // `build`: the background takes over, as for any transient failure.
                let mut st = self.lock();
                st.attempting = None;
                st.last_error = Some(StatusCode::DeadObject);
                self.ensure_thread(&mut st);
                return None;
            }
            return Some(Failure::transient(StatusCode::DeadObject));
        }
        st.gen += 1;
        st.in_flight = None;
        st.attempting = None;
        st.last_error = None;
        st.phase = Phase::Connected;
        if thread {
            st.thread_alive = false;
            st.thread_id = None;
        }
        let old = st.conn.replace(conn);
        log::info!(
            "Reconnecting: connected to {} (generation {})",
            self.cfg,
            st.gen
        );
        drop(st);
        self.notify();
        if let Some(old) = old {
            self.discard(old);
        }
        None
    }

    /// The pause after `failures` failed attempts in a row (`ReconnectPolicy`).
    fn backoff(&self, failures: u32) -> Duration {
        let p = &self.cfg.policy;
        let steps = failures.saturating_sub(1);
        let base = p
            .initial
            .saturating_mul(p.multiplier.max(1).saturating_pow(steps))
            .min(p.max);
        let j = u128::from(p.jitter_percent.min(100));
        if j == 0 {
            return base;
        }
        let mut h = RandomState::new().build_hasher();
        h.write_u32(failures);
        let pct = 100 - j + u128::from(h.finish()) % (2 * j + 1);
        let nanos = base.as_nanos() * pct / 100;
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX)).min(p.max)
    }

    /// Unlink and end what a dropped connection holds; never under the lock.
    fn discard(&self, conn: Conn<T>) {
        if let Some(r) = &conn.recipient {
            let _ = conn.binder.unlink_to_death_arc(r);
        }
        #[cfg(feature = "rpc")]
        if let Some(s) = &conn.session {
            if !s.is_ended() {
                s.close_session();
            }
        }
        drop(conn);
    }

    fn close(&self) {
        let (conn, waiter) = {
            let mut st = self.lock();
            if st.phase == Phase::Closed {
                return;
            }
            st.phase = Phase::Closed;
            (st.conn.take(), st.waiter.take())
        };
        self.notify();
        if let Some(w) = waiter {
            w.cancel();
        }
        if let Some(c) = conn {
            self.discard(c);
        }
    }

    /// One connection attempt: open, look up, cast, watch, `on_connect`. No lock held across.
    fn attempt(self: &Arc<Self>, id: u64, mode: Mode) -> std::result::Result<Conn<T>, Failure> {
        let mut options = ClientOptions::default();
        if let Some(f) = &self.cfg.options {
            let ran = catch_unwind(AssertUnwindSafe(|| f(&mut options, &self.cfg.uri.endpoint)));
            if let Err(payload) = ran {
                return Err(self.user_panic(mode, payload));
            }
        }
        let client =
            open_staged(self.cfg.uri.clone(), options).map_err(|(stage, code)| Failure {
                code,
                permanent: stage == OpenStage::Setup,
            })?;
        #[cfg(feature = "rpc")]
        let opened = client.session().cloned();
        let binder = match self.lookup(&client, mode) {
            Ok(b) => b,
            Err(f) => {
                #[cfg(feature = "rpc")]
                end_if_live(opened.as_ref());
                return Err(f);
            }
        };
        drop(client);

        #[cfg(feature = "rpc")]
        let session = (*binder)
            .as_any()
            .downcast_ref::<crate::rpc::RpcProxy>()
            .map(|p| p.session());
        // Defensive (`manager` gets handle >= 1): a handle-0 link latches `hub::default()` dead.
        let watch = if !binder.is_remote() || (*binder).as_proxy().is_some_and(|p| p.handle() == 0)
        {
            Watch::Never
        } else {
            #[cfg(feature = "rpc")]
            match &session {
                Some(s) if !s.notices_connection_loss() => Watch::Probe,
                _ => Watch::Recipient,
            }
            #[cfg(not(feature = "rpc"))]
            Watch::Recipient
        };
        let mut conn_parts = Parts {
            binder,
            #[cfg(feature = "rpc")]
            session,
            recipient: None,
        };

        let proxy: Strong<T> = match FromIBinder::try_from(conn_parts.binder.clone()) {
            Ok(p) => p,
            Err(code) => {
                self.end_parts(conn_parts);
                return Err(Failure {
                    code,
                    permanent: code == StatusCode::BadType,
                });
            }
        };

        if watch == Watch::Recipient {
            let recipient = Arc::new(Recipient {
                shared: Arc::downgrade(self),
                attempt: id,
            });
            if let Err(code) = conn_parts.binder.link_to_death_arc(&recipient) {
                // A dead binder the service manager has not forgotten yet, or a session gone.
                drop(proxy);
                self.end_parts(conn_parts);
                return Err(Failure::transient(code));
            }
            conn_parts.recipient = Some(recipient);
        }

        let generation = {
            let st = self.lock();
            if st.phase == Phase::Closed {
                drop(st);
                drop(proxy);
                self.end_parts(conn_parts);
                return Err(Failure::permanent(StatusCode::DeadObject));
            }
            st.gen + 1
        };
        if let Some(hook) = &self.cfg.on_connect {
            let c = Connection {
                proxy: &proxy,
                generation,
                #[cfg(feature = "rpc")]
                session: conn_parts.session.as_ref(),
            };
            let failed = match catch_unwind(AssertUnwindSafe(|| hook(&c))) {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(Ok(Failure {
                    code: e.code,
                    permanent: e.stop,
                })),
                Err(payload) => Some(Err(payload)),
            };
            if let Some(failed) = failed {
                drop(proxy);
                self.end_parts(conn_parts);
                return Err(failed.unwrap_or_else(|payload| self.user_panic(mode, payload)));
            }
        }
        Ok(Conn {
            proxy,
            binder: conn_parts.binder,
            attempt: id,
            #[cfg(feature = "rpc")]
            watch,
            recipient: conn_parts.recipient,
            #[cfg(feature = "rpc")]
            session: conn_parts.session,
        })
    }

    /// A user closure panicked: `build`'s caller gets the panic, the reconnect thread closes.
    fn user_panic(&self, mode: Mode, payload: Box<dyn Any + Send>) -> Failure {
        if mode == Mode::Build {
            resume_unwind(payload);
        }
        log::error!(
            "Reconnecting: a user closure panicked for {}; closing",
            self.cfg
        );
        Failure::permanent(StatusCode::Unknown)
    }

    /// Undo a failed attempt's link and session (plan 10-10b K6).
    fn end_parts(&self, parts: Parts<T>) {
        if let Some(r) = &parts.recipient {
            let _ = parts.binder.unlink_to_death_arc(r);
        }
        #[cfg(feature = "rpc")]
        end_if_live(parts.session.as_ref());
    }

    fn lookup(&self, client: &Client, mode: Mode) -> std::result::Result<SIBinder, Failure> {
        match &self.cfg.lookup {
            Lookup::Name(name) => {
                #[cfg(feature = "rpc")]
                if let Some(s) = client.session() {
                    return s.get_service(name).map_err(lookup_failure);
                }
                let _ = client;
                self.kernel_lookup(name, mode)
            }
            #[cfg(feature = "rpc")]
            Lookup::Root => client
                .session()
                .ok_or(Failure::permanent(StatusCode::BadValue))?
                .get_root()
                .map_err(lookup_failure),
        }
    }

    /// `build` looks once; the thread then waits for the name to be registered, not attempting.
    fn kernel_lookup(&self, name: &str, mode: Mode) -> std::result::Result<SIBinder, Failure> {
        let sm = crate::hub::default().map_err(Failure::transient)?;
        if let Some(b) = sm.try_get_service(name).map_err(Failure::transient)? {
            return Ok(b);
        }
        // Android 10 has no registration to probe; the thread's wait polls instead.
        #[cfg(all(target_os = "android", feature = "android_10"))]
        if mode == Mode::Build && matches!(*sm, crate::hub::ServiceManager::Android10(_)) {
            return Err(Failure::transient(StatusCode::NameNotFound));
        }
        if mode == Mode::Build {
            // `getService` answers a denied name as an absent one; a registration tells them apart.
            let probe = crate::hub::BnServiceCallback::new_binder(Unwatched);
            return match sm.register_for_notifications_status(name, &probe) {
                Ok(()) => {
                    let _ = sm.unregister_for_notifications(name, &probe);
                    Err(Failure::transient(StatusCode::NameNotFound))
                }
                Err(status) => Err(wait_failure(WaitEnd::from_registration(status))),
            };
        }
        let waiter = Arc::new(WaiterState::default());
        {
            let mut st = self.lock();
            if st.phase == Phase::Closed {
                return Err(Failure::permanent(StatusCode::DeadObject));
            }
            st.attempting = None;
            st.waiter = Some(Arc::clone(&waiter));
        }
        self.notify();
        let found = sm.wait_for_service_cancellable(name, waiter);
        {
            let mut st = self.lock();
            st.waiter = None;
            if st.phase != Phase::Closed {
                st.attempting = Some(Instant::now());
            }
        }
        found.map_err(wait_failure)
    }

    fn on_reconnect_thread(&self, st: &State<T>) -> bool {
        st.thread_id == Some(std::thread::current().id())
    }

    /// Take a connection for one call, starting a reconnect and waiting as `wait` allows.
    fn acquire(self: &Arc<Self>, wait: Wait) -> std::result::Result<Snapshot<T>, AcquireError> {
        let started = Instant::now();
        let call_wait = self.cfg.policy.call_wait;
        let mut st = self.lock();
        let may_wait = !crate::is_handling_transaction() && !self.on_reconnect_thread(&st);
        loop {
            match st.phase {
                Phase::Closed => return Err(AcquireError::Closed(st.last_error)),
                Phase::Stale => {
                    self.ensure_thread(&mut st);
                    // Before `NoThread`: in a transaction a wait is `WouldBlock`, as `connected`.
                    if matches!(wait, Wait::Connected(_)) && !may_wait {
                        return Err(AcquireError::WouldBlock);
                    }
                    // Stale with no thread only after a failed spawn: no notify would end a wait.
                    if !st.thread_alive {
                        return Err(AcquireError::NoThread);
                    }
                    let deadline = match wait {
                        Wait::Never => return Err(AcquireError::NoConnection),
                        Wait::Attempt if !may_wait => return Err(AcquireError::NoConnection),
                        Wait::Attempt => match st.attempting {
                            // An unrepresentable deadline waits without one.
                            Some(t0) if t0.elapsed() < call_wait => started.checked_add(call_wait),
                            _ => return Err(AcquireError::NoConnection),
                        },
                        Wait::Connected(deadline) => deadline,
                    };
                    st = match deadline {
                        None => self.cv.wait(st).unwrap_or_else(|e| e.into_inner()),
                        Some(d) => {
                            let now = Instant::now();
                            if now >= d {
                                return Err(match wait {
                                    Wait::Connected(_) => AcquireError::TimedOut,
                                    _ => AcquireError::NoConnection,
                                });
                            }
                            self.cv
                                .wait_timeout(st, d - now)
                                .unwrap_or_else(|e| e.into_inner())
                                .0
                        }
                    };
                }
                Phase::Connected => {
                    let conn = st.conn.as_ref().expect("Connected holds a connection");
                    let snap = Snapshot {
                        proxy: conn.proxy.clone(),
                        #[cfg(feature = "rpc")]
                        attempt: conn.attempt,
                        #[cfg(feature = "rpc")]
                        watch: conn.watch,
                        #[cfg(feature = "rpc")]
                        session: conn.session.clone(),
                    };
                    drop(st);
                    #[cfg(feature = "rpc")]
                    if self.ended_before_call(&snap) {
                        let attempt = snap.attempt;
                        drop(snap);
                        st = self.lock();
                        self.mark_stale(&mut st, attempt);
                        continue;
                    }
                    return Ok(snap);
                }
            }
        }
    }

    /// An RPC session that ended, or whose unwatched peer closed, before this call was sent.
    #[cfg(feature = "rpc")]
    fn ended_before_call(&self, snap: &Snapshot<T>) -> bool {
        let Some(s) = &snap.session else {
            return false;
        };
        let ended = s.is_ended() || (snap.watch == Watch::Probe && s.peer_closed());
        if ended {
            s.fail();
        }
        ended
    }

    /// Under the lock: the connection `attempt` has ended; reconnect unless already replaced.
    #[cfg(feature = "rpc")]
    fn mark_stale(self: &Arc<Self>, st: &mut State<T>, attempt: u64) {
        if st.phase == Phase::Connected && st.conn.as_ref().is_some_and(|c| c.attempt == attempt) {
            log::info!("Reconnecting: {} ended; reconnecting", self.cfg);
            st.phase = Phase::Stale;
            self.ensure_thread(st);
            self.cv.notify_all();
        }
    }

    fn with<R, E: From<StatusCode>>(
        self: &Arc<Self>,
        f: impl FnOnce(&Strong<T>) -> std::result::Result<R, E>,
    ) -> std::result::Result<R, E> {
        let snap = self.acquire(Wait::Attempt).map_err(|e| E::from(e.dead()))?;
        let result = f(&snap.proxy);
        #[cfg(feature = "rpc")]
        if result.is_err() && snap.session.as_ref().is_some_and(RpcSession::is_ended) {
            let mut st = self.lock();
            self.mark_stale(&mut st, snap.attempt);
        }
        result
    }

    fn current(self: &Arc<Self>) -> Result<Strong<T>> {
        self.acquire(Wait::Never)
            .map(|s| s.proxy)
            .map_err(AcquireError::dead)
    }

    fn wait_connected(self: &Arc<Self>, timeout: Option<Duration>) -> Result<Strong<T>> {
        let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
        self.acquire(Wait::Connected(deadline))
            .map(|s| s.proxy)
            .map_err(|e| match e {
                AcquireError::Closed(code) => code.unwrap_or(StatusCode::DeadObject),
                AcquireError::WouldBlock => StatusCode::WouldBlock,
                AcquireError::TimedOut => StatusCode::TimedOut,
                AcquireError::NoConnection => StatusCode::DeadObject,
                AcquireError::NoThread => StatusCode::Unknown,
            })
    }
}

/// What an attempt holds before it becomes a `Conn`.
struct Parts<T: FromIBinder + ?Sized + 'static> {
    binder: SIBinder,
    #[cfg(feature = "rpc")]
    session: Option<RpcSession>,
    recipient: Option<Arc<Recipient<T>>>,
}

/// A live session the helper drops must be closed: its incoming threads hold it (K6).
#[cfg(feature = "rpc")]
fn end_if_live(session: Option<&RpcSession>) {
    if let Some(s) = session {
        if !s.is_ended() {
            s.close_session();
        }
    }
}

/// `BadType` is another interface (or a libbinder root asked for a name); the rest may pass.
#[cfg(feature = "rpc")]
fn lookup_failure(code: StatusCode) -> Failure {
    Failure {
        code,
        permanent: code == StatusCode::BadType,
    }
}

fn wait_failure(end: WaitEnd) -> Failure {
    match end {
        WaitEnd::Transient(code) => Failure::transient(code),
        WaitEnd::Refused(status) => {
            log::error!("Reconnecting: the service manager refused to watch the name ({status})");
            // `StatusCode::from` would fold both exceptions into `FailedTransaction`.
            Failure::permanent(match status.exception_code() {
                crate::ExceptionCode::Security => StatusCode::PermissionDenied,
                _ => StatusCode::BadValue,
            })
        }
        WaitEnd::Cancelled => Failure::permanent(StatusCode::DeadObject),
    }
}

/// The callback `build` registers only to learn whether the name may be watched.
struct Unwatched;

impl crate::Interface for Unwatched {}

impl crate::hub::IServiceCallback for Unwatched {
    fn onRegistration(&self, _name: &str, _service: &SIBinder) -> crate::status::BinderResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// Makes `spawn_reconnect_thread` fail on this thread, as `EAGAIN` would.
        pub(super) static FAIL_SPAWN: Cell<bool> = const { Cell::new(false) };
    }

    struct Unreachable;

    impl crate::Interface for Unreachable {}

    impl FromIBinder for Unreachable {
        fn try_from(_: SIBinder) -> Result<Strong<Self>> {
            Err(StatusCode::BadType)
        }
    }

    /// A helper as `build` leaves it after a transient failure: `Stale`, no thread yet.
    fn stale_helper() -> Reconnecting<Unreachable> {
        let name = "rsbinder-reconnect-spawn-test";
        Reconnecting {
            shared: Arc::new(Shared {
                cfg: Config {
                    uri: uri::parse(&format!("binder://{name}")).expect("a kernel URI"),
                    lookup: Lookup::Name(name.into()),
                    options: None,
                    on_connect: None,
                    policy: ReconnectPolicy::default(),
                },
                state: Mutex::new(State::new()),
                cv: Condvar::new(),
                #[cfg(feature = "tokio")]
                watch: tokio::sync::watch::channel(()).0,
            }),
        }
    }

    /// No reconnect thread means no notify: every wait returns the spawn error instead of parking.
    #[test]
    fn a_reconnect_thread_that_cannot_spawn_ends_every_wait() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            FAIL_SPAWN.with(|fail| fail.set(true));
            let h = stale_helper();
            let waited = h.wait_connected(None).err();
            let current = h.current().err();
            #[cfg(feature = "tokio")]
            let connected = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime")
                .block_on(h.connected())
                .err();
            #[cfg(not(feature = "tokio"))]
            let connected = waited;
            let _ = tx.send((waited, current, connected));
        });
        let (waited, current, connected) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("a wait with no reconnect thread must not park");
        assert_eq!(waited, Some(StatusCode::Unknown));
        assert_eq!(current, Some(StatusCode::DeadObject));
        assert_eq!(connected, Some(StatusCode::Unknown));
    }
}
