// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use super::uri::{Endpoint, Uri};
use crate::error::{Result, StatusCode};
use crate::{FromIBinder, SIBinder, Strong};

/// Options that do not fit in the URI. Set through
/// [`Client::open_with`]. An option that does not apply to the
/// transport is [`StatusCode::BadValue`] (logged), never ignored.
#[derive(Default)]
#[non_exhaustive]
pub struct ClientOptions {
    /// `tls://`: TLS client config (required) and the server name to
    /// verify (defaults to the URI host).
    #[cfg(feature = "rpc-tls")]
    pub tls: Option<std::sync::Arc<crate::rpc::rustls::ClientConfig>>,
    #[cfg(feature = "rpc-tls")]
    pub tls_server_name: Option<String>,
    /// RPC (android13plus profile): join an existing server session by
    /// its 32-byte id instead of opening a new one.
    pub session_id: Option<Vec<u8>>,
    /// RPC (android13plus profile, every RPC transport): number of
    /// outgoing connections to open (AOSP `setupClient` fan-out).
    pub outgoing_connections: Option<u32>,
    /// RPC (android13plus profile, every RPC transport): number of
    /// incoming (callback) connections to open — AOSP
    /// `setMaxIncomingThreads`. Needed for the server to call this
    /// client's callbacks from outside a handler, and for any oneway
    /// callback. Each one is a further
    /// connection to the same endpoint (a `tls://` one is its own TLS
    /// session), because the android-13+ wire lets the server start a
    /// call only on a connection the client reads outside its own reply
    /// wait.
    ///
    /// Setting this `> 0` makes the resulting [`Client`] one that must be
    /// shut down explicitly (`client.session().unwrap().close_session()`):
    /// the serving threads keep the session alive, so dropping every
    /// handle reclaims nothing. Like every connection of the session, the
    /// loss of one ends the whole session — including the server's own
    /// reply timeout elapsing on a slow callback handler, which closes the
    /// founding connection and fires `binder_died` on every proxy although
    /// the handler was only slow. See
    // The target only exists with `rpc`, so only link it then.
    #[cfg_attr(
        feature = "rpc",
        doc = "[`RpcClientConfig::incoming_connections`](crate::rpc::RpcClientConfig::incoming_connections)."
    )]
    #[cfg_attr(
        not(feature = "rpc"),
        doc = "`RpcClientConfig::incoming_connections` (`rpc` feature)."
    )]
    pub incoming_connections: Option<u32>,
    /// RPC: FD transport mode to negotiate. Requesting
    /// [`FileDescriptorTransportMode::Unix`](crate::rpc::FileDescriptorTransportMode)
    /// is rejected on a transport that cannot carry fds — see
    /// [`Endpoint::supports_fd_passing`].
    #[cfg(feature = "rpc")]
    pub fd_mode: Option<crate::rpc::FileDescriptorTransportMode>,
    /// RPC: how long the server may go without answering before this end
    /// counts it as broken (`RpcSession::set_timeout`, plan 2-24 D4 and D6).
    ///
    /// - **Connecting**: it bounds each blocking step of connecting — one
    ///   `connect(2)` per address the founding connection tries (`tls://`),
    ///   the TLS handshake, then each read and write of the
    ///   android-13+ handshake — on the founding connection and on every
    ///   fan-out or incoming attach. It is not a budget for the phase as a
    ///   whole, so `open` can take a multiple of it before returning; what
    ///   it guarantees is that no single step waits on a silent peer
    ///   forever. Two steps are out of its reach: resolving the host name,
    ///   which blocks in the platform's resolver, and on Linux and Android a
    ///   `unix://` or `unix-abstract://` connect into a listener whose
    ///   accept queue is full, which waits until the server accepts.
    /// - **The session**: applied as soon as the session exists, so it also
    ///   bounds the round trips `open` makes after that point (the r34
    ///   fd-mode negotiation, the `GET_MAX_THREADS` / `GET_SESSION_ID`
    ///   exchanges a multi-connection setup needs), and then every reply
    ///   wait, send and liveness check. An expired reply wait ends the
    ///   session.
    ///
    /// `None` (default) waits forever, so a peer that accepts the socket
    /// and then writes nothing hangs `open`. Set it whenever the peer is
    /// untrusted or merely unreliable. `Some(Duration::ZERO)` is no
    /// deadline either, as `RpcSession::set_timeout` treats it; unlike the
    /// deprecated `handshake_timeout` (`rpc` feature), `open` does not
    /// refuse it.
    pub timeout: Option<Duration>,
    /// RPC: deadline for the connection **handshake**, in place of
    /// [`timeout`](Self::timeout) for that phase. The r34 wire (no
    /// `?profile=`) has no handshake, so on a plain r34 endpoint other than
    /// `tls://` `open` refuses it with
    /// [`StatusCode::BadValue`](crate::StatusCode::BadValue), and it
    /// refuses `Some(Duration::ZERO)` the same way.
    #[cfg(feature = "rpc")]
    #[deprecated(
        since = "0.12.0",
        note = "`timeout` bounds each connect and handshake step too; set it instead (plan 2-24 D6)"
    )]
    pub handshake_timeout: Option<Duration>,
    /// Kernel: `?driver=` equivalent. The device is fixed process-wide by
    /// whoever initializes `ProcessState` first, so a *different* path here
    /// is [`StatusCode::BadValue`](crate::StatusCode::BadValue) at
    /// [`open`](Client::open) — see
    /// [`ServeOptions::threads`](super::ServeOptions::threads) for the same
    /// rule on the server side.
    pub driver: Option<std::path::PathBuf>,
    /// Kernel: `?mmap=` equivalent — the size of the mapping this
    /// process receives into. A client is a receiver too: the reply to
    /// every call it makes is allocated out of *its* mapping, so a
    /// client expecting replies larger than the ~1 MB default raises it
    /// here. Unlike [`ServeOptions::mmap_size`](super::ServeOptions::mmap_size),
    /// this one takes effect — [`open`](Client::open) reads it before it
    /// initializes `ProcessState`. A *different* size than the one
    /// already in force is
    /// [`StatusCode::BadValue`](crate::StatusCode::BadValue), as with
    /// [`driver`](Self::driver).
    pub mmap_size: Option<usize>,
}

/// A resolver for named services on one endpoint: the system service
/// manager (`binder://`) or one RPC session.
///
/// Proxies returned by [`get`](Self::get) stay valid after the `Client`
/// is dropped — an RPC proxy holds its session; keep the `Client` only
/// to issue more lookups on the same session. The kernel form also
/// starts the binder thread pool so callbacks, death notifications and
/// registration waits are delivered promptly.
///
/// **One exception to "just drop it".** A client opened with
/// [`ClientOptions::incoming_connections`] `> 0` owns the threads
/// serving those connections, and each of them holds the session alive:
/// dropping the `Client` *and* every proxy reclaims nothing, so the
/// threads, the session and both ends' sockets survive until the process
/// exits. Call `client.session().unwrap().close_session()` when you are done
/// with such a client.
pub struct Client {
    endpoint: Endpoint,
    inner: Inner,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Client");
        match &self.inner {
            Inner::Kernel => d.field("transport", &"kernel"),
            #[cfg(feature = "rpc")]
            Inner::Rpc(_) => d.field("transport", &"rpc"),
        };
        d.finish()
    }
}

impl std::fmt::Debug for ClientOptions {
    #[allow(deprecated)] // Shows `handshake_timeout` while it is still honored.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("ClientOptions");
        #[cfg(feature = "rpc-tls")]
        d.field("tls", &self.tls.is_some())
            .field("tls_server_name", &self.tls_server_name);
        d.field("session_id", &self.session_id.as_ref().map(|v| v.len()))
            .field("outgoing_connections", &self.outgoing_connections)
            .field("incoming_connections", &self.incoming_connections);
        #[cfg(feature = "rpc")]
        d.field("fd_mode", &self.fd_mode)
            .field("handshake_timeout", &self.handshake_timeout);
        d.field("timeout", &self.timeout)
            .field("driver", &self.driver)
            .field("mmap_size", &self.mmap_size)
            .finish()
    }
}

enum Inner {
    Kernel,
    #[cfg(feature = "rpc")]
    Rpc(crate::rpc::RpcSession),
}

/// Merges a [`ClientOptions`] field with its URI key; differing values are `BadValue`.
fn one_source<T: PartialEq + std::fmt::Debug>(
    what: &str,
    from_option: Option<T>,
    from_uri: Option<T>,
) -> Result<Option<T>> {
    match (from_option, from_uri) {
        (Some(o), Some(u)) if o != u => {
            log::error!(
                "rsbinder::Client::open: ClientOptions::{what}={o:?} conflicts with the URI's \
                 {u:?} — give the value once"
            );
            Err(StatusCode::BadValue)
        }
        (o, u) => Ok(o.or(u)),
    }
}

#[allow(deprecated)] // Refuses `handshake_timeout` where it does not apply, while it is honored.
pub(super) fn new_client(uri: Uri, o: ClientOptions) -> Result<Client> {
    if uri.service.is_some() {
        log::error!(
            "rsbinder::Client::open: a `#service` fragment is not allowed here (use connect)"
        );
        return Err(StatusCode::BadValue);
    }
    let reject = |what: &str| {
        log::error!(
            "rsbinder::Client::open: option `{what}` does not apply to {:?}",
            uri.endpoint
        );
        StatusCode::BadValue
    };
    match &uri.endpoint {
        Endpoint::Kernel {
            driver,
            threads,
            mmap_size,
        } => {
            if o.session_id.is_some()
                || o.outgoing_connections.is_some()
                || o.incoming_connections.is_some()
                || o.timeout.is_some()
            {
                return Err(reject(
                    "session_id/outgoing_connections/incoming_connections/timeout",
                ));
            }
            #[cfg(feature = "rpc")]
            if o.fd_mode.is_some() || o.handshake_timeout.is_some() {
                return Err(reject("fd_mode/handshake_timeout"));
            }
            #[cfg(feature = "rpc-tls")]
            if o.tls.is_some() || o.tls_server_name.is_some() {
                return Err(reject("tls/tls_server_name"));
            }
            let driver = one_source("driver", o.driver.as_deref(), driver.as_deref())?;
            let mmap_size = one_source("mmap_size", o.mmap_size, *mmap_size)?;
            super::server::kernel_init(driver, *threads, mmap_size)?;
            crate::ProcessState::start_thread_pool();
            Ok(Client {
                endpoint: uri.endpoint.clone(),
                inner: Inner::Kernel,
            })
        }
        #[cfg(not(feature = "rpc"))]
        _ => {
            log::error!(
                "rsbinder::Client::open: {:?} needs the `rpc` feature",
                uri.endpoint
            );
            Err(StatusCode::InvalidOperation)
        }
        #[cfg(feature = "rpc")]
        _ => {
            if o.driver.is_some() || o.mmap_size.is_some() {
                return Err(reject("driver/mmap_size"));
            }
            if o.fd_mode == Some(crate::rpc::FileDescriptorTransportMode::Unix)
                && !uri.endpoint.supports_fd_passing()
            {
                log::error!(
                    "rsbinder::Client::open: option `fd_mode` cannot request Unix fd passing \
                     on {:?} — only Unix-domain sockets carry SCM_RIGHTS",
                    uri.endpoint
                );
                return Err(StatusCode::BadValue);
            }
            let session = rpc_connect(&uri, &o)?;
            Ok(Client {
                endpoint: uri.endpoint.clone(),
                inner: Inner::Rpc(session),
            })
        }
    }
}

#[cfg(feature = "rpc")]
#[allow(deprecated)] // Forwards `handshake_timeout` to the config's own deprecated setter.
fn rpc_connect(uri: &Uri, o: &ClientOptions) -> Result<crate::rpc::RpcSession> {
    #[cfg(feature = "rpc-tls")]
    let reject_option = |what: &str, endpoint: &Endpoint| {
        log::error!("rsbinder::Client::open: option `{what}` does not apply to {endpoint:?}");
        StatusCode::BadValue
    };
    use crate::rpc::{AddressSpace, RpcSession};

    // Refuse zero here, where the option can be named; its downstream consumers reject it too.
    crate::rpc::session::reject_zero_handshake_timeout(
        o.handshake_timeout,
        "ClientOptions::handshake_timeout",
    )?;
    let versioned = uri.wire_max_version;
    let fan_out = o.outgoing_connections.unwrap_or(1).max(1);
    let incoming = o.incoming_connections.unwrap_or(0);
    // `is_some()`, not the value: a set option this endpoint lacks is `BadValue`, never ignored.
    let multi_conn = o.outgoing_connections.is_some() || o.incoming_connections.is_some();
    if versioned.is_none() && (o.session_id.is_some() || multi_conn) {
        log::error!(
            "rsbinder::Client::open: session_id/outgoing_connections/incoming_connections \
             need `?profile=android13plus` ({:?})",
            uri.endpoint
        );
        return Err(StatusCode::BadValue);
    }
    // r34 has no handshake to bound (see `ClientOptions::handshake_timeout`); `tls://` does.
    if versioned.is_none()
        && o.handshake_timeout.is_some()
        && !matches!(uri.endpoint, Endpoint::Tls(..))
    {
        log::error!(
            "rsbinder::Client::open: handshake_timeout needs `?profile=android13plus` \
             (or a `tls://` endpoint) ({:?})",
            uri.endpoint
        );
        return Err(StatusCode::BadValue);
    }

    // TLS only on `tls://`: elsewhere it would connect in plaintext the caller thinks encrypted.
    #[cfg(feature = "rpc-tls")]
    if !matches!(uri.endpoint, Endpoint::Tls(..))
        && (o.tls.is_some() || o.tls_server_name.is_some())
    {
        return Err(reject_option("tls/tls_server_name", &uri.endpoint));
    }

    let mut cfg = client_config(&uri.endpoint, o, versioned.unwrap_or(0))?;
    if let Some(m) = o.fd_mode {
        cfg = cfg.fd_mode(m);
    }
    if let Some(t) = o.timeout {
        cfg = cfg.timeout(t);
    }
    if let Some(t) = o.handshake_timeout {
        cfg = cfg.handshake_timeout(t);
    }
    // Forwarded, not dropped: the session layer refuses what it cannot honor (never ignored).
    if let Some(id) = o.session_id.as_deref() {
        cfg = cfg.session_id(id);
    }

    if versioned.is_some() {
        // One connection each, as AOSP `setupClient` calls `connectAndInit`, on every transport.
        return RpcSession::setup_client_android13plus_with_config(
            cfg.outgoing_connections(fan_out)
                .incoming_connections(incoming),
        );
    }

    // r34 wire: no handshake, so the session is built on the connection itself.
    let session =
        RpcSession::new(cfg.connect_once()?, AddressSpace::Initiator).map_err(StatusCode::from)?;
    // Before the negotiation below: that transaction reads this value when it runs.
    session.set_timeout(o.timeout);
    // r34 negotiates the FD mode by a transaction after connect (versioned: in the handshake).
    if let Some(mode) = o.fd_mode {
        session.negotiate_fd_transport(mode)?;
    }
    Ok(session)
}

/// Session config offering `max_version` (ignored on r34); refuses what the build lacks early.
#[cfg(feature = "rpc")]
fn client_config<'a>(
    endpoint: &'a Endpoint,
    o: &'a ClientOptions,
    max_version: u32,
) -> Result<crate::rpc::RpcClientConfig<'a>> {
    use crate::rpc::RpcClientConfig;
    match endpoint {
        Endpoint::Kernel { .. } => unreachable!("kernel handled by caller"),
        Endpoint::Unix(path) => Ok(RpcClientConfig::unix(path, max_version)),
        Endpoint::UnixAbstract(name) => {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                Ok(RpcClientConfig::unix_abstract(name, max_version))
            }
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            {
                let _ = name;
                log::error!("rsbinder::Client::open: abstract Unix sockets are Linux/Android only");
                Err(StatusCode::InvalidOperation)
            }
        }
        Endpoint::Vsock(cid, port) => {
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            {
                Ok(RpcClientConfig::vsock(*cid, *port, max_version))
            }
            #[cfg(not(all(
                feature = "rpc-vsock",
                any(target_os = "linux", target_os = "android")
            )))]
            {
                let _ = (cid, port);
                log::error!(
                    "rsbinder::Client::open: vsock needs the `rpc-vsock` feature (Linux/Android)"
                );
                Err(StatusCode::InvalidOperation)
            }
        }
        Endpoint::Tls(host, port) => {
            #[cfg(feature = "rpc-tls")]
            {
                let tls = o.tls.clone().ok_or_else(|| {
                    log::error!("rsbinder::Client::open: `tls://` requires `ClientOptions::tls`");
                    StatusCode::BadValue
                })?;
                let name = o.tls_server_name.as_deref().unwrap_or(host);
                Ok(RpcClientConfig::tls(host, *port, name, tls, max_version))
            }
            #[cfg(not(feature = "rpc-tls"))]
            {
                let _ = (host, port, o);
                log::error!("rsbinder::Client::open: `tls://` needs the `rpc-tls` feature");
                Err(StatusCode::InvalidOperation)
            }
        }
    }
}

impl Client {
    /// Open a resolver on `uri` (no `#service`). See [`ClientOptions`]
    /// for what cannot be expressed in the URI.
    pub fn open(uri: &str) -> Result<Client> {
        new_client(super::uri::parse(uri)?, ClientOptions::default())
    }

    /// [`open`](Self::open) with [`ClientOptions`]. The closure also sees
    /// the parsed [`Endpoint`], so an option that applies to only some
    /// transports can be set conditionally without re-parsing or
    /// string-matching the URI.
    pub fn open_with(uri: &str, f: impl FnOnce(&mut ClientOptions, &Endpoint)) -> Result<Client> {
        let parsed = super::uri::parse(uri)?;
        let mut o = ClientOptions::default();
        f(&mut o, &parsed.endpoint);
        new_client(parsed, o)
    }

    /// The endpoint this client is connected to. Mirrors
    /// [`Server::endpoint`](super::Server::endpoint).
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Resolve `name` and cast it to `T`. Kernel: blocks until the
    /// service is registered (AOSP `waitForService`); RPC: one lookup in
    /// the server's directory ([`StatusCode::NameNotFound`] if absent).
    pub fn get<T: FromIBinder + ?Sized>(&self, name: &str) -> Result<Strong<T>> {
        FromIBinder::try_from(self.binder(name)?)
    }

    /// Non-waiting form of [`get`](Self::get): `Ok(None)` when `name` is
    /// not (yet) registered.
    pub fn try_get<T: FromIBinder + ?Sized>(&self, name: &str) -> Result<Option<Strong<T>>> {
        match &self.inner {
            Inner::Kernel => crate::hub::try_get_interface(name),
            #[cfg(feature = "rpc")]
            Inner::Rpc(s) => match s.get_service(name) {
                Ok(b) => FromIBinder::try_from(b).map(Some),
                Err(StatusCode::NameNotFound) => Ok(None),
                Err(e) => Err(e),
            },
        }
    }

    /// Resolve `name` as an untyped binder (waiting semantics of
    /// [`get`](Self::get)).
    pub fn binder(&self, name: &str) -> Result<SIBinder> {
        match &self.inner {
            // The service manager's own failure, not `NameNotFound`: the name was never looked up.
            Inner::Kernel => crate::hub::default()?
                .wait_for_service(name)
                .ok_or(StatusCode::NameNotFound),
            #[cfg(feature = "rpc")]
            Inner::Rpc(s) => s.get_service(name),
        }
    }

    /// What this client's transport can do, as a
    /// [`TransportCaps`](crate::TransportCaps) set.
    ///
    /// Kernel is always the full set. An RPC client reports what its
    // The target only exists with `rpc`, so only link it then.
    #[cfg_attr(
        feature = "rpc",
        doc = "session has *now* — see [`RpcSession::caps`](crate::rpc::RpcSession::caps)"
    )]
    #[cfg_attr(
        not(feature = "rpc"),
        doc = "session has *now* — see `RpcSession::caps` (`rpc` feature)"
    )]
    /// for why that is a snapshot, and
    /// [`Endpoint::static_caps`] for what the transport could offer at
    /// best.
    ///
    /// ```no_run
    /// # fn f() -> rsbinder::Result<()> {
    /// use rsbinder::TransportCaps;
    /// let client = rsbinder::Client::open("unix:///tmp/x.sock")?;
    /// // Fails here, naming the option to set, rather than on the first
    /// // callback.
    /// client.caps().require(TransportCaps::CALLBACKS, "event subscription")?;
    /// # Ok(()) }
    /// ```
    pub fn caps(&self) -> crate::TransportCaps {
        match &self.inner {
            Inner::Kernel => crate::TransportCaps::KERNEL,
            #[cfg(feature = "rpc")]
            Inner::Rpc(s) => s.caps(),
        }
    }

    /// The underlying RPC session (`None` for `binder://`).
    #[cfg(feature = "rpc")]
    pub fn session(&self) -> Option<&crate::rpc::RpcSession> {
        match &self.inner {
            Inner::Rpc(s) => Some(s),
            Inner::Kernel => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Option vs URI key: agreeing or single values pass; a conflict is refused, not resolved.
    #[test]
    fn a_setting_given_twice_must_agree() {
        assert_eq!(one_source::<usize>("mmap_size", None, None), Ok(None));
        assert_eq!(one_source("mmap_size", Some(8192), None), Ok(Some(8192)));
        assert_eq!(one_source("mmap_size", None, Some(8192)), Ok(Some(8192)));
        assert_eq!(
            one_source("mmap_size", Some(8192), Some(8192)),
            Ok(Some(8192))
        );
        assert_eq!(
            one_source("mmap_size", Some(8192), Some(4096)),
            Err(StatusCode::BadValue)
        );
        assert_eq!(
            one_source("driver", Some("/dev/binder"), Some("/dev/vndbinder")),
            Err(StatusCode::BadValue)
        );
    }
}
