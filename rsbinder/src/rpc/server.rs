// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcServer` — bind / listen / accept, one session per connection.
//!
//! Model: **one connection ⇒ one [`RpcSession`] ⇒ one worker thread**,
//! each with its own [`super::state::RpcState`] (no global, so sessions
//! are isolated and the suite is parallel-safe). Concurrent clients use
//! independent connections; nested re-entrant calls run inline on a
//! connection's worker (the `client_transact` recv loop dispatches
//! inbound `TRANSACT`s). The *semantics* (concurrency-correct,
//! isolated, oneway FIFO, negotiated, timed-out) match android-12 r34.
//!
//! Naming: android semantics, snake_case (`setup_unix_server`,
//! `get_root`, `add_service`, `set_max_threads`).

use std::collections::HashMap;
#[cfg(feature = "rpc-tls")]
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(target_os = "android")]
use std::os::android::net::SocketAddrExt;
#[cfg(target_os = "linux")]
use std::os::linux::net::SocketAddrExt;
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::os::unix::net::SocketAddr as UnixSocketAddr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::binder::{Interface, Remotable, SIBinder, TransactionCode};
use crate::error::{Result, StatusCode};
use crate::native::Binder;
use crate::parcel::Parcel;

use super::session::{RpcSession, RpcSessionId, RpcSessionInner};
#[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
use super::transport::VsockTransport;
use super::transport::{PeerIdentity, RpcTransport, UnixTransport};
#[cfg(feature = "rpc-tls")]
use super::transport::{TlsStream, TlsTransport};
use super::RpcResult;

/// Server-side TLS handle. `Some` ⇒ every accepted
/// connection is TLS-wrapped on its worker thread (handshake under the
/// `max_connections` cap, so a slow-handshake attacker can stall its own
/// worker but never the accept loop). `None` ⇒ plain transport
/// (byte-identical to a non-TLS server). `Mutex<Option<...>>` mirrors the other
/// late-bind config fields (`max_threads`, `authorizer`, etc.) so a
/// caller that builds the server then `set_*`s knobs has a single
/// mutability discipline.
#[cfg(feature = "rpc-tls")]
type TlsServerConfigCell = Mutex<Option<Arc<rustls::ServerConfig>>>;

/// Backend-agnostic listener kind for [`RpcServer`].
/// The accept loop in [`RpcServer::run`] holds one of these and the
/// `accept_raw` helper produces a [`RawAccepted`] so the wrap step
/// (native or TLS) happens on the worker thread. Default
/// `setup_unix_server` callers stay on the `Unix` variant, so the wire
/// is byte-unchanged on that path.
///
/// **Tcp variant**: TCP is *internal-only*. There is no public
/// `setup_tcp_server` factory because plaintext network RPC is never
/// production-appropriate (see [`super`] module doc). The TCP arm is
/// reached only through [`setup_tcp_server_tls`](RpcServer::setup_tcp_server_tls).
enum ServerListener {
    Unix(UnixListener),
    #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
    Vsock(vsock::VsockListener),
    #[cfg(feature = "rpc-tls")]
    Tcp(TcpListener),
}

/// Backend-agnostic bind metadata. `Drop` branches on this for the
/// per-backend cleanup: path `Unix` removes the socket file;
/// abstract Unix, `Vsock`, and `Tcp` have no filesystem cleanup (the
/// kernel reclaims the bind on `Drop` of the listener fd itself).
enum BindAddress {
    Unix(PathBuf),
    UnixAbstract,
    #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
    Vsock {
        cid: u32,
        port: u32,
    },
    #[cfg(feature = "rpc-tls")]
    Tcp(SocketAddr),
}

/// Raw accepted stream awaiting the worker-thread
/// wrap. Yielded by [`ServerListener::accept_raw`]; consumed by
/// [`RawAccepted::into_transport`] inside the spawned worker.
///
/// Splitting accept (cheap kernel `accept(2)`) from wrap
/// (potentially-expensive TLS handshake) is what keeps a slow-
/// handshake attacker from stalling the accept loop — the worker
/// thread eats the handshake time, bounded by
/// [`RpcServer::set_max_connections`](RpcServer::set_max_connections).
enum RawAccepted {
    Unix(UnixStream),
    #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
    Vsock(vsock::VsockStream),
    #[cfg(feature = "rpc-tls")]
    Tcp(TcpStream),
}

impl ServerListener {
    /// Set the listener to non-blocking so the accept loop can poll
    /// `shutdown`. The `vsock` crate exposes `set_nonblocking` on
    /// `VsockListener` mirroring `UnixListener`/`TcpListener`'s std API.
    fn set_nonblocking(&self, on: bool) -> std::io::Result<()> {
        match self {
            ServerListener::Unix(l) => l.set_nonblocking(on),
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            ServerListener::Vsock(l) => l.set_nonblocking(on),
            #[cfg(feature = "rpc-tls")]
            ServerListener::Tcp(l) => l.set_nonblocking(on),
        }
    }

    /// Per-backend accept: returns the raw stream paired with its
    /// backend tag, *without* wrapping it as `RpcTransport`. The wrap
    /// (and any TLS handshake) runs in the worker thread spawned by
    /// [`RpcServer::serve_connection_raw`].
    fn accept_raw(&self) -> std::io::Result<RawAccepted> {
        match self {
            ServerListener::Unix(l) => {
                let (stream, _addr) = l.accept()?;
                // The listener is non-blocking so the accept loop can
                // poll `shutdown`; the accepted connection must be
                // blocking for the worker's `recv_frame` (and for the
                // worker-side TLS handshake when applicable).
                stream.set_nonblocking(false)?;
                Ok(RawAccepted::Unix(stream))
            }
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            ServerListener::Vsock(l) => {
                let (stream, _addr) = l.accept()?;
                stream.set_nonblocking(false)?;
                Ok(RawAccepted::Vsock(stream))
            }
            #[cfg(feature = "rpc-tls")]
            ServerListener::Tcp(l) => {
                let (stream, _addr) = l.accept()?;
                stream.set_nonblocking(false)?;
                // TCP-specific: `nodelay=true` matches the client-side
                // `TlsTransport::connect`/`accept` (back-compat
                // convenience). Small-frame RPC traffic ⇒ disable
                // Nagle so handshake + first request don't coalesce.
                stream.set_nodelay(true)?;
                Ok(RawAccepted::Tcp(stream))
            }
        }
    }
}

impl RawAccepted {
    /// Arm a read deadline on the raw stream *before* it is wrapped, so
    /// the pre-wrap TLS handshake (driven inside [`into_transport`],
    /// which performs blocking reads on this socket) is itself bounded.
    /// Without this, the [`run_connection_in_worker`] deadline — armed
    /// only after the wrap returns — never covers the handshake, so a
    /// connected-but-silent TLS peer would pin its worker thread (and,
    /// under [`set_max_connections`](RpcServer::set_max_connections), the
    /// whole accept loop) indefinitely. Best-effort: a set failure just
    /// means no deadline. Harmless on the plain (UDS/vsock) path — that
    /// wrap performs no I/O and the same deadline is re-armed before the
    /// native handshake reads.
    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> std::io::Result<()> {
        match self {
            RawAccepted::Unix(s) => s.set_read_timeout(timeout),
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            RawAccepted::Vsock(s) => s.set_read_timeout(timeout),
            #[cfg(feature = "rpc-tls")]
            RawAccepted::Tcp(s) => s.set_read_timeout(timeout),
        }
    }

    /// Companion to [`set_read_timeout`](Self::set_read_timeout): bound the
    /// pre-wrap handshake's *write* side too. A peer that completes enough
    /// of the handshake to be admitted but then stops reading would stall
    /// our blocking handshake-reply `write_all` once its receive window
    /// fills, pinning this worker (and its admission slot) — symmetric to
    /// the read-side Slowloris. Best-effort.
    fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> std::io::Result<()> {
        match self {
            RawAccepted::Unix(s) => s.set_write_timeout(timeout),
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            RawAccepted::Vsock(s) => s.set_write_timeout(timeout),
            #[cfg(feature = "rpc-tls")]
            RawAccepted::Tcp(s) => s.set_write_timeout(timeout),
        }
    }

    /// Wrap the raw stream as `RpcTransport`, on the
    /// worker thread. `tls_config = Some(cfg)` ⇒ drive a server-side
    /// TLS handshake via [`TlsTransport::accept_stream`] over the raw
    /// byte stream; `None` ⇒ a plain backend transport
    /// (`UnixTransport`/`VsockTransport`).
    ///
    /// Plain TCP is rejected here (`TLS-only on TCP` — see
    /// [`ServerListener::Tcp`]).
    #[cfg(feature = "rpc-tls")]
    fn into_transport(
        self,
        tls_config: Option<Arc<rustls::ServerConfig>>,
    ) -> RpcResult<Box<dyn RpcTransport>> {
        if let Some(cfg) = tls_config {
            let stream: Box<dyn TlsStream> = match self {
                RawAccepted::Unix(s) => Box::new(s),
                #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
                RawAccepted::Vsock(s) => Box::new(s),
                RawAccepted::Tcp(s) => Box::new(s),
            };
            return Ok(Box::new(TlsTransport::accept_stream(stream, cfg)?));
        }
        self.into_native_transport()
    }

    /// rpc-tls-OFF build entry point — TLS path absent, plain wrap only.
    #[cfg(not(feature = "rpc-tls"))]
    fn into_transport(self) -> RpcResult<Box<dyn RpcTransport>> {
        self.into_native_transport()
    }

    /// Native (plain) wrap — common to both feature build modes.
    fn into_native_transport(self) -> RpcResult<Box<dyn RpcTransport>> {
        match self {
            RawAccepted::Unix(s) => Ok(Box::new(UnixTransport::from_stream(s)?)),
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            RawAccepted::Vsock(s) => Ok(Box::new(VsockTransport::from_stream(s)?)),
            #[cfg(feature = "rpc-tls")]
            RawAccepted::Tcp(_) => Err(super::RpcError::Protocol(
                "plain-text TCP server is not exposed (TLS-only on TCP)",
            )),
        }
    }
}

/// Built-in directory interface descriptor + its single transaction.
const DIRECTORY_DESC: &str = "rsbinder.rpc.IServiceDirectory";
const TX_GET_SERVICE: TransactionCode = crate::binder::FIRST_CALL_TRANSACTION;

/// Default handshake/admission read deadline (see
/// [`RpcServer::set_handshake_timeout`]). Bounds only the pre-serve
/// phase; a connected peer that never sends its handshake is dropped
/// after this so it cannot hold a `max_connections` slot (or pin the
/// server's `Arc`) forever.
const DEFAULT_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Built-in name → binder directory, used to back [`RpcServer::add_service`]
/// (android RPC has a single root object; this *is* that root when
/// named services are registered). Reused, unmodified, via the same
/// `Remotable::on_transact` server path as any AIDL stub.
///
/// The map is **shared** (`Arc<Mutex<…>>`) with [`RpcServer::named`], so
/// the directory binder is built once and every later
/// [`RpcServer::add_service`] is an O(1) insert visible through this same
/// directory — no per-call rebuild or root swap.
struct ServiceDirectory {
    services: Arc<Mutex<HashMap<String, SIBinder>>>,
}

impl Remotable for ServiceDirectory {
    fn descriptor() -> &'static str {
        DIRECTORY_DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            TX_GET_SERVICE => {
                let name: String = reader.read()?;
                // Clone the binder out from under the lock before writing.
                let found = self
                    .services
                    .lock()
                    .expect("named poisoned")
                    .get(&name)
                    .cloned();
                match found {
                    Some(b) => {
                        reply.write(&crate::Status::from(StatusCode::Ok))?;
                        reply.write(&b)
                    }
                    None => reply.write(&crate::Status::from(StatusCode::NameNotFound)),
                }
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

/// Authorization hook: given the connecting
/// peer's [`PeerIdentity`], return `true` to admit, `false` to refuse
/// (the connection is closed before any RPC byte). `Arc` so it can be
/// cloned out of the lock and invoked lock-free.
type Authorizer = Arc<dyn Fn(&PeerIdentity) -> bool + Send + Sync>;

/// An RPC server. Backend is chosen by the constructor:
/// [`setup_unix_server`](RpcServer::setup_unix_server) (UDS, default) or
/// `setup_vsock_server` (Linux/Android only,
/// AVF / Microdroid). The accept loop + worker dispatch are
/// backend-agnostic — every accepted connection becomes one `RpcSession`
/// on a worker thread regardless of backend.
pub struct RpcServer {
    listener: ServerListener,
    bind: BindAddress,
    /// `Some(cfg)` ⇒ TLS server, every accepted
    /// connection is handshaken on its worker thread with this config.
    /// `None` ⇒ plain transport (the default; byte-identical to
    /// a non-TLS `RpcServer` for UDS/vsock). Late-bound via
    /// [`setup_unix_server_tls`](Self::setup_unix_server_tls),
    /// [`setup_tcp_server_tls`](Self::setup_tcp_server_tls), and
    /// [`setup_vsock_server_tls`](Self::setup_vsock_server_tls).
    #[cfg(feature = "rpc-tls")]
    tls_config: TlsServerConfigCell,
    root: Mutex<Option<SIBinder>>,
    /// Named services backing [`add_service`](RpcServer::add_service).
    /// `Arc` so the single [`ServiceDirectory`] root (`directory`) shares
    /// this exact map — each later `add_service` is an O(1) insert, no
    /// rebuild. Per-server instance state, not a process global.
    named: Arc<Mutex<HashMap<String, SIBinder>>>,
    /// The directory root binder, built once at construction over the
    /// shared `named` map (so it reads later inserts live) and installed
    /// as the root on the first `add_service`. Per-server instance state.
    directory: SIBinder,
    max_threads: Mutex<u32>,
    /// Whether per-connection sessions advertise `Unix` FD support
    /// (default false ⇒ FD reject everywhere).
    fd_unix_supported: AtomicBool,
    /// Opt-in android-13+ versioned wire:
    /// `None` ⇒ the default android-12 r34 wire (byte-unchanged);
    /// `Some(max)` ⇒ each accepted connection runs the AOSP handshake
    /// negotiating `min(max, client_max)`.
    wire_max_version: Mutex<Option<u32>>,
    /// Opt-in **server-side admission bound** on the number of
    /// *concurrent* connection-worker threads. `None` (default) ⇒
    /// unbounded, byte-for-byte the prior behavior (additive
    /// invariant). `Some(max)` ⇒ the accept loop stops accepting while
    /// `max` workers are live (excess clients wait in the kernel listen
    /// backlog — clean backpressure, no reactor, no dropped client),
    /// resuming when a worker finishes. This is the rsbinder analogue
    /// of AOSP `RpcServer`'s bounded server resources (rsbinder is
    /// 1-connection = 1-session = 1-worker, so the resource to bound is
    /// the concurrent worker count); it is **not** a wire/semantic port
    /// and does **not** reduce workers below the connection count
    /// (that would require I/O multiplexing — explicitly out of scope).
    max_connections: Mutex<Option<usize>>,
    /// Handshake/admission read deadline applied to each accepted
    /// connection *before* it enters the blocking serve loop.
    /// `Some(d)` (the default — see [`DEFAULT_HANDSHAKE_TIMEOUT`]) ⇒ a
    /// connected-but-silent peer that never sends its handshake surfaces
    /// as a worker-loop error after `d`, releasing both its
    /// `Arc<RpcServer>` and its `max_connections` admission slot. `None`
    /// ⇒ no deadline (a hung peer can hold a slot indefinitely). The
    /// deadline is cleared once serving begins, so an established
    /// two-way session may idle between requests unbounded.
    handshake_timeout: Mutex<Option<std::time::Duration>>,
    /// Optional per-connection *idle* read deadline applied to the
    /// android-13+ serve loop **after** the handshake completes. `None`
    /// (the default) ⇒ an established session idles between requests
    /// unbounded (byte-identical to prior behavior). `Some(d)` ⇒ a peer
    /// that completes the handshake and then goes silent surfaces as a
    /// serve-loop read error after `d`, releasing its worker and its
    /// [`set_max_connections`](Self::set_max_connections) admission slot —
    /// the post-handshake Slowloris defense that
    /// [`set_handshake_timeout`](Self::set_handshake_timeout) (handshake
    /// phase only) does not cover. Set this only when the protocol has
    /// regular traffic or idle eviction is acceptable.
    idle_timeout: Mutex<Option<std::time::Duration>>,
    /// Opt-in authorization hook. `None`
    /// (default) ⇒ accept-all = byte-for-byte a server without the hook
    /// (additive invariant). When set, it runs at
    /// [`serve_connection`](RpcServer::serve_connection) entry —
    /// **before** the wire-profile branch, session build, handshake,
    /// or any `recv_frame` — so a rejected peer receives **zero RPC
    /// bytes** (the connection is closed). Backend-independent: it is
    /// pure on [`RpcTransport::peer_identity`] (unix `SO_PEERCRED`/
    /// `getpeereid`, tls cert, vsock cid, …). `Arc` (not `Box`) so the
    /// hook is cloned out of the lock and invoked **lock-free**, so a
    /// hook may itself touch the server without self-deadlock (same
    /// discipline as `RpcProxy::send_obituary`). This is the
    /// *enforcement point* for `peer_identity()`.
    authorizer: Mutex<Option<Authorizer>>,
    /// Shutdown-reject e2e scaffolding hook
    /// (`#[doc(hidden)]`, test-only). When set, the closure runs on the
    /// android-13+ attach arm *between* a successful handshake and the
    /// `server.shutdown.load()` gate (the very race window the test
    /// targets, otherwise un-bound by code observability alone). An integration
    /// test acquires the worker at this barrier, calls
    /// [`shutdown`](RpcServer::shutdown), then releases the worker so it
    /// re-reads the now-true flag and takes the reject branch — turning
    /// the otherwise sub-microsecond window into a deterministic test
    /// point. `None` default ⇒ no invocation, byte-identical to the
    /// pre-hook attach path. `Arc<dyn Fn>` so the closure is cloned out
    /// of the lock and invoked **lock-free** (same discipline as
    /// `authorizer` — re-entrant calls into `server` from the probe do
    /// not self-deadlock). Same `__`-prefix unstable-API discipline as
    /// `__fuzz_decode_rpc_parcel`; not part of the supported API.
    #[doc(hidden)]
    attach_shutdown_probe: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Session-id → shared-session registry
    /// (AOSP `RpcServer::mSessions`). The android-13+ accept handshake
    /// reads the client's `RpcConnectionHeader.sessionId`:
    ///  - **empty** id (the default — every single-connection client) ⇒ a
    ///    brand-new session; its server-minted id is registered here
    ///    (a [`std::sync::Weak`] of the session's shared state) and is
    ///    **never looked up** on this path, so the default behavior is
    ///    byte-for-byte unchanged (purely additive);
    ///  - **non-empty** id that resolves to a live session ⇒ **attach**
    ///    this connection to that pre-existing `SharedSession`
    ///    (id-demux: a binder published over the founding connection is
    ///    reachable here — shared `state`/`root`);
    ///  - **non-empty** id that is unknown / stale ⇒ reject (AOSP
    ///    `ALOGE`+return).
    ///
    /// `Weak` so a fully-torn-down session (all connections gone) is
    /// reclaimable and a later echo of its id is treated as unknown.
    /// Written on every mint, resolved on every non-empty id; an attach
    /// that produced a fresh session (instead of reaching the founding
    /// one) would leave `attached_count` at 0 and the 2nd connection
    /// unable to reach the founding connection's binder.
    /// `RpcSessionId` keys (newtype) — type-explicit that the
    /// 32-byte map key is an *attach capability*, not just an opaque
    /// hash. Internal-only; public APIs continue to take `&[u8]` /
    /// `[u8; 32]` for compatibility.
    /// Holds a `Weak` of the founding `RpcSessionInner` itself so
    /// id-echoing attaches add a slot onto the *single* inner via
    /// [`RpcSession::add_incoming_slot`] —
    /// `state.remote_proxies`-cached `RpcProxy`s' `Weak<RpcSessionInner>`
    /// then point to the only inner and any server worker's nested
    /// `proxy.transact` `find_conn`s stay within its own slot pool
    /// (no cross-slot aliasing). The public API
    /// (`live_session_node_count`/`session_live_conns`) is byte-
    /// unchanged via `RpcSessionInner` delegate methods.
    sessions: Mutex<HashMap<RpcSessionId, std::sync::Weak<RpcSessionInner>>>,
    /// Observability counters. Plain atomics off the per-transaction
    /// path — zero-cost on the default (empty-id) flow.
    /// `session_registered` = new-session mints; `attached_count` =
    /// id-demux attaches; `rejected_unknown_id` = non-empty ids that
    /// resolved to no live session.
    session_registered: AtomicUsize,
    attached_count: AtomicUsize,
    rejected_unknown_id: AtomicUsize,
    shutdown: Arc<AtomicBool>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl RpcServer {
    /// Bind + listen on a Unix-domain socket path. A stale socket file
    /// at `path` is removed first (best effort).
    pub fn setup_unix_server(path: impl Into<PathBuf>) -> Result<Arc<RpcServer>> {
        let path = path.into();
        let _ = std::fs::remove_file(&path);
        // `StatusCode: From<std::io::Error>` — `?` converts directly.
        let listener = UnixListener::bind(&path)?;
        let listener = ServerListener::Unix(listener);
        // Non-blocking accept so the loop can observe `shutdown`.
        listener.set_nonblocking(true)?;
        Ok(Self::wrap(listener, BindAddress::Unix(path)))
    }

    /// Bind + listen on a Linux/Android abstract Unix-domain socket.
    /// Abstract sockets have no filesystem entry, so there is no stale
    /// path to remove and no drop-time unlink.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn setup_unix_server_abstract(name: &[u8]) -> Result<Arc<RpcServer>> {
        let addr = UnixSocketAddr::from_abstract_name(name)?;
        let listener = ServerListener::Unix(UnixListener::bind_addr(&addr)?);
        listener.set_nonblocking(true)?;
        Ok(Self::wrap(listener, BindAddress::UnixAbstract))
    }

    /// Bind + listen on a vsock `(cid, port)`. The
    /// returned `RpcServer` is otherwise identical to one built by
    /// [`setup_unix_server`](RpcServer::setup_unix_server): accept loop,
    /// authorizer hook, max-threads cap, session registry, and the
    /// android-13+ wire negotiation all run unchanged.
    ///
    /// **Address-family note**: vsock is Linux-kernel-only, and the
    /// `vsock` crate marks its types `cfg(any(target_os = "linux",
    /// target_os = "android"))`. Use `vsock::VMADDR_CID_LOCAL` for
    /// loopback (the `vsock_loopback` kernel module must be loaded on a
    /// host where there is no VM peer). For Android Virtualization
    /// Framework / Microdroid pVM scenarios the cid is the
    /// guest-assigned id.
    ///
    /// **Cleanup**: vsock has no filesystem entry, so `Drop` only flips
    /// the shutdown flag (the kernel reclaims the `(cid, port)` on the
    /// listener fd close).
    #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
    pub fn setup_vsock_server(cid: u32, port: u32) -> Result<Arc<RpcServer>> {
        let listener = vsock::VsockListener::bind_with_cid_port(cid, port).map_err(|e| {
            log::warn!("VsockListener::bind_with_cid_port({cid}, {port}) failed: {e}");
            crate::StatusCode::from(e)
        })?;
        let listener = ServerListener::Vsock(listener);
        listener.set_nonblocking(true)?;
        Ok(Self::wrap(listener, BindAddress::Vsock { cid, port }))
    }

    /// Backend-agnostic `RpcServer` construction. All factories
    /// (`setup_unix_server`, `setup_vsock_server`, and the TLS
    /// factories) funnel through here so the field set stays in one
    /// place.
    fn wrap(listener: ServerListener, bind: BindAddress) -> Arc<RpcServer> {
        // Build the directory root once over the shared `named` map; later
        // `add_service` inserts are seen through it with no rebuild.
        let named: Arc<Mutex<HashMap<String, SIBinder>>> = Arc::new(Mutex::new(HashMap::new()));
        let directory = Interface::as_binder(&Binder::new(ServiceDirectory {
            services: Arc::clone(&named),
        }));
        Arc::new(RpcServer {
            listener,
            bind,
            #[cfg(feature = "rpc-tls")]
            tls_config: Mutex::new(None),
            root: Mutex::new(None),
            named,
            directory,
            max_threads: Mutex::new(1),
            fd_unix_supported: AtomicBool::new(false),
            wire_max_version: Mutex::new(None),
            max_connections: Mutex::new(None),
            handshake_timeout: Mutex::new(Some(DEFAULT_HANDSHAKE_TIMEOUT)),
            idle_timeout: Mutex::new(None),
            authorizer: Mutex::new(None),
            attach_shutdown_probe: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
            session_registered: AtomicUsize::new(0),
            attached_count: AtomicUsize::new(0),
            rejected_unknown_id: AtomicUsize::new(0),
            shutdown: Arc::new(AtomicBool::new(false)),
            workers: Mutex::new(Vec::new()),
        })
    }

    /// UDS server with TLS. Same as
    /// [`setup_unix_server`](Self::setup_unix_server) plus a server-side
    /// `rustls::ServerConfig`; every accepted connection runs the TLS
    /// handshake on its worker thread (so a slow-handshake attacker
    /// stalls its own worker but never the accept loop — handshake
    /// budget is bounded by
    /// [`set_max_connections`](Self::set_max_connections)).
    ///
    /// `config` is the caller's `rustls::ServerConfig` (server cert
    /// chain + private key, optional mTLS client-cert verifier);
    /// rsbinder never invents crypto. Use
    /// [`super::rustls`](super::rustls) (re-export of the linked
    /// `rustls` version) to construct the config.
    #[cfg(feature = "rpc-tls")]
    pub fn setup_unix_server_tls(
        path: impl Into<PathBuf>,
        config: Arc<rustls::ServerConfig>,
    ) -> Result<Arc<RpcServer>> {
        let server = Self::setup_unix_server(path)?;
        *server.tls_config.lock().expect("tls_config poisoned") = Some(config);
        Ok(server)
    }

    /// TCP server with TLS. The TCP backend is
    /// **TLS-only** by design (plain-text network RPC is never
    /// production-appropriate; see [`super`] module doc). Use
    /// [`super::rustls`](super::rustls) to construct the config (server
    /// cert chain + private key, optional mTLS).
    #[cfg(feature = "rpc-tls")]
    pub fn setup_tcp_server_tls(
        addr: impl std::net::ToSocketAddrs,
        config: Arc<rustls::ServerConfig>,
    ) -> Result<Arc<RpcServer>> {
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?;
        let listener = ServerListener::Tcp(listener);
        listener.set_nonblocking(true)?;
        let server = Self::wrap(listener, BindAddress::Tcp(local));
        *server.tls_config.lock().expect("tls_config poisoned") = Some(config);
        Ok(server)
    }

    /// vsock server with TLS. Same as
    /// [`setup_vsock_server`](Self::setup_vsock_server) plus a
    /// server-side `rustls::ServerConfig`. The 1-tier Android AVF /
    /// Microdroid pVM target — vsock for the host↔guest socket plane,
    /// TLS for the crypto plane.
    #[cfg(all(
        feature = "rpc-tls",
        feature = "rpc-vsock",
        any(target_os = "linux", target_os = "android")
    ))]
    pub fn setup_vsock_server_tls(
        cid: u32,
        port: u32,
        config: Arc<rustls::ServerConfig>,
    ) -> Result<Arc<RpcServer>> {
        let server = Self::setup_vsock_server(cid, port)?;
        *server.tls_config.lock().expect("tls_config poisoned") = Some(config);
        Ok(server)
    }

    /// Snapshot of the current TLS config (cloned `Arc`), or `None` for
    /// a plain server. Called once per accepted connection so a worker
    /// gets a stable `Arc<ServerConfig>` for its whole lifetime.
    #[cfg(feature = "rpc-tls")]
    fn tls_snapshot(&self) -> Option<Arc<rustls::ServerConfig>> {
        self.tls_config.lock().expect("tls_config poisoned").clone()
    }

    /// Publish the single root object (android `setRootObject`).
    pub fn set_root(&self, binder: SIBinder) {
        *self.root.lock().expect("root poisoned") = Some(binder);
    }

    /// Register a named service. The first call installs a built-in
    /// `ServiceDirectory` as the root; that directory shares this server's
    /// service map, so every later call is an O(1) insert seen through the
    /// same root — no rebuild or root swap. Clients reach it via
    /// [`RpcSession::get_service`].
    pub fn add_service(&self, name: &str, binder: SIBinder) -> Result<()> {
        self.named
            .lock()
            .expect("named poisoned")
            .insert(name.to_string(), binder);
        // Install the once-built directory as the root (idempotent; shares
        // `named`, so the just-inserted entry is already visible through
        // it). Reinstalling makes `add_service` win over any prior
        // `set_root`.
        *self.root.lock().expect("root poisoned") = Some(self.directory.clone());
        Ok(())
    }

    /// Set the advertised max-threads value (AOSP-faithful
    /// `setMaxIncomingThreads`). Default 1.
    ///
    /// `n` has two roles, both always in effect:
    ///
    /// 1. **Advertised value**. Returned verbatim to a client on
    ///    `GET_MAX_THREADS`, so a peer's `negotiate(local_max)` sees
    ///    `min(local_max, n)`. AOSP-compatible.
    /// 2. **Incoming-slot cap**. The attach arm refuses id-echoing
    ///    attach attempts past `n` with `rejected_unknown_id` —
    ///    AOSP-faithful `setMaxIncomingThreads` (`RpcServer.cpp`
    ///    `session->setMaxIncomingThreads(server->mMaxThreads)`).
    ///
    /// Distinct from [`set_max_connections`](RpcServer::set_max_connections),
    /// which caps *concurrent connection-worker threads*
    /// across the **whole server**; this caps *incoming slots* within
    /// a **single session**. Both are additive — when both are active,
    /// the tighter cap wins.
    ///
    /// `N == 1` (default, single-connection) and `N >= 2` (multi-
    /// connection-per-session) are both validated against real
    /// android-13/14/15/16 libbinder peers.
    pub fn set_max_threads(&self, n: u32) {
        *self.max_threads.lock().expect("max_threads poisoned") = n.max(1);
    }

    /// Opt-in **server-side admission bound** on concurrent
    /// connection-worker threads (reactor-free backpressure). Default
    /// (unset) is unbounded — byte-for-byte the prior behavior, so this
    /// is purely additive. When set, the accept loop stops accepting
    /// while `n` workers are live; pending clients wait in the kernel
    /// listen backlog and are served as workers finish (no client is
    /// dropped, `shutdown` is still polled). `n` is clamped to ≥ 1.
    ///
    /// rsbinder is 1-connection = 1-session = 1-worker, so the bounded
    /// resource is the worker count; this is the rsbinder analogue of
    /// AOSP `RpcServer`'s bounded server limits, **not** a wire/semantic
    /// port. It does not (and structurally cannot, without I/O
    /// multiplexing) make workers fewer than connections.
    ///
    /// **Slot exhaustion**: each worker holds its admission slot until it
    /// exits, so a connected-but-silent peer would pin a slot forever
    /// without a read deadline. The default
    /// [`set_handshake_timeout`](RpcServer::set_handshake_timeout) guards
    /// against this; do not set it to `None` together with a small `n`
    /// unless the peer set is trusted.
    pub fn set_max_connections(&self, n: usize) {
        *self
            .max_connections
            .lock()
            .expect("max_connections poisoned") = Some(n.max(1));
    }

    /// Set (or disable) the **handshake/admission read deadline** applied
    /// to each accepted connection before it enters the blocking serve
    /// loop. Default [`DEFAULT_HANDSHAKE_TIMEOUT`] (10s). `Some(d)` ⇒ a
    /// connected-but-silent peer that never sends its handshake is
    /// dropped after `d`, releasing both its `Arc<RpcServer>` (so the
    /// server's `Drop` cleanup can run) and its
    /// [`set_max_connections`](RpcServer::set_max_connections) admission
    /// slot — without it, a few idle peers can exhaust the cap and wedge
    /// the accept loop. `None` disables the deadline (a hung peer may
    /// then hold a slot indefinitely). The deadline is armed on **both**
    /// the read and write sides of the handshake, so a peer that is
    /// admitted but then refuses to read our handshake reply (stalling our
    /// blocking `write_all` once its receive window fills) is bounded too,
    /// not just a peer that refuses to send.
    ///
    /// The deadline bounds **only** the handshake/first-contact phase. For
    /// the android-13+ profile it is cleared after the explicit handshake;
    /// for the default r34 profile (no separate handshake) it covers the
    /// first serve-loop frame and is cleared once that frame is read. Either
    /// way an established two-way session may then sit idle between requests
    /// unbounded (the per-call reply deadline is managed separately via
    /// [`RpcSession::set_timeout`](super::RpcSession::set_timeout)).
    pub fn set_handshake_timeout(&self, timeout: Option<std::time::Duration>) {
        *self
            .handshake_timeout
            .lock()
            .expect("handshake_timeout poisoned") = timeout;
    }

    /// Set (or disable) the **idle read deadline** applied to the
    /// android-13+ serve loop *after* the handshake completes. Default
    /// `None` ⇒ an established session may idle between requests unbounded
    /// (byte-identical to prior behavior).
    ///
    /// [`set_handshake_timeout`](Self::set_handshake_timeout) only bounds
    /// the handshake/first-contact phase; once a peer completes the
    /// handshake it can then go silent and hold its worker — and, under
    /// [`set_max_connections`](Self::set_max_connections), an admission
    /// slot — indefinitely (a post-handshake Slowloris that starves the
    /// accept loop). Setting a `Some(d)` idle timeout evicts such a peer
    /// after `d` of silence, freeing the slot. Use it when the protocol
    /// has regular traffic or idle eviction is acceptable; pair it with
    /// `set_max_connections` for untrusted peers, since OS-level TCP
    /// keepalive/timeouts are otherwise the only backstop. (Currently
    /// honored on the android-13+ serve path; the r34 profile already
    /// bounds its first frame via the handshake deadline.)
    ///
    /// The serve phase arms this value on the **write** side too, so a peer
    /// that stops draining replies is evicted as well — but a consumer that
    /// legitimately reads a large reply slower than `d` is also dropped
    /// mid-send (the connection is torn down, not desynced). Size `d`
    /// against the slowest acceptable consumer, not just the idle gap.
    pub fn set_idle_timeout(&self, timeout: Option<std::time::Duration>) {
        *self.idle_timeout.lock().expect("idle_timeout poisoned") = timeout;
    }

    /// Reap finished worker handles and return the live (concurrent)
    /// count. Shared by [`serve_connection`](RpcServer::serve_connection)
    /// (bounds `workers` by concurrent, not cumulative, connections)
    /// and the accept-loop admission gate.
    fn live_worker_count(&self) -> usize {
        let mut workers = self.workers.lock().expect("workers poisoned");
        workers.retain(|h| !h.is_finished());
        workers.len()
    }

    /// Opt-in **authorization hook**. `f` is
    /// invoked once per accepted connection with the peer's
    /// [`PeerIdentity`] **before any RPC byte is exchanged**; returning
    /// `false` closes the connection immediately (the peer's next op
    /// sees `DeadObject` — RPC payload zero bytes, the local-transport
    /// analogue of a TLS reject). Unset (default) =
    /// accept-all = byte-for-byte a server without the hook — so
    /// this is purely additive and gives opt-in mutual authentication
    /// with no cost when off.
    ///
    /// rsbinder provides only the gate; the policy is the caller's
    /// closure, e.g.
    /// `|p| p.uid() == Some(EXPECTED_UID)` or, with the
    /// `rpc-macos-codesign` feature,
    /// `matches!(p, PeerIdentity::CodeSigned(c) if c.team_id() == Some("TEAMID"))`.
    /// Backend-independent (unix/mem/tls/vsock). The hook must not
    /// block indefinitely (it runs on the accept path).
    pub fn set_authorizer<F>(&self, f: F)
    where
        F: Fn(&PeerIdentity) -> bool + Send + Sync + 'static,
    {
        *self.authorizer.lock().expect("authorizer poisoned") = Some(Arc::new(f));
    }

    /// Shutdown-reject e2e scaffolding (test-only,
    /// `#[doc(hidden)]`). Install a barrier the android-13+ attach
    /// worker invokes *after* a successful handshake and *before* the
    /// `shutdown` gate read, turning the production race window
    /// into a deterministic test point. The closure runs lock-free
    /// (cloned out of the field's mutex first), so it may re-enter
    /// `server` without self-deadlock. `None` (default, no
    /// `__set_attach_shutdown_probe` call) = byte-identical to the
    /// pre-hook attach path. Same `__`-prefix unstable-API discipline
    /// as `__fuzz_decode_rpc_parcel`; not part of the supported API
    /// surface.
    #[doc(hidden)]
    pub fn __set_attach_shutdown_probe<F>(&self, f: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self
            .attach_shutdown_probe
            .lock()
            .expect("attach_shutdown_probe poisoned") = Some(Arc::new(f));
    }

    /// Run the `#[doc(hidden)]` attach-shutdown probe (no-op when
    /// unset). Clones the `Arc<dyn Fn>` out of the mutex first so the
    /// closure runs **lock-free** (the closure may re-enter `server`
    /// without self-deadlock — same discipline as `authorizer`).
    fn run_attach_shutdown_probe(&self) {
        let probe = self
            .attach_shutdown_probe
            .lock()
            .expect("attach_shutdown_probe poisoned")
            .clone();
        if let Some(p) = probe {
            p();
        }
    }

    /// Advertise the FD-over-RPC modes this server will accept.
    /// Default: only `None` (the categorical reject). Pass
    /// `&[FileDescriptorTransportMode::Unix]` to opt in to UDS
    /// `SCM_RIGHTS` fd passing for clients that also opt in.
    pub fn set_supported_fd_modes(&self, modes: &[crate::rpc::FileDescriptorTransportMode]) {
        let unix = modes.contains(&crate::rpc::FileDescriptorTransportMode::Unix);
        self.fd_unix_supported.store(unix, Ordering::SeqCst);
    }

    /// Opt in to the **android-13+ versioned RPC wire**.
    /// `max_version` is the highest `RPC_WIRE_PROTOCOL_VERSION`
    /// this server offers (`0` = android-13, `1` = android-14/15,
    /// **`2` = android-16**); each accepted connection
    /// then runs the AOSP connection handshake and negotiates
    /// `min(max_version, client_max)`. Default (unset) keeps the
    /// android-12 r34 wire, byte-unchanged. Has effect only on a
    /// transport with raw byte access (`unix`).
    ///
    /// **Sequencing:** advertising `2` is sound
    /// only because the Parcel binder/FD object-position producer
    /// (`Parcel::rpc_record_object_position`, the
    /// `records_binder_positions`/`records_fd_positions` profile gate)
    /// is compiled in unconditionally here. Without that producer,
    /// `2` would frame a *binder-bearing* parcel with an empty
    /// object table and a real libbinder v2 peer would `BAD_VALUE` it;
    /// no-object traffic is v1≡v2 byte-identical and safe at any
    /// version. Negotiating down to v0/v1 against an older peer stays
    /// correct (the codec is version-keyed).
    pub fn set_android13plus(&self, max_version: u32) {
        *self
            .wire_max_version
            .lock()
            .expect("wire_max_version poisoned") = Some(max_version);
    }

    /// Apply this server's shared root + negotiated max-threads + FD
    /// policy to a freshly-built per-connection session (its `RpcState`
    /// is fresh — isolated). Shared by the r34 and android-13+
    /// connection paths.
    fn configure_session(&self, session: &RpcSession) {
        if let Some(root) = self.root.lock().expect("root poisoned").clone() {
            session.set_root(root);
        }
        session.set_max_threads(*self.max_threads.lock().expect("max_threads poisoned"));
        if self.fd_unix_supported.load(Ordering::SeqCst) {
            session.set_supported_fd_modes(&[crate::rpc::FileDescriptorTransportMode::Unix]);
        }
    }

    /// Build a per-connection r34 session sharing this server's root +
    /// negotiated max-threads (its `RpcState` is fresh — isolated).
    fn make_session(&self, transport: Box<dyn RpcTransport>) -> super::RpcResult<RpcSession> {
        // The server accepted this connection ⇒ Acceptor subspace.
        let session = RpcSession::new(transport, super::address::AddressSpace::Acceptor)?;
        self.configure_session(&session);
        Ok(session)
    }

    // --- session-id → shared-session registry

    /// Register a newly-minted session's founding `RpcSessionInner`
    /// under its 32-byte id (new-session / empty-id accept path). Stored
    /// as a `Weak` so a fully-torn-down session (last slot exit drops
    /// the `Arc<RpcSessionInner>`) does not pin memory, and its id, if
    /// later echoed, resolves to "unknown". Holds a
    /// `Weak<RpcSessionInner>` so the attach path adds a slot onto the
    /// founding inner directly.
    fn register_session(&self, id: RpcSessionId, inner: &Arc<RpcSessionInner>) {
        let mut map = self.sessions.lock().expect("sessions poisoned");
        // Opportunistically prune fully-dead sessions so the map is
        // bounded by *live* sessions, not cumulative over the server's
        // lifetime (random 32-byte ids never collide in practice, so a
        // dead `Weak` would otherwise linger forever). Explicit
        // `unregister_session` is unnecessary — the founding worker's
        // exit is no longer the session's death (any *last* slot exit
        // is), so prune-on-register suffices.
        map.retain(|_, w| w.strong_count() > 0);
        map.insert(id, Arc::downgrade(inner));
        drop(map);
        self.session_registered.fetch_add(1, Ordering::SeqCst);
    }

    /// Resolve a client-echoed id to a **live** founding inner
    /// (id-demux, returning the `RpcSessionInner` so the attach path
    /// can `add_incoming_slot` on it directly). `None` for any
    /// non-32-byte id (AOSP
    /// `kSessionIdBytes == 32`), an unknown id, or a stale `Weak`
    /// (session fully torn down) — all of which the caller rejects.
    fn resolve_session(&self, id: &[u8]) -> Option<Arc<RpcSessionInner>> {
        let key = RpcSessionId::try_from_slice(id)?;
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .get(&key)
            .and_then(std::sync::Weak::upgrade)
    }

    /// Observability counters.
    /// Respectively: new-session ids registered; **id-demux attaches**
    /// (a 2nd+ connection bound to a pre-existing shared
    /// session); non-empty ids that resolved to no live session and
    /// were rejected. All zero on the default (empty-id) flow ⇒ a
    /// no-regression witness.
    pub fn session_registered_count(&self) -> usize {
        self.session_registered.load(Ordering::SeqCst)
    }
    pub fn attached_count(&self) -> usize {
        self.attached_count.load(Ordering::SeqCst)
    }
    pub fn rejected_unknown_id_count(&self) -> usize {
        self.rejected_unknown_id.load(Ordering::SeqCst)
    }

    /// Leak observability: total live local-node count
    /// across all currently-live registered sessions (dead `Weak`s
    /// skipped). The AOSP `timesSent`/`flushExcessBinderRefs` books
    /// must net to **0** once every client proxy is dropped — a value
    /// stuck above baseline indicates a leaked excess `DEC_STRONG`.
    ///
    /// Lock ladder: collect the live `Arc<RpcSessionInner>` snapshot
    /// **first** (releasing the `sessions` mutex), then walk each
    /// session's `state` mutex (via the inner's `local_node_count`
    /// delegate). Avoids the nested-lock pattern (`sessions` → `state`); a
    /// poisoned `state` lock in one session no longer poisons
    /// `sessions` as a side-effect.
    pub fn live_session_node_count(&self) -> usize {
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .expect("sessions poisoned")
            .values()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        sessions.iter().map(|s| s.local_node_count()).sum()
    }

    /// Deterministic teardown witness: live connection count of the
    /// session keyed by `id`. `None` ⇒ no live session with that id
    /// (fully torn down or never registered). Lets tests `poll_until`
    /// for the server-side `serve_blocking_on` exit hook (which
    /// transitions the typed `SessionLifecycle` `Live(n) → Live(n-1)`
    /// or `Live(1) → Dying`) without a `sleep(N ms)`
    /// heuristic that races scheduler jitter.
    pub fn session_live_conns(&self, id: &[u8; 32]) -> Option<usize> {
        // Public API keeps the raw-byte shape (internal-only newtype);
        // wrap inline for the map lookup.
        let key = RpcSessionId::new(*id);
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .get(&key)
            .and_then(std::sync::Weak::upgrade)
            .map(|s| s.live_conn_count())
    }

    /// Slot-count witness: count of slots in the
    /// founding `RpcSessionInner`'s pool for the session keyed by `id`.
    /// `None` ⇒ no live session with that id. Each
    /// id-echoing attached connection adds a slot here (single inner
    /// per session). A topology that built a fresh inner per attached
    /// connection would leave the founding inner at one slot, so a
    /// test that establishes (founding + attached = 2) and asserts
    /// `Some(2)` here is satisfied only by the unified topology.
    pub fn session_slot_count(&self, id: &[u8; 32]) -> Option<usize> {
        let key = RpcSessionId::new(*id);
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .get(&key)
            .and_then(std::sync::Weak::upgrade)
            .map(|s| s.slot_count())
    }

    /// Serve one already-connected transport on its own worker thread
    /// (used by in-memory tests and by [`super::session`] direct calls).
    /// The accept loop uses [`serve_connection_raw`](Self::serve_connection_raw)
    /// to keep TLS handshake (if any) on the worker side.
    pub fn serve_connection(self: &Arc<Self>, transport: Box<dyn RpcTransport>) {
        let server = Arc::clone(self);
        let handle = std::thread::spawn(move || {
            Self::run_connection_in_worker(server, transport);
        });
        let mut workers = self.workers.lock().expect("workers poisoned");
        // Reap finished handles so `workers` is bounded by *concurrent*
        // (not cumulative) connections — same discipline as the accept
        // loop's reaping.
        workers.retain(|h| !h.is_finished());
        workers.push(handle);
    }

    /// Accept loop entry point. Takes the raw
    /// accepted stream (the kernel `accept(2)` result, before any
    /// blocking I/O on the socket), spawns a worker thread, and wraps
    /// the stream as `RpcTransport` *inside* that worker — so a
    /// potentially-expensive TLS handshake never stalls the accept
    /// loop. The handshake budget is bounded by
    /// [`set_max_connections`](Self::set_max_connections) (the worker-
    /// thread cap also bounds the in-flight handshake count). Plain
    /// transports (UDS / vsock) skip the TLS branch and wrap natively.
    fn serve_connection_raw(self: &Arc<Self>, raw: RawAccepted) {
        let server = Arc::clone(self);
        let handle = std::thread::spawn(move || {
            // Bound the pre-wrap handshake phase on the raw socket before
            // any blocking I/O. The TLS handshake runs inside
            // `wrap_accepted` (below), *before* `run_connection_in_worker`
            // arms its deadline, so a silent peer would otherwise pin this
            // worker — and, with `set_max_connections`, the accept loop —
            // forever. `run_connection_in_worker` re-arms (idempotent) and
            // clears it before the long-lived serve.
            if let Some(d) = *server
                .handshake_timeout
                .lock()
                .expect("handshake_timeout poisoned")
            {
                if let Err(e) = raw.set_read_timeout(Some(d)) {
                    log::debug!("RPC: failed to arm pre-wrap handshake read timeout: {e:?}");
                }
                if let Err(e) = raw.set_write_timeout(Some(d)) {
                    log::debug!("RPC: failed to arm pre-wrap handshake write timeout: {e:?}");
                }
            }
            let transport = match server.wrap_accepted(raw) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("RPC transport wrap (TLS or native) failed: {e:?}");
                    return;
                }
            };
            Self::run_connection_in_worker(server, transport);
        });
        let mut workers = self.workers.lock().expect("workers poisoned");
        workers.retain(|h| !h.is_finished());
        workers.push(handle);
    }

    /// Worker-thread helper that wraps a `RawAccepted` as
    /// `Box<dyn RpcTransport>`. Two cfg variants so the function
    /// signature stays uniform — the snapshot of `tls_config` happens
    /// here (worker thread) rather than at accept time; that's safe
    /// because `tls_config` is set only by the factories
    /// (`setup_*_server_tls`) before the server is shared as `Arc`,
    /// so it's effectively immutable from the accept loop's PoV.
    #[cfg(feature = "rpc-tls")]
    fn wrap_accepted(&self, raw: RawAccepted) -> RpcResult<Box<dyn RpcTransport>> {
        raw.into_transport(self.tls_snapshot())
    }
    #[cfg(not(feature = "rpc-tls"))]
    fn wrap_accepted(&self, raw: RawAccepted) -> RpcResult<Box<dyn RpcTransport>> {
        raw.into_transport()
    }

    /// Transition a connection from the bounded handshake/admission phase
    /// to the long-lived serving phase (best-effort), arming **both** the
    /// read and write deadlines. By default both are lifted (`None`), so an
    /// established two-way session may idle between requests unbounded —
    /// byte-identical to prior behavior. If
    /// [`set_idle_timeout`](Self::set_idle_timeout) was called, the serve
    /// loop inherits that value on each side, so a peer that completes the
    /// handshake and then goes silent (or stops reading our replies) — a
    /// post-handshake Slowloris that would otherwise pin a
    /// [`set_max_connections`](Self::set_max_connections) admission slot
    /// forever — surfaces as a serve-loop error and is evicted.
    fn arm_serve_timeouts(&self, transport: &dyn RpcTransport) {
        let idle = *self.idle_timeout.lock().expect("idle_timeout poisoned");
        if let Err(e) = transport.set_read_timeout(idle) {
            log::debug!("RPC: failed to set serve-phase read timeout: {e:?}");
        }
        // Mirror the deadline onto the write side so a peer that idles
        // *and* stops reading can't pin the worker via a blocked reply
        // send. `None` (the default) leaves writes unbounded — byte-
        // identical to prior behavior; a configured idle timeout now
        // bounds both directions.
        if let Err(e) = transport.set_write_timeout(idle) {
            log::debug!("RPC: failed to set serve-phase write timeout: {e:?}");
        }
    }

    /// Runs **inside** the worker thread after the transport has been
    /// wrapped (native or TLS). Performs authorization, then dispatches
    /// to the r34 / android-13+ branch and serves the session inline
    /// (no nested spawn — we're already on the worker thread).
    fn run_connection_in_worker(server: Arc<Self>, transport: Box<dyn RpcTransport>) {
        // Authorization gate. The single
        // chokepoint common to r34, android-13+, AND in-memory test
        // direct calls — *before* the wire-profile branch, session
        // build, handshake, or any `recv_frame`, so a rejected peer
        // gets zero RPC bytes. Default (unset) ⇒ no-op, byte-identical
        // (additive). On a TLS server the peer identity is
        // already final here (the TLS handshake completed in
        // `into_transport`, so `transport.peer_identity()` returns the
        // post-handshake `Certificate` or `Anonymous`).
        let authorizer = server
            .authorizer
            .lock()
            .expect("authorizer poisoned")
            .clone();
        if let Some(authz) = authorizer {
            let peer = transport.peer_identity();
            if !authz(&peer) {
                log::warn!("RPC connection rejected by authorizer: peer {peer:?}");
                return;
            }
        }
        // Bound the pre-serve handshake/first-contact phase so a
        // connected-but-silent peer can't pin its `Arc<RpcServer>` +
        // admission slot forever. Transitioned to the serve-phase deadline
        // by `arm_serve_timeouts` before any long-lived serve (`None`
        // ⇒ idle unbounded, or the configured `set_idle_timeout`).
        // Best-effort: a set failure just means no deadline.
        if let Some(d) = *server
            .handshake_timeout
            .lock()
            .expect("handshake_timeout poisoned")
        {
            if let Err(e) = transport.set_read_timeout(Some(d)) {
                log::debug!("RPC: failed to arm handshake read timeout: {e:?}");
            }
            if let Err(e) = transport.set_write_timeout(Some(d)) {
                log::debug!("RPC: failed to arm handshake write timeout: {e:?}");
            }
        }
        let a13_max = *server
            .wire_max_version
            .lock()
            .expect("wire_max_version poisoned");
        match a13_max {
            Some(max) => {
                // android-13+: the AOSP connection handshake is
                // blocking I/O on the accepted socket. We're already in
                // the worker — handshake/serve inline (no nested spawn).
                // The AOSP handshake reads the
                // client's `RpcConnectionHeader.fileDescriptorTransport
                // Mode`; honor `Unix` only if this server opted in
                // (`set_supported_fd_modes`) — else degrade to `None`
                // (the fd write then `BAD_TYPE`-rejects). `false` keeps
                // the byte-identical no-FD android-13+ path.
                let fd_unix = server.fd_unix_supported.load(Ordering::SeqCst);
                // Split handshake from build so we can branch on
                // the client-supplied session id (new vs attach vs
                // reject) and direction (outgoing vs incoming).
                let (transport, codec, client_fd_mode, client_id, incoming) =
                    match RpcSession::android13plus_accept_handshake(transport, max) {
                        Ok(parts) => parts,
                        Err(e) => {
                            // Abnormal interop/security event
                            // (version mismatch, truncated header,
                            // hostile peer) — `warn!` not `debug!`.
                            log::warn!("android-13+ RPC handshake failed: {e:?}");
                            return;
                        }
                    };
                // Handshake done: transition the admission deadline to the
                // serve-phase deadline — `None` (default, unbounded idle) or
                // the configured `set_idle_timeout` so a post-handshake
                // silent peer is evicted instead of pinning its slot.
                server.arm_serve_timeouts(transport.as_ref());
                if incoming {
                    // Attach + incoming (`server_accept` already
                    // rejected new + incoming): resolve the session,
                    // register a callback slot, and exit the worker.
                    // The slot lives in the pool for server→client
                    // sends; there is no read loop because
                    // client→server traffic only uses outgoing
                    // connections (per AOSP `RpcSession::mConnections.mOutgoing`).
                    match server.resolve_session(&client_id) {
                        Some(inner) => {
                            if inner.wire_protocol_version() != Some(codec.version()) {
                                server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                                log::warn!(
                                    "android-13+ RPC: incoming attach codec version {} \
                                     ≠ founding inner version {:?}; rejecting",
                                    codec.version(),
                                    inner.wire_protocol_version()
                                );
                                drop(transport);
                                return;
                            }
                            // Shutdown-reject e2e scaffolding (see
                            // `__set_attach_shutdown_probe`). No-op
                            // unless a test installed a barrier.
                            server.run_attach_shutdown_probe();
                            if server.shutdown.load(Ordering::SeqCst) {
                                server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                                log::warn!(
                                    "android-13+ RPC: incoming attach after server \
                                     shutdown; rejecting"
                                );
                                drop(transport);
                                return;
                            }
                            // Bound callback (incoming) slots. Unlike outgoing
                            // attaches — which carry a serve loop and are
                            // reclaimed by `remove_slot` on disconnect — a
                            // callback slot has no read loop and lives until
                            // the whole session tears down, so without a cap a
                            // peer holding the session id could grow the slot
                            // pool (and its held fds) without bound. AOSP opens
                            // symmetric incoming+outgoing connections (each
                            // bounded by the negotiated max-threads), so cap the
                            // shared pool at `2 * max_threads` — tight enough to
                            // bound the DoS, loose enough never to refuse a
                            // well-behaved client's callback connections.
                            //
                            // The cap is enforced atomically inside
                            // `add_callback_slot` (check-and-push under the
                            // `conn_state` lock) rather than via a separate
                            // pre-check, so concurrent attach workers cannot
                            // each clear an advisory check and overshoot it.
                            let incoming_cap =
                                (inner.max_threads_value() as usize).saturating_mul(2);
                            let session = RpcSession::wrap_inner(inner);
                            if let Err(e) = session.add_callback_slot(transport, incoming_cap) {
                                server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                                log::warn!(
                                    "android-13+ RPC: incoming callback attach refused \
                                     (slot cap {incoming_cap} reached or session torn \
                                     down): {e:?}"
                                );
                            }
                        }
                        None => {
                            server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                            log::warn!(
                                "android-13+ RPC: incoming connection supplied an \
                                 unknown/stale session id; rejecting"
                            );
                            drop(transport);
                        }
                    }
                    return;
                }
                if client_id.is_empty() {
                    // New session: mint, register, serve. Registry
                    // entry is `Weak`; reclaimed by the next
                    // `register_session` prune when the founding
                    // `Arc<RpcSessionInner>` is dropped.
                    let session = match RpcSession::from_android13plus(
                        transport,
                        codec,
                        client_fd_mode,
                        fd_unix,
                        None,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            log::warn!("android-13+ RPC: from_android13plus failed: {e:?}");
                            return;
                        }
                    };
                    let id = RpcSessionId::new(session.session_id());
                    server.register_session(id, &session.inner_arc());
                    server.configure_session(&session);
                    if let Err(e) = session.serve_blocking() {
                        log::debug!("RPC session ended: {e:?}");
                    }
                } else if let Some(inner) = server.resolve_session(&client_id) {
                    // Attach: add a slot on the founding inner so
                    // proxy-cache + slot-pool stay unified (no
                    // cross-slot aliasing). Reject on profile
                    // mismatch — codec version is immutable for
                    // the session.
                    if inner.wire_protocol_version() != Some(codec.version()) {
                        server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                        log::warn!(
                            "android-13+ RPC: attach codec version {} ≠ \
                             founding inner version {:?}; rejecting",
                            codec.version(),
                            inner.wire_protocol_version()
                        );
                        drop(transport);
                        return;
                    }
                    // Shutdown-reject e2e scaffolding
                    // (see `__set_attach_shutdown_probe`). No-op
                    // unless a test installed a barrier; the
                    // production race window sits exactly between
                    // this point and the `load` below.
                    server.run_attach_shutdown_probe();
                    // Shutdown gate: refuse attaches
                    // once the server is shutting down (clean
                    // teardown semantics — a late-arriving
                    // id-echoing client must not be allowed to
                    // hook onto a session whose worker pool is
                    // already winding down).
                    if server.shutdown.load(Ordering::SeqCst) {
                        server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                        log::warn!("android-13+ RPC: attach after server shutdown; rejecting");
                        drop(transport);
                        return;
                    }
                    // AOSP-faithful `setMaxIncomingThreads` cap; see
                    // `RpcServer::set_max_threads` rustdoc for the
                    // advertise vs. slot-cap split.
                    //
                    // Race: this check and the subsequent
                    // `add_incoming_slot` are two separate critical
                    // sections, so concurrent attach workers can
                    // transiently overshoot `cap` by up to (N − 1),
                    // bounded by `set_max_connections` (default
                    // unlimited). A check-and-increment atomic would
                    // tighten this further.
                    let cap = inner.max_threads_value() as usize;
                    if inner.slot_count() >= cap {
                        server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                        log::warn!(
                            "android-13+ RPC: attach refused (incoming slot \
                             cap reached: slot_count={}, max_threads={})",
                            inner.slot_count(),
                            cap
                        );
                        drop(transport);
                        return;
                    }
                    // `add_incoming_slot` atomically combines the
                    // anti-resurrection gate (`try_bump_live_
                    // conns`) with the slot enqueue.
                    let session = RpcSession::wrap_inner(inner);
                    let slot_id = match session.add_incoming_slot(transport) {
                        Ok(id) => id,
                        Err(e) => {
                            server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                            log::warn!(
                                "android-13+ RPC: session torn down between \
                                 resolve and attach (F4 race); rejecting: {e:?}"
                            );
                            return;
                        }
                    };
                    // Bump *after* `add_incoming_slot` succeeded
                    // so external observers never see a count for
                    // a slot that never reached the pool.
                    server.attached_count.fetch_add(1, Ordering::SeqCst);
                    if let Err(e) = session.serve_blocking_on(slot_id) {
                        log::debug!("RPC attached connection ended: {e:?}");
                    }
                } else {
                    server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                    log::warn!(
                        "android-13+ RPC: client supplied an unknown/stale \
                         session id; rejecting connection"
                    );
                    drop(transport);
                }
            }
            None => {
                // r34 (default): build session (incl. its handshake-
                // free first-contact shape) + serve inline. We're
                // already on the worker thread — no nested spawn.
                // r34 has no separate handshake (first contact is the
                // first serve-loop frame), so the admission deadline must
                // cover that first frame: a silent peer times out and
                // releases its Arc + slot. `make_session`/`RpcSession::new`
                // do no blocking read, so the still-armed deadline reaches
                // the serve loop, which clears it after the first frame so
                // an established idle session is not torn down by it.
                let session = match server.make_session(transport) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("RPC r34: make_session failed: {e:?}");
                        return;
                    }
                };
                if let Err(e) = session.serve_blocking_clearing_deadline_after_first() {
                    log::debug!("RPC session ended: {e:?}");
                }
            }
        }
    }

    /// Run the accept loop until [`RpcServer::shutdown`]. Each accepted
    /// connection gets its own session + worker thread.
    pub fn run(self: &Arc<Self>) -> Result<()> {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            // Admission bound (opt-in; `None` ⇒ skip entirely, prior
            // behavior bit-identical). At capacity we simply don't
            // accept this iteration: pending clients wait in the kernel
            // listen backlog (reactor-free backpressure, no client
            // dropped). `continue` re-checks `shutdown` every tick, so
            // a full server still shuts down promptly. `live_worker_
            // count()` reaps finished handles, so a freed slot is
            // observed here.
            if let Some(max) = *self
                .max_connections
                .lock()
                .expect("max_connections poisoned")
            {
                if self.live_worker_count() >= max {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
            }
            match self.listener.accept_raw() {
                Ok(raw) => {
                    // `accept_raw` returns the raw stream without
                    // wrapping; `serve_connection_raw` spawns the
                    // worker and wraps the stream (native or TLS)
                    // *inside* the worker — so TLS handshake never
                    // stalls the accept loop.
                    self.serve_connection_raw(raw);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Listener is non-blocking only so we can poll
                    // `shutdown`; no pending connection.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    // Transient: peer reset between SYN and accept()
                    // (ECONNABORTED), or EINTR. A normal accept loop
                    // continues past these — they must NOT take the
                    // whole server down for all future clients.
                    log::warn!("transient accept error, continuing: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(code)
                            if code == libc::EMFILE
                                || code == libc::ENFILE
                                || code == libc::ENOMEM
                                || code == libc::ENOBUFS
                    ) =>
                {
                    // Resource exhaustion: the process or system fd table
                    // is full (EMFILE/ENFILE) or the kernel is out of
                    // memory/buffers (ENOMEM/ENOBUFS). A peer that churns
                    // connections can drive us to RLIMIT_NOFILE, at which
                    // point `accept` returns EMFILE. These map to
                    // `ErrorKind::Uncategorized`/`OutOfMemory`, so without
                    // this arm they fall through to the fatal branch and
                    // kill the listener for ALL future clients — turning a
                    // transient overload into a permanent outage. The
                    // condition is self-healing as in-flight sessions close
                    // their fds, so back off (longer than the EINTR case to
                    // give descriptors time to free) and keep serving.
                    log::warn!("accept resource exhaustion, backing off: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    // Fatal (e.g. the listener was closed): surface it.
                    // Never disguise a hard failure as `Ok(())`, which
                    // would make `run_background` silently dead.
                    log::debug!("accept loop ending (fatal): {e}");
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }

    /// Spawn the accept loop on a background thread; returns its handle.
    pub fn run_background(self: &Arc<Self>) -> JoinHandle<()> {
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let _ = me.run();
        })
    }

    /// Request shutdown: stop accepting and let in-flight sessions
    /// drain as their peers disconnect.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Join all session workers (call after the clients disconnect).
    ///
    /// `Drop` only flips the shutdown flag and removes the socket — it
    /// deliberately does **not** join in-flight session workers (they
    /// drain on peer close). A worker that panicked is therefore only
    /// observable through this call: for clean shutdown and worker
    /// error/panic observability, call `join_workers` explicitly rather
    /// than relying on `Drop`.
    pub fn join_workers(&self) {
        let handles: Vec<_> = std::mem::take(&mut *self.workers.lock().expect("workers poisoned"));
        for h in handles {
            let _ = h.join();
        }
    }

    /// The bound socket path for a Unix-domain server. `None` for other
    /// backends (vsock, TCP+TLS) — the listener has no filesystem entry
    /// to expose.
    pub fn path(&self) -> Option<&Path> {
        match &self.bind {
            BindAddress::Unix(p) => Some(p.as_path()),
            BindAddress::UnixAbstract => None,
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            BindAddress::Vsock { .. } => None,
            #[cfg(feature = "rpc-tls")]
            BindAddress::Tcp(_) => None,
        }
    }

    /// Bound vsock address for a vsock server.
    /// `None` for other backends. Available only on platforms where the
    /// vsock backend is compiled in (Linux / Android).
    #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
    pub fn vsock_address(&self) -> Option<(u32, u32)> {
        match &self.bind {
            BindAddress::Vsock { cid, port } => Some((*cid, *port)),
            BindAddress::Unix(_) => None,
            BindAddress::UnixAbstract => None,
            #[cfg(feature = "rpc-tls")]
            BindAddress::Tcp(_) => None,
        }
    }

    /// Bound TCP socket address for a
    /// [`setup_tcp_server_tls`](Self::setup_tcp_server_tls) server.
    /// `None` for other backends. Useful when the caller bound port
    /// `0` and needs to learn the kernel-assigned port.
    #[cfg(feature = "rpc-tls")]
    pub fn tcp_address(&self) -> Option<SocketAddr> {
        match &self.bind {
            BindAddress::Tcp(addr) => Some(*addr),
            BindAddress::Unix(_) => None,
            BindAddress::UnixAbstract => None,
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            BindAddress::Vsock { .. } => None,
        }
    }
}

impl Drop for RpcServer {
    /// `Drop` is best-effort: it flips the `shutdown` flag (which the
    /// accept loop polls each tick) and removes the bound socket
    /// file. It does **not** join in-flight session workers — they
    /// drain on peer close.
    ///
    /// **Caveat**: each `serve_connection`
    /// worker closure captures `Arc::clone(self)` for the duration of
    /// the session (so it can call `server.shutdown.load(…)`,
    /// `server.register_session(…)`, etc.). For a *hung* peer that
    /// never closes the connection, the worker holds a strong
    /// reference indefinitely — the last external `Arc<RpcServer>`
    /// going out of scope does **not** trigger this `Drop` until the
    /// worker also releases its clone (peer close, kernel reset,
    /// etc.). For deterministic teardown call
    /// [`RpcServer::shutdown`] **and** [`RpcServer::join_workers`]
    /// explicitly, or impose a `set_read_timeout` so a stalled peer
    /// surfaces as a worker-loop error instead of an indefinite
    /// hold. Closing socket-level paths so the kernel times out the
    /// peer is also sufficient. (Using `Weak<Self>` plus periodic
    /// upgrade-checks in worker hot paths would remove the hold
    /// entirely, at the cost of a larger refactor.)
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Best-effort backend-specific cleanup; never panic in Drop.
        match &self.bind {
            BindAddress::Unix(p) => {
                // Remove the UDS file so a follow-up `setup_unix_server`
                // on the same path doesn't see a stale ENOENT/EADDRINUSE.
                let _ = std::fs::remove_file(p);
            }
            BindAddress::UnixAbstract => {}
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            BindAddress::Vsock { .. } => {
                // vsock has no filesystem entry; the kernel reclaims the
                // (cid, port) when the listener fd is closed (the
                // listener is owned by `self.listener` so Drop closes it
                // for us — no explicit step needed).
            }
            #[cfg(feature = "rpc-tls")]
            BindAddress::Tcp(_) => {
                // TCP has no filesystem entry; the kernel reclaims the
                // bound port when the listener fd is closed.
            }
        }
    }
}

impl RpcSession {
    /// Client: resolve a named service published via
    /// [`RpcServer::add_service`].
    pub fn get_service(&self, name: &str) -> Result<SIBinder> {
        let root = self.get_root()?;
        let rp = (*root)
            .as_any()
            .downcast_ref::<super::proxy::RpcProxy>()
            .ok_or(StatusCode::BadType)?;
        let mut data = rp.build_request(DIRECTORY_DESC)?;
        data.write(&name)?;
        let mut reply = rp
            .transact(TX_GET_SERVICE, &data, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        let st: crate::Status = reply.read()?;
        if !st.is_ok() {
            return Err(StatusCode::from(st));
        }
        reply.read::<SIBinder>()
    }

    /// Client: resolve a named service published via
    /// [`RpcServer::add_service`] and cast it to the interface `T`.
    ///
    /// Convenience for `Strong::try_from(self.get_service(name)?)`, mirroring
    /// [`hub::wait_for_interface`](crate::hub::wait_for_interface) on the kernel
    /// stack. Returns [`StatusCode::BadType`] if the resolved binder does not
    /// implement `T`, or any error surfaced by [`get_service`](Self::get_service).
    pub fn get_interface<T: crate::FromIBinder + ?Sized>(
        &self,
        name: &str,
    ) -> Result<crate::Strong<T>> {
        crate::Strong::<T>::try_from(self.get_service(name)?)
    }
}
