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
    /// RPC (android13plus profile, `unix`/`unix-abstract`): number of
    /// outgoing connections to open (AOSP `setupClient` fan-out).
    pub outgoing_connections: Option<u32>,
    /// RPC, android-13+ profile, Unix sockets only: number of incoming
    /// (callback) connections to open — AOSP `setMaxIncomingThreads`.
    /// Needed for the server to call this client's callbacks from
    /// outside a handler.
    ///
    /// Setting this `> 0` makes the resulting [`Client`] one that must be
    /// shut down explicitly (`client.session().unwrap().shutdown()`):
    /// the serving threads keep the session alive, so dropping every
    /// handle reclaims nothing. It also makes the loss of the last such
    /// connection this session's death — including a connection the
    /// server retires on its own reply timeout, which closes the
    /// founding connection and fires `binder_died` on every proxy while
    /// the peer is still up. See
    // The target only exists with `rpc`, so only link it then.
    #[cfg_attr(
        feature = "rpc",
        doc = "[`RpcUnixClientConfig::incoming_connections`](crate::rpc::RpcUnixClientConfig::incoming_connections)."
    )]
    #[cfg_attr(
        not(feature = "rpc"),
        doc = "`RpcUnixClientConfig::incoming_connections` (`rpc` feature)."
    )]
    pub incoming_connections: Option<u32>,
    /// RPC: FD transport mode to negotiate. Requesting
    /// [`FileDescriptorTransportMode::Unix`](crate::rpc::FileDescriptorTransportMode)
    /// is rejected on a transport that cannot carry fds — see
    /// [`Endpoint::supports_fd_passing`].
    #[cfg(feature = "rpc")]
    pub fd_mode: Option<crate::rpc::FileDescriptorTransportMode>,
    /// RPC: reply deadline (`RpcSession::set_timeout`). Applied to the
    /// session as soon as it exists, so besides the calls made
    /// afterwards it bounds the round trips `open` makes *after* that
    /// point: the r34 fd-mode negotiation, and the `GET_MAX_THREADS` /
    /// `GET_SESSION_ID` exchanges a multi-connection setup needs.
    ///
    /// It does **not** bound the connection handshake, which runs before
    /// the session exists — use
    // The target only exists with `rpc`, so only link it then.
    #[cfg_attr(
        feature = "rpc",
        doc = "[`handshake_timeout`](Self::handshake_timeout) for that phase."
    )]
    #[cfg_attr(
        not(feature = "rpc"),
        doc = "`handshake_timeout` (`rpc` feature) for that phase."
    )]
    pub timeout: Option<Duration>,
    /// RPC: deadline for the connection **handshake** — the phase
    /// [`timeout`](Self::timeout) cannot reach, because it runs before the
    /// session exists. Covers the `tls://` `connect(2)` and the android-13+
    /// session handshake, on the founding connection and on every fan-out
    /// or incoming attach.
    ///
    /// `None` (default) blocks forever, so a peer that accepts the socket
    /// and then writes nothing hangs `open`. Set it whenever the peer is
    /// untrusted or merely unreliable. This is the client-side counterpart
    /// of [`ServeOptions::handshake_timeout`](super::ServeOptions::handshake_timeout).
    ///
    /// `Some(Duration::ZERO)` is not a deadline and `open` refuses it with
    /// [`StatusCode::BadValue`](crate::StatusCode::BadValue) rather than
    /// silently dropping the bound; use `None` to wait indefinitely on
    /// purpose.
    #[cfg(feature = "rpc")]
    pub handshake_timeout: Option<Duration>,
    /// Kernel: `?driver=` equivalent.
    pub driver: Option<std::path::PathBuf>,
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
/// exits. Call `client.session().unwrap().shutdown()` when you are done
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
            .finish()
    }
}

enum Inner {
    Kernel,
    #[cfg(feature = "rpc")]
    Rpc(crate::rpc::RpcSession),
}

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
        Endpoint::Kernel { driver, threads } => {
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
            if o.fd_mode.is_some() {
                return Err(reject("fd_mode"));
            }
            #[cfg(feature = "rpc-tls")]
            if o.tls.is_some() || o.tls_server_name.is_some() {
                return Err(reject("tls/tls_server_name"));
            }
            let driver = o.driver.as_deref().or(driver.as_deref());
            super::server::kernel_init(driver, *threads)?;
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
            if o.driver.is_some() {
                return Err(reject("driver"));
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
fn rpc_connect(uri: &Uri, o: &ClientOptions) -> Result<crate::rpc::RpcSession> {
    #[cfg(feature = "rpc-tls")]
    let reject_option = |what: &str, endpoint: &Endpoint| {
        log::error!("rsbinder::Client::open: option `{what}` does not apply to {endpoint:?}");
        StatusCode::BadValue
    };
    use crate::rpc::transport::RpcTransport;
    use crate::rpc::{AddressSpace, FileDescriptorTransportMode, RpcSession};

    // Before any connect: this value reaches a read deadline on every
    // RPC endpoint and `TcpStream::connect_timeout` on `tls://`, and
    // both reject a zero duration — refuse it here, where the option
    // that carries it can still be named.
    crate::rpc::session::reject_zero_handshake_timeout(
        o.handshake_timeout,
        "ClientOptions::handshake_timeout",
    )?;
    let versioned = uri.wire_max_version;
    let fan_out = o.outgoing_connections.unwrap_or(1).max(1);
    let incoming = o.incoming_connections.unwrap_or(0);
    // Gate on `is_some()`, not on the value: `Some(0)`/`Some(1)` is still
    // the caller asking for an option this endpoint may not have, and
    // `ClientOptions` promises such an option is `BadValue`, never ignored.
    let multi_conn = o.outgoing_connections.is_some() || o.incoming_connections.is_some();
    if versioned.is_none() && (o.session_id.is_some() || multi_conn) {
        log::error!(
            "rsbinder::Client::open: session_id/outgoing_connections/incoming_connections \
             need `?profile=android13plus` ({:?})",
            uri.endpoint
        );
        return Err(StatusCode::BadValue);
    }

    // The unix fan-out / incoming-connection path has its own
    // multi-connection setup.
    if let (Some(v), true) = (versioned, multi_conn) {
        let mut cfg = match &uri.endpoint {
            Endpoint::Unix(path) => crate::rpc::RpcUnixClientConfig::path(path, v),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Endpoint::UnixAbstract(name) => {
                crate::rpc::RpcUnixClientConfig::abstract_name(name.as_slice(), v)
            }
            _ => {
                log::error!(
                    "rsbinder::Client::open: outgoing_connections > 1 / incoming_connections \
                     are unix-only"
                );
                return Err(StatusCode::BadValue);
            }
        }
        .outgoing_connections(fan_out)
        .incoming_connections(incoming);
        if let Some(m) = o.fd_mode {
            cfg = cfg.fd_mode(m);
        }
        if let Some(t) = o.timeout {
            cfg = cfg.timeout(t);
        }
        if let Some(t) = o.handshake_timeout {
            cfg = cfg.handshake_timeout(t);
        }
        // Forward rather than drop: the session layer refuses the
        // combination (`BadValue`), which is the "never ignored" contract.
        if let Some(id) = o.session_id.as_deref() {
            cfg = cfg.session_id(id);
        }
        #[cfg(feature = "rpc-tls")]
        if o.tls.is_some() || o.tls_server_name.is_some() {
            return Err(reject_option("tls/tls_server_name", &uri.endpoint));
        }
        return RpcSession::setup_unix_client_android13plus_with_config(cfg);
    }

    // `ClientOptions::tls` is honored only for `tls://`; every other
    // endpoint would otherwise connect in plaintext while the caller
    // believes the link is encrypted.
    #[cfg(feature = "rpc-tls")]
    if !matches!(uri.endpoint, Endpoint::Tls(..))
        && (o.tls.is_some() || o.tls_server_name.is_some())
    {
        return Err(reject_option("tls/tls_server_name", &uri.endpoint));
    }

    let transport: Box<dyn RpcTransport> = match &uri.endpoint {
        Endpoint::Kernel { .. } => unreachable!("kernel handled by caller"),
        Endpoint::Unix(path) => Box::new(crate::rpc::transport::UnixTransport::connect(path)?),
        Endpoint::UnixAbstract(name) => {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                Box::new(crate::rpc::transport::UnixTransport::connect_abstract(
                    name,
                )?)
            }
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            {
                let _ = name;
                log::error!("rsbinder::Client::open: abstract Unix sockets are Linux/Android only");
                return Err(StatusCode::InvalidOperation);
            }
        }
        Endpoint::Vsock(cid, port) => {
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            {
                Box::new(crate::rpc::transport::VsockTransport::connect(*cid, *port)?)
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
                return Err(StatusCode::InvalidOperation);
            }
        }
        Endpoint::Tls(host, port) => {
            #[cfg(feature = "rpc-tls")]
            {
                let cfg = o.tls.clone().ok_or_else(|| {
                    log::error!("rsbinder::Client::open: `tls://` requires `ClientOptions::tls`");
                    StatusCode::BadValue
                })?;
                let name = o.tls_server_name.as_deref().unwrap_or(host);
                let tcp = match o.handshake_timeout {
                    // `connect_timeout` takes one resolved address, so try
                    // each until one connects — what `TcpStream::connect`
                    // does internally for a `(host, port)` pair.
                    Some(d) => {
                        use std::net::ToSocketAddrs;
                        let mut last = None;
                        let mut sock = None;
                        for addr in (host.as_str(), *port).to_socket_addrs()? {
                            match std::net::TcpStream::connect_timeout(&addr, d) {
                                Ok(t) => {
                                    sock = Some(t);
                                    break;
                                }
                                Err(e) => last = Some(e),
                            }
                        }
                        match sock {
                            Some(t) => t,
                            None => {
                                return Err(StatusCode::from(last.unwrap_or_else(|| {
                                    std::io::Error::new(
                                        std::io::ErrorKind::NotFound,
                                        "no address resolved",
                                    )
                                })))
                            }
                        }
                    }
                    None => std::net::TcpStream::connect((host.as_str(), *port))?,
                };
                Box::new(crate::rpc::transport::TlsTransport::connect(
                    tcp, name, cfg,
                )?)
            }
            #[cfg(not(feature = "rpc-tls"))]
            {
                let _ = (host, port);
                log::error!("rsbinder::Client::open: `tls://` needs the `rpc-tls` feature");
                return Err(StatusCode::InvalidOperation);
            }
        }
    };
    match versioned {
        None => {
            let session =
                RpcSession::new(transport, AddressSpace::Initiator).map_err(StatusCode::from)?;
            // Before the negotiation below, not after `rpc_connect`
            // returns: that transaction reads this value when it runs.
            session.set_timeout(o.timeout);
            // r34 wire: FD mode is negotiated by a special transaction
            // after connect (versioned profiles do it in the handshake).
            if let Some(mode) = o.fd_mode {
                session.negotiate_fd_transport(mode)?;
            }
            Ok(session)
        }
        Some(v) => {
            let session = RpcSession::connect_android13plus_fd_with_id_hs(
                transport,
                v,
                o.fd_mode.unwrap_or(FileDescriptorTransportMode::None),
                o.session_id.as_deref().unwrap_or(&[]),
                o.handshake_timeout,
            )?;
            session.set_timeout(o.timeout);
            Ok(session)
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
            Inner::Kernel => crate::hub::wait_for_service(name).ok_or(StatusCode::NameNotFound),
            #[cfg(feature = "rpc")]
            Inner::Rpc(s) => s.get_service(name),
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
