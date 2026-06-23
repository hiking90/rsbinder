// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Cross-transport service facade (Plan 2-16 Phase D).
//!
//! Write service **registration and lookup code once** and pick the
//! transport — kernel binder or RPC — by construction. The AIDL
//! interface, the generated `Bp*`/`Bn*` stubs, the service `impl`, and the
//! call sites are already transport-agnostic; this layer makes the
//! *bootstrap* (register / look up) transport-agnostic too, behind two
//! small object-safe traits:
//!
//! - [`Registry`] (server side) — `add_service(name, binder)`.
//! - [`Broker`] (client side) — `lookup(name)` + `get_interface::<T>(name)`.
//!
//! and two typed host/broker pairs, one per transport:
//!
//! - [`kernel::Host`] / [`kernel::Broker`] — over `ProcessState` + `hub`.
//! - [`rpc::Host`] / [`rpc::Broker`] — over `RpcServer` + `RpcSession`
//!   (`#[cfg(feature = "rpc")]`).
//!
//! # This is additive, not a replacement
//!
//! The low-level `ProcessState` / `hub` / `RpcServer` / `RpcSession` APIs
//! are unchanged and remain the direct path. This is a *typed* convenience
//! layer over them (no wire change, kernel byte-identical — R3).
//!
//! # The intersection is deliberately small (Plan 2-16 §2, objection 3)
//!
//! Only `add_service` / `lookup` are on the shared traits — the genuine
//! kernel∩RPC intersection. Kernel-only powers (`list_services`,
//! service-notification callbacks, lazy services, dumpsys priority) stay
//! on the concrete kernel types / the [`crate::hub`] module: they are not
//! hidden behind the trait, and **not** faked on RPC.
//!
//! # The transports' security differs — and that is made loud, not hidden
//!
//! Moving a service kernel→RPC changes the trust boundary. Plan 2-16
//! Phase A/B made that explicit so this facade is safe to use:
//! `@EnforcePermission` methods **deny** over RPC (never silently grant —
//! Phase A), and [`crate::get_calling_uid`] returns the kernel-vouched
//! peer uid over Unix RPC / a fail-closed sentinel on uid-less transports
//! (Phase B). Authorization on RPC is the transport's own boundary
//! (`PeerIdentity` + [`crate::rpc::RpcServer::set_authorizer`]).
//!
//! # Why typed pairs, not one `Endpoint` enum
//!
//! `serve()` means different things per transport (kernel = the whole
//! process pool; RPC = this one socket), the kernel host is a
//! process-global singleton while an RPC host is a real instance, and the
//! construction options do not overlap. Distinct types keep all of that
//! visible at the call site and make a wrong-transport option a compile
//! error rather than a silent no-op.
//!
//! ```no_run
//! use rsbinder::service::{Registry, Broker};
//! # use rsbinder::{SIBinder, Strong, FromIBinder};
//! # fn demo<R: Registry>(reg: &R, svc: SIBinder) -> rsbinder::Result<()> {
//! // register once, generic over the transport
//! reg.add_service("hello", svc)?;
//! # Ok(()) }
//! ```

use crate::error::{Result, StatusCode};
use crate::{FromIBinder, SIBinder, Strong};

/// Server side of the facade: register a binder under a name.
///
/// The **genuine kernel∩RPC intersection** — kernel-only registration
/// extras (notifications, lazy services, list/dump) stay on the concrete
/// kernel type, not here. Object-safe.
pub trait Registry {
    /// Publish `binder` under `name` so a [`Broker`] on the same transport
    /// can [`Broker::lookup`] it.
    ///
    /// Kernel: delegates to [`crate::hub::add_service`] (the system service
    /// manager). RPC: delegates to
    /// [`crate::rpc::RpcServer::add_service`] (an in-process directory the
    /// session root resolves) — there is no system-wide RPC service
    /// manager, by design (mirrors AOSP).
    fn add_service(&self, name: &str, binder: SIBinder) -> Result<()>;
}

/// Client side of the facade: resolve a binder (and cast it) by name.
///
/// Object-safe in `lookup`; the generic [`Broker::get_interface`] cast is
/// a provided default so the common `Strong<dyn IFoo>` path is one call.
pub trait Broker {
    /// Resolve the raw binder published under `name`, or
    /// [`StatusCode::NameNotFound`] if absent.
    fn lookup(&self, name: &str) -> Result<SIBinder>;

    /// Resolve `name` and cast it to the interface `T` (the
    /// `interface_cast` step), mirroring [`crate::hub::get_interface`].
    ///
    /// `where Self: Sized` keeps the trait object-safe: this generic
    /// convenience stays callable on a concrete broker or an
    /// `impl Broker` bound, while `&dyn Broker` remains usable via
    /// [`Broker::lookup`] + a free [`FromIBinder::try_from`].
    fn get_interface<T: FromIBinder + ?Sized>(&self, name: &str) -> Result<Strong<T>>
    where
        Self: Sized,
    {
        FromIBinder::try_from(self.lookup(name)?)
    }
}

pub mod kernel {
    //! Kernel-binder transport. [`Host`]/[`Broker`] are process-global
    //! handles over the singleton [`crate::ProcessState`] + the system
    //! [`crate::hub`] service manager.

    use super::*;
    use crate::process_state::{CallRestriction, ProcessState};

    /// Initialize (idempotently) the process-global kernel `ProcessState`
    /// and return a handle. `ProcessState::init`/`init_default` is
    /// process-wide; a second `Host` in the same process **reuses** the
    /// existing instance. See [`Host::builder`] for the loud-on-conflict
    /// behavior.
    fn init(driver: Option<&std::path::Path>, max_threads: u32) -> Result<()> {
        let pre = ProcessState::is_initialized();
        let ps = match driver {
            Some(p) => ProcessState::init(&p.to_string_lossy(), max_threads),
            None => ProcessState::init_default(),
        }
        .map_err(|e| {
            log::error!("kernel::Host: ProcessState init failed: {e}");
            StatusCode::NoInit
        })?;

        // Loud on lost config (Plan 2-16 §6): keep the idempotent reuse
        // (two independent hosts in one process is a valid pattern), but
        // never silently drop a *different* requested config.
        if pre {
            let want_driver = driver.map(|p| p.to_path_buf());
            let driver_mismatch = want_driver
                .as_deref()
                .is_some_and(|d| d != ps.driver_name());
            // Compare the *clamped* requested value against what was
            // actually stored, so re-requesting the same out-of-range
            // `max_threads` (both clamped to the default) does not warn.
            let threads_mismatch = max_threads != 0
                && ProcessState::clamp_max_threads(max_threads) != ps.max_threads();
            if driver_mismatch || threads_mismatch {
                log::warn!(
                    "kernel::Host: ProcessState already initialized; requested \
                     driver={driver:?} max_threads={max_threads} ignored, using \
                     existing driver={:?} max_threads={}",
                    ps.driver_name(),
                    ps.max_threads()
                );
            }
        }
        Ok(())
    }

    /// Process-global kernel-binder service host.
    ///
    /// Construction is **idempotent** (`ProcessState` is process-wide);
    /// [`Host::serve`] joins the **process-wide** binder thread pool — it
    /// is not a per-instance lifecycle. Contrast [`super::rpc::Host`],
    /// whose `serve()` drives a single socket.
    pub struct Host {
        _priv: (),
    }

    impl Host {
        /// Initialize the process `ProcessState` with the default binder
        /// path (idempotent) and return a host.
        pub fn new() -> Result<Self> {
            init(None, 0)?;
            Ok(Host { _priv: () })
        }

        /// Builder for the non-default cases (custom driver path,
        /// `max_threads`, call restriction). All options map onto the
        /// existing [`crate::ProcessState`] surface.
        pub fn builder() -> HostBuilder {
            HostBuilder::default()
        }

        /// Start the binder thread pool and **block**, joining the
        /// process-wide pool until it terminates. Process-wide, not
        /// per-instance (see the type docs).
        pub fn serve(&self) -> Result<()> {
            ProcessState::start_thread_pool();
            ProcessState::join_thread_pool()
        }
    }

    impl Registry for Host {
        fn add_service(&self, name: &str, binder: SIBinder) -> Result<()> {
            crate::hub::add_service(name, binder).map_err(StatusCode::from)
        }
    }

    /// Builder for [`Host`]. The options live here (not on a shared
    /// cross-transport builder) precisely because they are kernel-only —
    /// a wrong-transport option is a compile error, not a silent no-op.
    #[derive(Default)]
    pub struct HostBuilder {
        driver: Option<std::path::PathBuf>,
        max_threads: u32,
        call_restriction: Option<CallRestriction>,
    }

    impl HostBuilder {
        /// Binder driver path (default `/dev/binderfs/binder`). Maps to
        /// [`crate::ProcessState::init`].
        pub fn driver(mut self, path: impl Into<std::path::PathBuf>) -> Self {
            self.driver = Some(path.into());
            self
        }

        /// Kernel thread-pool size (`0` = kernel default).
        pub fn max_threads(mut self, n: u32) -> Self {
            self.max_threads = n;
            self
        }

        /// Restrict outgoing calls (AOSP `setCallRestriction`).
        pub fn call_restriction(mut self, cr: CallRestriction) -> Self {
            self.call_restriction = Some(cr);
            self
        }

        /// Initialize `ProcessState` (idempotent — warns if a prior init
        /// used a different driver/`max_threads`) and return the host.
        pub fn build(self) -> Result<Host> {
            init(self.driver.as_deref(), self.max_threads)?;
            if let Some(cr) = self.call_restriction {
                ProcessState::as_self().set_call_restriction(cr);
            }
            Ok(Host { _priv: () })
        }
    }

    /// Process-global kernel-binder client broker over the system
    /// [`crate::hub`] service manager.
    pub struct Broker {
        _priv: (),
    }

    impl Broker {
        /// Initialize the process `ProcessState` (idempotent) so the
        /// system service manager is reachable, and return a broker.
        pub fn new() -> Result<Self> {
            init(None, 0)?;
            Ok(Broker { _priv: () })
        }
    }

    impl super::Broker for Broker {
        // Immediate lookup mirrors the RPC `Broker::lookup`; single-shot is
        // the intended facade semantics (no implicit wait).
        #[allow(deprecated)]
        fn lookup(&self, name: &str) -> Result<SIBinder> {
            crate::hub::get_service(name).ok_or(StatusCode::NameNotFound)
        }
    }
}

#[cfg(feature = "rpc")]
pub mod rpc {
    //! RPC transport. [`Host`] wraps an [`crate::rpc::RpcServer`] (one
    //! socket); [`Broker`] wraps an [`crate::rpc::RpcSession`] client.

    use super::*;
    use crate::rpc::transport::PeerIdentity;
    use crate::rpc::{RpcServer, RpcSession};
    use std::sync::Arc;

    /// RPC service host — one listening socket (contrast the process-wide
    /// [`super::kernel::Host`]).
    pub struct Host {
        server: Arc<RpcServer>,
    }

    impl Host {
        /// Listen on a Unix-domain socket at `path`.
        pub fn unix(path: impl Into<std::path::PathBuf>) -> Result<Self> {
            Ok(Host {
                server: RpcServer::setup_unix_server(path)?,
            })
        }

        /// Builder for the optioned case (max threads, max connections,
        /// authorizer). The options are RPC-only — they live here, not on
        /// a shared builder.
        pub fn builder() -> HostBuilder {
            HostBuilder::default()
        }

        /// Borrow the underlying [`RpcServer`] for the RPC-only powers not
        /// on [`Registry`] (`set_max_connections`, fd modes, TLS, the
        /// session/connection counters, …).
        pub fn server(&self) -> &Arc<RpcServer> {
            &self.server
        }

        /// Serve this socket and **block** until shutdown.
        pub fn serve(&self) -> Result<()> {
            self.server.run()
        }

        /// Serve this socket on a background thread, returning its
        /// [`JoinHandle`](std::thread::JoinHandle). RPC-specific lifecycle
        /// (no kernel analogue), so it is on the concrete type only.
        pub fn serve_background(&self) -> std::thread::JoinHandle<()> {
            self.server.run_background()
        }
    }

    impl Registry for Host {
        fn add_service(&self, name: &str, binder: SIBinder) -> Result<()> {
            self.server.add_service(name, binder)
        }
    }

    type Authorizer = Box<dyn Fn(&PeerIdentity) -> bool + Send + Sync + 'static>;

    /// Builder for [`Host`].
    #[derive(Default)]
    pub struct HostBuilder {
        path: Option<std::path::PathBuf>,
        max_threads: Option<u32>,
        max_connections: Option<usize>,
        authorizer: Option<Authorizer>,
    }

    impl HostBuilder {
        /// Listen on a Unix-domain socket at `path`.
        pub fn unix(mut self, path: impl Into<std::path::PathBuf>) -> Self {
            self.path = Some(path.into());
            self
        }

        /// Advertised / incoming-slot max threads
        /// ([`RpcServer::set_max_threads`]).
        pub fn max_threads(mut self, n: u32) -> Self {
            self.max_threads = Some(n);
            self
        }

        /// Concurrent connection-worker cap
        /// ([`RpcServer::set_max_connections`]).
        pub fn max_connections(mut self, n: usize) -> Self {
            self.max_connections = Some(n);
            self
        }

        /// Per-connection authorizer over the peer's
        /// [`PeerIdentity`] ([`RpcServer::set_authorizer`]) — the RPC
        /// trust-boundary hook (there is no uid/permission model on RPC).
        pub fn authorizer<F>(mut self, f: F) -> Self
        where
            F: Fn(&PeerIdentity) -> bool + Send + Sync + 'static,
        {
            self.authorizer = Some(Box::new(f));
            self
        }

        /// Build the [`Host`] (currently Unix-only; use the low-level
        /// [`RpcServer`] setup constructors for vsock/TLS).
        pub fn build(self) -> Result<Host> {
            let path = self.path.ok_or(StatusCode::BadValue)?;
            let server = RpcServer::setup_unix_server(path)?;
            if let Some(n) = self.max_threads {
                server.set_max_threads(n);
            }
            if let Some(n) = self.max_connections {
                server.set_max_connections(n);
            }
            if let Some(f) = self.authorizer {
                // `Box<dyn Fn..>` already implements `Fn`, so hand it to
                // `set_authorizer` directly — no second closure/box layer.
                server.set_authorizer(f);
            }
            Ok(Host { server })
        }
    }

    /// RPC client broker over an [`RpcSession`] connection.
    pub struct Broker {
        session: RpcSession,
    }

    impl Broker {
        /// Connect to a Unix-domain RPC server at `path`.
        pub fn unix(path: impl AsRef<std::path::Path>) -> Result<Self> {
            Ok(Broker {
                session: RpcSession::setup_unix_client(path)?,
            })
        }

        /// Borrow the underlying [`RpcSession`] for RPC-only client powers
        /// (the negotiated root object, fan-out connect, …).
        pub fn session(&self) -> &RpcSession {
            &self.session
        }
    }

    impl super::Broker for Broker {
        fn lookup(&self, name: &str) -> Result<SIBinder> {
            self.session.get_service(name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both facade traits are object-safe (the module/trait rustdoc says
    /// so). `Broker::get_interface` is generic, so it must carry
    /// `where Self: Sized` to stay out of the vtable — this lock-in fails
    /// to compile if that bound is dropped (the original review finding).
    #[test]
    fn traits_are_object_safe() {
        fn _registry(_: &dyn Registry) {}
        fn _broker(_: &dyn Broker) {}
    }
}
