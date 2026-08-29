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

/// Options that do not fit in the URI. Set through [`Server::with`].
/// An option that does not apply to the server's transport is reported
/// at [`Server::run`] / [`Server::spawn`] time as [`StatusCode::BadValue`]
/// (with a log line naming it) — never silently ignored.
#[derive(Default)]
#[non_exhaustive]
pub struct ServeOptions {
    /// Kernel: `ProcessState` max threads (also settable as
    /// `binder://?threads=`). RPC: `RpcServer::set_max_threads`.
    pub threads: Option<u32>,
    /// RPC: `RpcServer::set_max_connections`.
    pub max_connections: Option<usize>,
    /// RPC: `RpcServer::set_handshake_timeout`.
    pub handshake_timeout: Option<Duration>,
    /// RPC: `RpcServer::set_idle_timeout`.
    pub idle_timeout: Option<Duration>,
    /// RPC: `RpcServer::set_authorizer` — accept/reject a peer by identity.
    #[cfg(feature = "rpc")]
    pub authorizer: Option<Authorizer>,
    /// `tls://` (required there) or any RPC socket: TLS server config.
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
/// process reuses it and logs a warning if it asked for a different
/// driver/thread count). RPC: the listener is bound at `run`/`spawn`
/// so [`ServeOptions`] (TLS config, limits) can be applied first.
pub struct Server {
    uri: Uri,
    options: ServeOptions,
    pending: Vec<(String, SIBinder)>,
}

/// Handle for a [`Server::spawn`]ed server. Dropping it shuts an RPC
/// server down (listener closed, workers joined). For the kernel there
/// is nothing to stop — the process thread pool has no shutdown — so
/// the guard is inert.
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
    /// Stop the server now (RPC) and wait for its threads. Kernel: no-op.
    pub fn shutdown(mut self) {
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
            server.shutdown();
            let _ = jh.join();
            server.join_workers();
        }
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) fn new_server(uri: Uri) -> Result<Server> {
    if let Endpoint::Kernel { driver, threads } = &uri.endpoint {
        kernel_init(driver.as_deref(), *threads)?;
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

/// Initialize the process-global kernel `ProcessState` (idempotent).
/// Loud on a lost config: a *different* driver / `max_threads` than the
/// one already in force is warned about, never silently dropped.
///
/// `max_threads` is `None` when the URI carried no `?threads=`, which is
/// the only thing that means "the default" — `?threads=0` asks for a
/// literal zero and gets it, as [`ProcessState::init`] documents.
pub(super) fn kernel_init(
    driver: Option<&std::path::Path>,
    max_threads: Option<u32>,
) -> Result<()> {
    let pre = ProcessState::is_initialized();
    let ps = match driver {
        Some(p) => ProcessState::init(
            &p.to_string_lossy(),
            max_threads.unwrap_or(crate::DEFAULT_MAX_BINDER_THREADS),
        ),
        None => match max_threads {
            Some(n) => ProcessState::init(ProcessState::default_driver_path(), n),
            None => ProcessState::init_default(),
        },
    }
    .map_err(|e| {
        log::error!("rsbinder: ProcessState init failed: {e}");
        StatusCode::NoInit
    })?;
    if pre {
        let driver_mismatch = driver.is_some_and(|d| d != ps.driver_name());
        let threads_mismatch = max_threads.is_some_and(|n| n != ps.max_threads());
        if driver_mismatch || threads_mismatch {
            log::warn!(
                "rsbinder: ProcessState already initialized; requested driver={driver:?} \
                 max_threads={max_threads:?} ignored, using existing driver={:?} max_threads={}",
                ps.driver_name(),
                ps.max_threads()
            );
        }
    }
    Ok(())
}

impl Server {
    /// Publish `svc` under `name`. Kernel: registered with the system
    /// service manager immediately. RPC: queued and registered in the
    /// server's directory when it starts.
    pub fn add(mut self, name: &str, svc: impl Into<SIBinder>) -> Result<Self> {
        let binder = svc.into();
        if self.uri.endpoint.is_kernel() {
            crate::hub::add_service(name, binder).map_err(StatusCode::from)?;
        } else {
            self.pending.push((name.to_string(), binder));
        }
        Ok(self)
    }

    /// Set [`ServeOptions`]. May be called more than once; applied at
    /// [`run`](Self::run) / [`spawn`](Self::spawn).
    pub fn with(mut self, f: impl FnOnce(&mut ServeOptions)) -> Self {
        f(&mut self.options);
        self
    }

    /// The parsed endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.uri.endpoint
    }

    /// Start serving and block. Kernel: starts the thread pool and joins
    /// it (never returns normally). RPC: runs the accept loop until
    /// `RpcServer::shutdown` is called from another thread
    /// (reachable via [`spawn`](Self::spawn)'s guard instead).
    pub fn run(self) -> Result<()> {
        match self.uri.endpoint {
            Endpoint::Kernel { .. } => {
                self.apply_kernel_options()?;
                ProcessState::start_thread_pool();
                ProcessState::join_thread_pool()
            }
            #[cfg(feature = "rpc")]
            _ => self.build_rpc()?.run(),
            #[cfg(not(feature = "rpc"))]
            _ => Err(StatusCode::InvalidOperation),
        }
    }

    /// Start serving in the background. Kernel: starts the thread pool
    /// (the returned guard is inert). RPC: accept loop on its own thread;
    /// dropping the guard shuts it down.
    pub fn spawn(self) -> Result<ServerGuard> {
        match self.uri.endpoint {
            Endpoint::Kernel { .. } => {
                self.apply_kernel_options()?;
                ProcessState::start_thread_pool();
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

    fn apply_kernel_options(&self) -> Result<()> {
        let o = &self.options;
        if o.max_connections.is_some() || o.handshake_timeout.is_some() || o.idle_timeout.is_some()
        {
            return Err(self.reject("max_connections/handshake_timeout/idle_timeout"));
        }
        #[cfg(feature = "rpc")]
        if o.authorizer.is_some() || o.fd_modes.is_some() {
            return Err(self.reject("authorizer/fd_modes"));
        }
        #[cfg(feature = "rpc-tls")]
        if o.tls.is_some() {
            return Err(self.reject("tls"));
        }
        if o.threads.is_some() {
            // `threads` after init cannot change the pool; treat like the
            // URI form (warn on mismatch) by re-running the idempotent init.
            kernel_init(None, o.threads)?;
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
        if o.call_restriction.is_some() {
            log::error!(
                "rsbinder::serve: option `call_restriction` does not apply to {:?}",
                uri.endpoint
            );
            return Err(StatusCode::BadValue);
        }
        #[cfg(feature = "rpc-tls")]
        let tls = o.tls.clone();
        let server = match &uri.endpoint {
            Endpoint::Kernel { .. } => unreachable!("kernel handled by caller"),
            Endpoint::Unix(path) => {
                #[cfg(feature = "rpc-tls")]
                if let Some(cfg) = tls {
                    RpcServer::setup_unix_server_tls(path.clone(), cfg)?
                } else {
                    RpcServer::setup_unix_server(path.clone())?
                }
                #[cfg(not(feature = "rpc-tls"))]
                RpcServer::setup_unix_server(path.clone())?
            }
            Endpoint::UnixAbstract(name) => {
                // No TLS variant of the abstract listener exists; refusing
                // is the "never silently ignored" contract — and it matters
                // more here than anywhere: an abstract socket has no
                // filesystem permissions, so mTLS may be the only
                // authentication the operator configured.
                #[cfg(feature = "rpc-tls")]
                if tls.is_some() {
                    log::error!(
                        "rsbinder::serve: option `tls` does not apply to {:?} \
                         (no TLS listener for abstract Unix sockets)",
                        uri.endpoint
                    );
                    return Err(StatusCode::BadValue);
                }
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
            Endpoint::Vsock(cid, port) => {
                #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
                {
                    #[cfg(feature = "rpc-tls")]
                    if let Some(cfg) = tls {
                        RpcServer::setup_vsock_server_tls(*cid, *port, cfg)?
                    } else {
                        RpcServer::setup_vsock_server(*cid, *port)?
                    }
                    #[cfg(not(feature = "rpc-tls"))]
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
            Endpoint::Tls(host, port) => {
                #[cfg(feature = "rpc-tls")]
                {
                    let cfg = tls.ok_or_else(|| {
                        log::error!("rsbinder::serve: `tls://` requires `ServeOptions::tls`");
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
        if let Some(v) = uri.wire_max_version {
            server.set_android13plus(v);
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
