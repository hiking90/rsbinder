// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Topics without kernel binder: the subscription call and the death notices over rsbinder's RPC
//! on a Unix domain socket (plans/15-pubsub-uds), for a Linux where binderfs cannot be used, as
//! in a container without `CAP_SYS_ADMIN`, a kernel without `CONFIG_ANDROID_BINDERFS` or a CI
//! runner with no binder device. **Experimental.**
//!
//! Only the setup moves. The records still travel through the sealed memfd ring and the wakes
//! through a futex or a socket pair, which binder never carried, so a
//! [`Publisher`](crate::Publisher) and its subscribers behave the same once connected.
//!
//! ```no_run
//! use rsbinder::rpc::RpcServer;
//! use rsbinder_topic::{uds, Bytes, Publisher, PublisherPolicy, SubscriberPolicy};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // The publishing process.
//! let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default())?;
//! let server = RpcServer::setup_unix_server("/run/example/topic.sock")?;
//! let serving = uds::serve(&server, publisher.as_binder())?;
//! publisher.publish(&Bytes(b"hello".to_vec()))?;
//! // In this order: subscribers read the topic's end before the server ends their sessions.
//! drop(publisher);
//! drop(serving);
//!
//! // A subscribing process.
//! let endpoint = uds::Endpoint::Path("/run/example/topic.sock".into());
//! let mut subscriber = uds::subscribe::<Bytes>(endpoint, &SubscriberPolicy::default())?;
//! # let _ = subscriber.recv();
//! # Ok(()) }
//! ```
//!
//! # What differs from kernel binder
//!
//! - **The caller's identity is fixed at connect.** Kernel binder names the caller of each
//!   transaction. Here [`Authorize`](crate::Authorize) and
//!   [`SubscriberInfo`](crate::SubscriberInfo) see `SO_PEERCRED` of the connection: a process
//!   that hands its connected socket to another makes that one's calls look like its own.
//! - **Resources grow per subscriber.** Kernel binder serves every peer with one descriptor and
//!   one thread pool. Here each subscriber's session is two sockets and two threads at each end
//!   (measured, plans/15-pubsub-uds §12), for as long as it lasts. The bound of
//!   [`RpcServer::set_max_connections`] counts serve threads: one per subscriber, plus one free
//!   while a subscriber's second socket attaches.
//! - **Death is the session's end.** The loss of any connection of a session is a death: a
//!   deadline on the session that runs out on a slow callback reads as
//!   [`Closed::PublisherDied`](crate::Closed::PublisherDied). The session [`subscribe`] makes has
//!   a deadline during setup and none afterwards ([`Options::timeout`]); one the caller
//!   gives [`Subscriber::request`] keeps what the caller set.
//! - **`uid` and `pid` depend on the namespaces.** Across user namespaces a uid with no mapping
//!   reads 65534 to [`Authorize`](crate::Authorize) and to [`Options::expect_uid`], and a pid
//!   outside this pid namespace reads -1 ([`PeerIdentity::pid`] returns `None`).
//! - **A publisher serves one name at a time.** [`Publisher`](crate::Publisher) holds one
//!   advertisement, and `advertise_on` with any resolver replaces the one it has, so one publisher
//!   is not advertised to a kernel registry and over a socket at once. `rsbinder::hub::add_service`
//!   of its binder takes no advertisement and goes with either.

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rsbinder::rpc::{
    FileDescriptorTransportMode, PeerIdentity, RpcClientConfig, RpcServer, RpcServerHandle,
    RpcSession,
};
#[cfg(not(any(target_os = "linux", target_os = "android")))]
use rsbinder::StatusCode;
use rsbinder::{BinderResult, ExceptionCode, SIBinder, Status};

use crate::connect::status_of;
use crate::{layout, Payload, Subscriber, SubscriberPolicy};

/// The android-13+ wire version [`subscribe`] offers: its incoming connections are what death
/// notices arrive on, and only that wire has them.
pub(crate) const WIRE: u32 = rsbinder::entry::WireVersion::MAX.get();

/// The server's default handshake deadline: both ends wait as long for one connect.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a publisher listens.
#[derive(Debug)]
#[non_exhaustive]
pub enum Endpoint {
    /// A socket at a path of the file system: its mode bits and those of the directories above
    /// it say who may connect.
    Path(PathBuf),
    /// A Linux abstract socket: no file, so **any process of the network namespace can connect**.
    /// Prefer [`Path`](Self::Path) unless the publisher's [`Authorize`](crate::Authorize) checks
    /// its callers.
    Abstract(Vec<u8>),
    /// An `O_PATH` descriptor of the socket's file, as a registry hands them out: the file may
    /// be unlinked. It needs `/proc` mounted ([`RpcClientConfig::unix_fd`]). Linux only.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Fd(OwnedFd),
}

/// How [`subscribe_with`] connects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The bound on each step of connecting and on the subscription call. Default 10 s. `None`
    /// waits as long as the publisher takes.
    ///
    /// The session keeps no deadline once the subscription is made: on a session, one that runs
    /// out ends it, and its subscriber would read the end of a slow callback as
    /// [`Closed::PublisherDied`](crate::Closed::PublisherDied).
    ///
    /// A connect into a listener whose accept queue is full is out of its reach: Linux has it
    /// wait in `connect(2)` until the publisher accepts or ends. The steps after the connect are
    /// bounded, so a stopped publisher with room in its queue is found out.
    pub timeout: Option<Duration>,
    /// The uid the process at the other end has to be, by `SO_PEERCRED`: whoever called `listen`
    /// on the socket. A publisher of any other uid is refused with `EX_SECURITY`, before the
    /// session is used. Default `None`: any uid. Kernel binder has no such check.
    ///
    /// With a socket at a path, this is the one way to know who listens there; with a registry,
    /// a second check for a registry that was set up wrong.
    pub expect_uid: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timeout: Some(DEFAULT_TIMEOUT),
            expect_uid: None,
        }
    }
}

/// Closes its session when dropped: the threads of its incoming connections hold it open
/// otherwise, whatever else lets go of it.
pub(crate) struct SessionGuard(pub(crate) RpcSession, AtomicBool);

impl SessionGuard {
    pub(crate) fn new(session: RpcSession) -> Self {
        Self(session, AtomicBool::new(false))
    }

    /// Closed because its publisher refused this subscriber, for good while it has the name.
    pub(crate) fn was_refused(&self) -> bool {
        self.1.load(Ordering::Relaxed)
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.close_session();
    }
}

/// Close a refusing publisher's `session` now if a [`SessionGuard`]; another's is left alone.
pub(crate) fn close_refused(session: &(dyn std::any::Any + Send + Sync)) {
    if let Some(guard) = session.downcast_ref::<SessionGuard>() {
        guard.1.store(true, Ordering::Relaxed);
        guard.0.close_session();
    }
}

impl std::fmt::Debug for SessionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionGuard").finish_non_exhaustive()
    }
}

fn config(endpoint: Endpoint) -> BinderResult<RpcClientConfig> {
    match endpoint {
        Endpoint::Path(path) => Ok(RpcClientConfig::unix(path, WIRE)),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Endpoint::Abstract(name) => Ok(RpcClientConfig::unix_abstract(name, WIRE)),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Endpoint::Fd(fd) => Ok(RpcClientConfig::unix_fd(fd, WIRE)),
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        Endpoint::Abstract(_) => Err(StatusCode::InvalidOperation.into()),
    }
}

/// A session to `endpoint` that passes fds and hears of its peer's end, closed when dropped.
pub(crate) fn connect(endpoint: Endpoint, options: &Options) -> BinderResult<SessionGuard> {
    let mut config = config(endpoint)?
        .fd_mode(FileDescriptorTransportMode::Unix)
        .incoming_connections(1);
    if let Some(timeout) = options.timeout {
        config = config.timeout(timeout);
    }
    let session = SessionGuard::new(RpcSession::setup_client_android13plus_with_config(config)?);
    check_uid(&session.0, options.expect_uid)?;
    Ok(session)
}

fn check_uid(session: &RpcSession, expected: Option<u32>) -> BinderResult<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    match session.peer_identity() {
        PeerIdentity::Local { uid, .. } if uid == expected => Ok(()),
        other => {
            let why = format!("the publisher is {other:?}, not of uid {expected}");
            Err(Status::from((ExceptionCode::Security, why.as_str())))
        }
    }
}

/// Subscribe to the topic of the publisher at `endpoint`, with [`Options`]' defaults.
///
/// The returned [`Subscriber`] holds the session and closes it as it drops, so nothing is left
/// to end. Its errors are those of [`Subscriber::request`] and of connecting: the code of the
/// kernel's answer (`NameNotFound` for no file, `Errno(-ECONNREFUSED)` for no listener,
/// `Errno(-EACCES)` for the socket file's mode) and `TimedOut` past [`Options::timeout`].
pub fn subscribe<T: Payload>(
    endpoint: Endpoint,
    policy: &SubscriberPolicy,
) -> BinderResult<Subscriber<T>> {
    subscribe_with(endpoint, policy, &Options::default())
}

/// [`subscribe`] with `options`.
pub fn subscribe_with<T: Payload>(
    endpoint: Endpoint,
    policy: &SubscriberPolicy,
    options: &Options,
) -> BinderResult<Subscriber<T>> {
    layout::check_target().map_err(status_of)?;
    let session = connect(endpoint, options)?;
    let subscriber = Subscriber::request(&session.0.get_root()?, policy)?;
    session.0.set_reply_timeout(None)?;
    Ok(subscriber.with_session(Arc::new(session)))
}

/// Serve `topic`, an `ITopic` binder of this process such as
/// [`Publisher::as_binder`](crate::Publisher::as_binder), on `server`: the session settings a
/// subscriber needs, then the accept loop.
///
/// Dropping the returned handle ends the server and every subscriber's session, which they read
/// as [`Closed::PublisherDied`](crate::Closed::PublisherDied) unless the topic wrote its end
/// record first: end the [`Publisher`](crate::Publisher) before the handle.
///
/// `InvalidOperation` when `topic` is an RPC binder, or the server has already started.
pub fn serve(server: &Arc<RpcServer>, topic: SIBinder) -> BinderResult<RpcServerHandle> {
    server.set_root(topic)?;
    configure(server);
    Ok(server.run_background()?)
}

/// The settings of a server a subscriber can use: the wire with incoming connections, and fds.
pub(crate) fn configure(server: &RpcServer) {
    server.set_android13plus(WIRE);
    server.set_supported_fd_modes(&[FileDescriptorTransportMode::Unix]);
}
