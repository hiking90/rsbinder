// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use super::uri::{Endpoint, Uri};
use crate::error::{Result, StatusCode};
use crate::process_state::{CallRestriction, ProcessState};
use crate::SIBinder;

/// Peer-identity gate for RPC servers (see `RpcServer::set_authorizer`).
#[cfg(feature = "rpc")]
pub type Authorizer = Box<dyn Fn(&crate::rpc::PeerIdentity) -> bool + Send + Sync>;

/// Options that are not part of the [`Uri`]. Set through [`Server::with`].
/// An option that does not apply to the server's transport is reported
/// at [`Server::run`] / [`Server::spawn`] time as [`StatusCode::BadValue`]
/// (with a log line naming it) — never silently ignored. The check runs
/// before any name is registered, so a refused option leaves nothing
/// published.
#[derive(Default)]
#[non_exhaustive]
pub struct ServeOptions {
    /// RPC: `RpcServer::set_max_threads`.
    ///
    /// Kernel: **[`KernelEndpoint::with_threads`](super::KernelEndpoint::with_threads)
    /// (`binder://?threads=`) is the only form that takes effect.**
    /// [`serve`](super::serve) initializes the
    /// process-wide `ProcessState` — where the kernel pool size is
    /// fixed, once, for the life of the process — before this option
    /// can be read, so a value set here is only compared against the
    /// pool already in force, and a *different* one is
    /// [`StatusCode::BadValue`] at [`run`](Server::run) /
    /// [`spawn`](Server::spawn). A value equal to the pool in force is
    /// accepted (it asks for what is already true).
    pub threads: Option<u32>,
    /// Kernel: the size of the mapping this process receives
    /// transactions into — [`ProcessState::init_with_mmap_size`], where
    /// the range and the driver's rules are documented. Raise it on a
    /// service that must accept transactions larger than the ~1 MB
    /// default.
    ///
    /// **[`KernelEndpoint::with_mmap_size`](super::KernelEndpoint::with_mmap_size)
    /// (`binder://?mmap=`) is the only form that takes effect**, for the
    /// same reason as [`threads`](Self::threads): the mapping is made when [`serve`](super::serve) initializes
    /// `ProcessState`, before this option is read. A value set here is
    /// only compared against the mapping already in force, and a
    /// *different* one (after page rounding) is [`StatusCode::BadValue`]
    /// at [`run`](Server::run) / [`spawn`](Server::spawn).
    pub mmap_size: Option<usize>,
    /// RPC: `RpcServer::set_max_connections`.
    pub max_connections: Option<usize>,
    /// RPC: `RpcServer::set_handshake_timeout`. `None` here means
    /// "leave it alone" — the setter is not called and the server keeps
    /// its 10 s default. It is *not* the setter's own `None` (deadline
    /// deliberately disabled), which this facade cannot express.
    /// `Some(Duration::ZERO)` is passed through and refused by the
    /// setter, which keeps the 10 s default (`idle_timeout` treats a
    /// zero the other way — as `None`).
    pub handshake_timeout: Option<Duration>,
    /// RPC: `RpcServer::set_idle_timeout`. `None` here means "leave it
    /// alone"; the server's own default is already `None` (no idle
    /// deadline), so the two coincide. `Some(Duration::ZERO)` is passed
    /// through and the setter treats it as `None`.
    pub idle_timeout: Option<Duration>,
    /// RPC: `RpcServer::set_reply_timeout` — bounds the wait for a reply
    /// to a callback this server issues to a client outside a handler of
    /// that client's session (`set_reply_timeout` states which calls).
    /// The only bound on that wait, so set it on any server that issues
    /// callbacks to clients it does not control (see
    /// [`ClientOptions::incoming_connections`](super::ClientOptions::incoming_connections),
    /// the client side of that path). Its expiry ends the session with
    /// that client.
    pub reply_timeout: Option<Duration>,
    /// RPC: `RpcServer::set_authorizer` — accept/reject a peer by identity.
    #[cfg(feature = "rpc")]
    pub authorizer: Option<Authorizer>,
    /// `tls://` only, and required there: TLS server config. Refused on
    /// every other endpoint, because
    /// [`ClientOptions::tls`](super::ClientOptions::tls) is `tls://`-only
    /// too — a server this facade wrapped in TLS over a Unix or vsock
    /// socket could not be reached by a client this facade built. TLS
    /// over those sockets is still available one layer down
    /// (`RpcServer::setup_unix_server_tls` / `setup_vsock_server_tls`),
    /// with a hand-assembled `TlsTransport::connect_stream` client.
    /// Refusing rather than ignoring matters most on an abstract socket,
    /// which has no filesystem permissions, so mTLS may be the only
    /// authentication the operator configured.
    #[cfg(feature = "rpc-tls")]
    pub tls: Option<std::sync::Arc<crate::rpc::rustls::ServerConfig>>,
    /// RPC: `RpcServer::set_supported_fd_modes`. Advertising
    /// [`FileDescriptorTransportMode::Unix`](crate::rpc::FileDescriptorTransportMode)
    /// is rejected on a transport that cannot carry fds — see
    /// [`Endpoint::supports_fd_passing`].
    #[cfg(feature = "rpc")]
    pub fd_modes: Option<Vec<crate::rpc::FileDescriptorTransportMode>>,
    /// Kernel: `ProcessState::set_call_restriction`.
    pub call_restriction: Option<CallRestriction>,
}

/// A server being assembled by [`serve`](super::serve). Transport is
/// fixed by the URI; services are added with [`add`](Self::add); then
/// [`run`](Self::run) (blocking) or [`spawn`](Self::spawn).
///
/// Kernel (`binder://`): construction initializes the process-wide
/// [`ProcessState`] (idempotently — a second kernel server in the same
/// process reuses it, and is refused with [`StatusCode::BadValue`] if it
/// asked for a different driver, thread count or mapping size, which the
/// process cannot give it). RPC: the listener is bound at `run`/`spawn` so
/// [`ServeOptions`] (TLS config, limits) can be applied first.
///
/// On either transport, names given to [`add`](Self::add) are registered
/// at `run`/`spawn`, after the options are checked — so a server that
/// fails to start has published nothing it cannot serve.
#[must_use = "a Server serves nothing until `run` or `spawn` is called"]
pub struct Server {
    uri: Uri,
    options: ServeOptions,
    pending: Vec<(String, SIBinder)>,
}

/// Handle for a [`Server::spawn`]ed server. Dropping it ends an RPC
/// server: the listener is closed, every session is ended (connected
/// clients see the connection go) and the workers are joined — so a
/// drop returns whether or not clients are still attached. For the
/// kernel there is nothing to stop — the process thread pool has no
/// shutdown — so the guard is inert.
///
/// Bind it to a named variable (`let _server = ...`): `let _ = ...` and a
/// bare `spawn()?;` drop it at once, which stops an RPC server while the
/// same line keeps a kernel server running.
#[must_use = "dropping the guard stops an RPC server; bind it to a named variable"]
pub struct ServerGuard {
    #[cfg(feature = "rpc")]
    rpc: Option<(
        std::sync::Arc<crate::rpc::RpcServer>,
        std::thread::JoinHandle<()>,
    )>,
}

impl std::fmt::Debug for ServerGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(feature = "rpc")]
        let kind = if self.rpc.is_some() { "rpc" } else { "kernel" };
        #[cfg(not(feature = "rpc"))]
        let kind = "kernel";
        f.debug_struct("ServerGuard")
            .field("transport", &kind)
            .finish()
    }
}

impl ServerGuard {
    /// End the server now (RPC): stop accepting, end every session, join
    /// the threads. Same as dropping the guard. Kernel: no-op.
    pub fn stop_and_join(mut self) {
        self.stop();
    }

    /// The underlying [`crate::rpc::RpcServer`] (`None` on `binder://`) —
    /// the escape hatch for RPC-only powers the entry layer does not
    /// wrap, notably the bound address after a `:0` port
    /// (`tcp_address()` / `vsock_address()` / `path()`) and the session
    /// counters. Mirrors [`super::Client::session`].
    #[cfg(feature = "rpc")]
    pub fn server(&self) -> Option<&std::sync::Arc<crate::rpc::RpcServer>> {
        self.rpc.as_ref().map(|(s, _)| s)
    }

    fn stop(&mut self) {
        #[cfg(feature = "rpc")]
        if let Some((server, jh)) = self.rpc.take() {
            // Flag, then join, so nothing is accepted after this; then end what is connected.
            server.stop_accepting();
            if jh.join().is_err() {
                log::warn!("RPC: accept loop thread panicked");
            }
            server.terminate();
        }
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) fn new_server(uri: Uri) -> Result<Server> {
    uri.validate()?;
    if let Endpoint::Kernel(k) = &uri.endpoint {
        kernel_init(k.driver(), k.threads(), k.mmap_size())?;
    }
    #[cfg(not(feature = "rpc"))]
    if !uri.endpoint.is_kernel() {
        log::error!(
            "rsbinder::serve: {:?} needs the `rpc` feature",
            uri.endpoint
        );
        return Err(StatusCode::InvalidOperation);
    }
    Ok(Server {
        uri,
        options: ServeOptions::default(),
        pending: Vec::new(),
    })
}

/// Idempotent kernel `ProcessState` init; `None` asks for nothing, a mismatch is `BadValue`.
pub(super) fn kernel_init(
    driver: Option<&std::path::Path>,
    max_threads: Option<u32>,
    mmap_size: Option<usize>,
) -> Result<()> {
    // Normalize first: a bad size is `BadValue` (not `NoInit`) and compares as the kernel maps it.
    let mmap_size = mmap_size
        .map(ProcessState::normalized_mmap_size)
        .transpose()?;
    let driver_path: &str = match driver {
        Some(p) => p.to_str().ok_or_else(|| {
            log::error!("rsbinder: binder driver path {p:?} is not UTF-8");
            StatusCode::BadValue
        })?,
        None => ProcessState::default_driver_path(),
    };
    let ps = ProcessState::init_with_mmap_size(
        driver_path,
        max_threads.unwrap_or(crate::DEFAULT_MAX_BINDER_THREADS),
        mmap_size.unwrap_or_else(ProcessState::default_mmap_size),
    )
    .map_err(|e| {
        log::error!("rsbinder: ProcessState init failed: {e}");
        StatusCode::NoInit
    })?;
    // Check what `init` returned: a pre-init `is_initialized()` sample misses a racing init.
    let driver_mismatch = driver.is_some_and(|d| d != ps.driver_name());
    let threads_mismatch = max_threads.is_some_and(|n| n != ps.max_threads());
    let mmap_mismatch = mmap_size.is_some_and(|n| n != ps.mmap_size());
    if driver_mismatch || threads_mismatch || mmap_mismatch {
        log::error!(
            "rsbinder: ProcessState is already initialized with driver={:?} max_threads={} \
             mmap_size={}; requested driver={driver:?} max_threads={max_threads:?} \
             mmap_size={mmap_size:?} cannot be applied — initialize once, or omit the option",
            ps.driver_name(),
            ps.max_threads(),
            ps.mmap_size()
        );
        return Err(StatusCode::BadValue);
    }
    Ok(())
}

impl Server {
    /// Publish `svc` under `name`. The name is queued here and registered
    /// when the server starts: kernel — with the system service manager, by
    /// [`run`](Self::run) / [`spawn`](Self::spawn), after the options are
    /// checked and the thread pool is started; RPC — in the server's
    /// directory. So a readiness signal belongs after `spawn`, not between
    /// `add` and `run`, and a registration error (a name the service
    /// manager refuses, a policy denial) comes back from `run` / `spawn`.
    ///
    /// An RPC endpoint refuses a **remote** binder with
    /// [`StatusCode::InvalidOperation`]: a proxy cannot be re-published on a
    /// socket server, because the binder that would leave this process is the
    /// proxy itself and no stack accepts one from the other (see the
    /// [gateway section] of the book — wrap it in a `Bn*` instead:
    /// `BnFoo::new_binder(proxy)`). The kernel arm still accepts a *kernel*
    /// proxy — re-registering one with the system service manager is a
    /// legitimate use — but an RPC proxy is refused there too, by the same
    /// stack-boundary check, when the registration parcel is written at
    /// `run` / `spawn`.
    ///
    /// [gateway section]: https://hiking90.github.io/rsbinder/cross-transport-services.html
    pub fn add(mut self, name: &str, svc: impl Into<SIBinder>) -> Result<Self> {
        let binder = svc.into();
        if !self.uri.endpoint.is_kernel() && (*binder).is_remote() {
            log::error!(
                "serve(...).add({name}): refusing a remote binder on an RPC endpoint; \
                 wrap it in a local Bn* (gateway) instead"
            );
            return Err(StatusCode::InvalidOperation);
        }
        self.pending.push((name.to_string(), binder));
        Ok(self)
    }

    /// Set [`ServeOptions`]. May be called more than once; applied at
    /// [`run`](Self::run) / [`spawn`](Self::spawn).
    pub fn with(mut self, f: impl FnOnce(&mut ServeOptions)) -> Self {
        f(&mut self.options);
        self
    }

    /// The endpoint this server listens on.
    pub fn endpoint(&self) -> &Endpoint {
        &self.uri.endpoint
    }

    /// Start serving and block. Kernel: checks the options, starts the
    /// thread pool, registers the [`add`](Self::add)ed names in order, then
    /// joins the pool (never returns normally). RPC: runs the accept loop
    /// until `RpcServer::stop_accepting` is called from another thread
    /// (reachable via [`spawn`](Self::spawn)'s guard instead).
    ///
    /// Kernel: if registering a name fails, the error is returned and the
    /// names registered before it stay published — and served, since the
    /// pool is already running — until the process exits.
    pub fn run(self) -> Result<()> {
        match self.uri.endpoint {
            Endpoint::Kernel(_) => {
                self.start_kernel()?;
                ProcessState::join_thread_pool()
            }
            #[cfg(feature = "rpc")]
            _ => self.build_rpc()?.run(),
            #[cfg(not(feature = "rpc"))]
            _ => Err(StatusCode::InvalidOperation),
        }
    }

    /// Start serving in the background. Kernel: as [`run`](Self::run)
    /// without the join — every name is registered when this returns `Ok`
    /// (the returned guard is inert). RPC: accept loop on its own thread;
    /// dropping the guard shuts it down.
    pub fn spawn(self) -> Result<ServerGuard> {
        match self.uri.endpoint {
            Endpoint::Kernel(_) => {
                self.start_kernel()?;
                Ok(ServerGuard {
                    #[cfg(feature = "rpc")]
                    rpc: None,
                })
            }
            #[cfg(feature = "rpc")]
            _ => {
                let server = self.build_rpc()?;
                let jh = server.run_background();
                Ok(ServerGuard {
                    rpc: Some((server, jh)),
                })
            }
            #[cfg(not(feature = "rpc"))]
            _ => Err(StatusCode::InvalidOperation),
        }
    }

    fn reject(&self, what: &str) -> StatusCode {
        log::error!(
            "rsbinder::serve: option `{what}` does not apply to {:?}",
            self.uri.endpoint
        );
        StatusCode::BadValue
    }

    /// Pool before names: a registered name always has a looper, even when a later one fails.
    fn start_kernel(self) -> Result<()> {
        self.apply_kernel_options()?;
        ProcessState::start_thread_pool();
        for (name, binder) in self.pending {
            crate::hub::add_service(&name, binder).map_err(|e| {
                log::error!("rsbinder::serve: registering {name:?} failed: {e:?}");
                StatusCode::from(e)
            })?;
        }
        Ok(())
    }

    fn apply_kernel_options(&self) -> Result<()> {
        let o = &self.options;
        if o.max_connections.is_some()
            || o.handshake_timeout.is_some()
            || o.idle_timeout.is_some()
            || o.reply_timeout.is_some()
        {
            return Err(self.reject("max_connections/handshake_timeout/idle_timeout/reply_timeout"));
        }
        #[cfg(feature = "rpc")]
        if o.authorizer.is_some() || o.fd_modes.is_some() {
            return Err(self.reject("authorizer/fd_modes"));
        }
        #[cfg(feature = "rpc-tls")]
        if o.tls.is_some() {
            return Err(self.reject("tls"));
        }
        if o.threads.is_some() || o.mmap_size.is_some() {
            // Both are fixed at init; re-running init makes a mismatch `BadValue`, as in a URI.
            kernel_init(None, o.threads, o.mmap_size)?;
        }
        if let Some(cr) = o.call_restriction {
            ProcessState::as_self().set_call_restriction(cr);
        }
        Ok(())
    }

    #[cfg(feature = "rpc")]
    fn build_rpc(self) -> Result<std::sync::Arc<crate::rpc::RpcServer>> {
        use crate::rpc::RpcServer;
        let Server {
            uri,
            options: o,
            pending,
        } = self;
        if o.call_restriction.is_some() || o.mmap_size.is_some() {
            log::error!(
                "rsbinder::serve: option `call_restriction`/`mmap_size` does not apply to {:?}",
                uri.endpoint
            );
            return Err(StatusCode::BadValue);
        }
        #[cfg(feature = "rpc-tls")]
        let tls = o.tls.clone();
        // As `ClientOptions::tls`: no facade TLS client over unix/vsock, so refuse, never ignore.
        #[cfg(feature = "rpc-tls")]
        if tls.is_some() && !matches!(uri.endpoint, Endpoint::Tls { .. }) {
            log::error!(
                "rsbinder::serve: option `tls` does not apply to {:?} \
                 (only `Endpoint::Tls`; RpcServer::setup_unix_server_tls / \
                 setup_vsock_server_tls are the direct forms)",
                uri.endpoint
            );
            return Err(StatusCode::BadValue);
        }
        let server = match &uri.endpoint {
            Endpoint::Kernel(_) => unreachable!("kernel handled by caller"),
            Endpoint::Unix(path) => RpcServer::setup_unix_server(path.clone())?,
            Endpoint::UnixAbstract(name) => {
                #[cfg(any(target_os = "linux", target_os = "android"))]
                {
                    RpcServer::setup_unix_server_abstract(name)?
                }
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                {
                    let _ = name;
                    log::error!("rsbinder::serve: abstract Unix sockets are Linux/Android only");
                    return Err(StatusCode::InvalidOperation);
                }
            }
            Endpoint::Vsock { cid, port } => {
                #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
                {
                    RpcServer::setup_vsock_server(*cid, *port)?
                }
                #[cfg(not(all(
                    feature = "rpc-vsock",
                    any(target_os = "linux", target_os = "android")
                )))]
                {
                    let _ = (cid, port);
                    log::error!(
                        "rsbinder::serve: vsock needs the `rpc-vsock` feature (Linux/Android)"
                    );
                    return Err(StatusCode::InvalidOperation);
                }
            }
            Endpoint::Tls { host, port } => {
                #[cfg(feature = "rpc-tls")]
                {
                    let cfg = tls.ok_or_else(|| {
                        log::error!("rsbinder::serve: a TLS endpoint requires `ServeOptions::tls`");
                        StatusCode::BadValue
                    })?;
                    RpcServer::setup_tcp_server_tls((host.as_str(), *port), cfg)?
                }
                #[cfg(not(feature = "rpc-tls"))]
                {
                    let _ = (host, port);
                    log::error!("rsbinder::serve: `tls://` needs the `rpc-tls` feature");
                    return Err(StatusCode::InvalidOperation);
                }
            }
        };
        if let super::uri::WireProfile::Android13Plus(v) = uri.wire {
            server.set_android13plus(v.get());
        }
        if let Some(n) = o.threads {
            server.set_max_threads(n);
        }
        if let Some(n) = o.max_connections {
            server.set_max_connections(n);
        }
        if let Some(t) = o.handshake_timeout {
            server.set_handshake_timeout(Some(t));
        }
        if let Some(t) = o.idle_timeout {
            server.set_idle_timeout(Some(t));
        }
        // Set before `run`: it is read once per session, when the founding connection is accepted.
        if let Some(t) = o.reply_timeout {
            server.set_reply_timeout(Some(t));
        }
        if let Some(f) = o.authorizer {
            server.set_authorizer(f);
        }
        if let Some(m) = &o.fd_modes {
            if m.contains(&crate::rpc::FileDescriptorTransportMode::Unix)
                && !uri.endpoint.supports_fd_passing()
            {
                log::error!(
                    "rsbinder::serve: option `fd_modes` cannot advertise Unix fd passing on \
                     {:?} — only Unix-domain sockets carry SCM_RIGHTS",
                    uri.endpoint
                );
                return Err(StatusCode::BadValue);
            }
            server.set_supported_fd_modes(m);
        }
        for (name, binder) in pending {
            server.add_service(&name, binder)?;
        }
        Ok(server)
    }
}
