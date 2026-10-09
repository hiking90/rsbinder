// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcServer` — bind / listen / accept, one session per connection.
//!
//! Model: **one connection ⇒ one [`RpcSession`] ⇒ one worker thread**,
//! each with its own `RpcState` (no global, so sessions
//! are isolated and the suite is parallel-safe). Concurrent clients use
//! independent connections; nested re-entrant calls run inline on a
//! connection's worker (the `client_transact` recv loop dispatches
//! inbound `TRANSACT`s). The *semantics* (concurrency-correct,
//! isolated, oneway FIFO, negotiated, timed-out) match android-12 r34.
//!
//! Naming: android semantics, snake_case (`setup_unix_server`,
//! `get_root`, `add_service`, `set_max_threads`).
//!
//! # Accept and wrap
//!
//! The accept loop only calls `accept(2)`; every other step runs on the
//! connection's worker thread, so a failure there drops only that
//! connection. The worker switches the stream back to blocking (the
//! listener is non-blocking only so the loop can poll `shutdown`),
//! disables Nagle on TCP (small-frame traffic, as the client-side
//! `TlsTransport::connect` does), and wraps it — natively, or through a
//! server-side TLS handshake. Keeping the handshake off the accept loop is
//! what stops a slow-handshake peer from stalling it: the worker absorbs
//! the handshake time, and `set_max_connections` bounds how many
//! handshakes are in flight.
//!
//! The TLS handshake does blocking reads and writes on the raw socket
//! before the worker arms its admission deadline on the wrapped
//! transport, so the handshake deadline is armed on the raw stream first:
//! on the read side for a connected-but-silent peer, and on the write side
//! for a peer that is admitted but stops reading, stalling our handshake
//! `write_all` once its receive window fills. Without it such a peer pins
//! its worker — and, under `set_max_connections`, the whole accept loop —
//! indefinitely. Arming is best-effort: a failure means no deadline. On
//! the plain (UDS/vsock) path the wrap does no I/O and the same deadline
//! is re-armed before the native handshake reads.
//!
//! The worker snapshots the TLS config instead of the accept loop; that is
//! sound because only the `setup_*_server_tls` factories set it, before
//! the server is shared as an `Arc`.
//!
//! TCP is internal-only. There is no public plaintext `setup_tcp_server`
//! (plaintext network RPC is never production-appropriate, see the
//! [`super`] module doc): the TCP listener is reached only through
//! `setup_tcp_server_tls`, and a plain TCP wrap is refused.
//!
//! # Admission and deadlines
//!
//! - **Authorizer.** Runs on the connection's worker, concurrently across
//!   connections and never blocking the accept loop, but before the
//!   wire-profile branch, session build, handshake or any `recv_frame`, so
//!   a rejected peer receives zero RPC bytes. It is pure on
//!   `RpcTransport::peer_identity` (unix `SO_PEERCRED`/`getpeereid`, TLS
//!   certificate, vsock cid) and is the enforcement point for that
//!   identity. It is cloned out of its lock and called lock-free, so it may
//!   re-enter the server without self-deadlock (the same discipline as
//!   `RpcProxy::send_obituary`). Unset, every peer is admitted.
//! - **Handshake deadline.** Armed on read and write before the blocking
//!   serve loop. The android-13+ path lifts both sides after its handshake;
//!   the r34 path, which has no handshake, reads the client's session-id
//!   preamble under it, lifts the write side before the serve loop and the
//!   read side after the first frame. The 10 s default
//!   exists so a peer that never sends its handshake cannot hold a
//!   `max_connections` slot, or pin the server's `Arc`, forever.
//! - **Serve deadlines.** After the android-13+ handshake the handshake
//!   deadline is replaced by the idle timeout on read, and on write by the
//!   smaller of the idle and reply timeouts (`set_reply_timeout`) — both
//!   `None` by default, so an established session may idle unbounded. A
//!   callback slot (a client's incoming attach, which the server only
//!   sends on) has no serve loop and arms only the write side.
//! - **`max_connections`.** The rsbinder analogue of AOSP `RpcServer`'s
//!   bounded server resources, not a wire or semantic port: rsbinder is one
//!   connection = one session = one worker, so the bounded resource is the
//!   concurrent worker count. At capacity the accept loop stops accepting
//!   and excess clients wait in the kernel listen backlog. Making workers
//!   fewer than connections would need I/O multiplexing, which is out of
//!   scope.
//!
//! # Session registry
//!
//! `sessions` maps a server-minted `RpcSessionId` to a `Weak` of the
//! founding `RpcSessionInner` (AOSP `RpcServer::mSessions`). The
//! android-13+ accept handshake reads the client's
//! `RpcConnectionHeader.sessionId`:
//!
//! - **empty** (every single-connection client): a new session; its id is
//!   registered here and never looked up on this path.
//! - **non-empty, live**: the connection attaches to that session.
//!   `add_incoming_slot_capped` adds a slot onto the single founding inner,
//!   so the `RpcProxy`s cached in `state.remote_proxies` point at the only
//!   inner and a server worker's nested `proxy.transact` `find_conn` stays
//!   within that inner's slot pool (no cross-slot aliasing). An attach that
//!   built a fresh session instead would leave `attached_count` at 0 and the
//!   second connection unable to reach the founding connection's binder.
//! - **non-empty, unknown or stale**, or any id that is not 32 bytes (AOSP
//!   `kSessionIdBytes == 32`): the connection is rejected (AOSP `ALOGE` +
//!   return) and counted in `rejected_unknown_id_count`.
//!
//! The map holds `Weak`s so it never keeps a session alive; dead entries
//! are pruned on the next registration, since no single exit marks a
//! session's death. An entry does outlive session death while a proxy
//! still pins the dead inner (proxies hold `Arc<RpcSessionInner>`); an id
//! echoed onto such a session is refused by its lifecycle —
//! `try_bump_live_conns` inside `add_incoming_slot_capped` refuses
//! `Dying`/`Dead` — not by a dangling `Weak`. The key is the
//! `RpcSessionId` newtype to mark the 32 bytes as an attach capability;
//! public APIs keep `&[u8]` / `[u8; 32]`.
//!
//! # Termination
//!
//! `terminating` is raised only by `terminate`, and stored before it takes
//! `live_sessions`. `track_session` and that take share the
//! `live_sessions` mutex, so a session the take missed was listed after
//! the store, and its worker's `minted_after_terminate` check — run right
//! after listing — sees the flag and ends the session instead of serving.
//! `live_sessions` lists every minted session because the id-keyed
//! `sessions` map knows only android-13+ sessions; an attach adds a slot to
//! an inner already listed. `terminating` is separate from `shutdown`
//! because the graceful `stop_accepting` also raises `shutdown` and must
//! leave a just-accepted session serving. The two flags carry no cross
//! invariant: a worker can read one raised and the other not, and a check
//! on one never substitutes for a check on the other.

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

/// Server TLS config, `None` = plain; `Mutex<Option>` like the other late-bound knobs.
#[cfg(feature = "rpc-tls")]
type TlsServerConfigCell = Mutex<Option<Arc<rustls::ServerConfig>>>;

/// The accept loop's listener; `Tcp` is TLS-only (see module doc "Accept and wrap").
enum ServerListener {
    Unix(UnixListener),
    #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
    Vsock(vsock::VsockListener),
    #[cfg(feature = "rpc-tls")]
    Tcp(TcpListener),
}

/// Bind metadata; only a path `Unix` bind leaves a file for `Drop` to remove.
enum BindAddress {
    /// `file`: the bound socket's (dev, ino), so `Drop` never unlinks a successor's socket.
    Unix {
        path: PathBuf,
        file: Option<(u64, u64)>,
    },
    #[cfg(any(target_os = "linux", target_os = "android"))]
    UnixAbstract,
    #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
    Vsock { cid: u32, port: u32 },
    #[cfg(feature = "rpc-tls")]
    Tcp(SocketAddr),
}

/// An accepted stream awaiting its wrap on the worker; see module doc "Accept and wrap".
enum RawAccepted {
    Unix(UnixStream),
    #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
    Vsock(vsock::VsockStream),
    #[cfg(feature = "rpc-tls")]
    Tcp(TcpStream),
}

impl ServerListener {
    /// Non-blocking, so the accept loop can poll `shutdown`.
    fn set_nonblocking(&self, on: bool) -> std::io::Result<()> {
        match self {
            ServerListener::Unix(l) => l.set_nonblocking(on),
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            ServerListener::Vsock(l) => l.set_nonblocking(on),
            #[cfg(feature = "rpc-tls")]
            ServerListener::Tcp(l) => l.set_nonblocking(on),
        }
    }

    /// `accept(2)` only; the wrap and any TLS handshake run on the worker.
    fn accept_raw(&self) -> std::io::Result<RawAccepted> {
        // Only `accept(2)`: setup runs on the worker, so one peer's RST can't end the loop.
        match self {
            ServerListener::Unix(l) => {
                let (stream, _addr) = l.accept()?;
                Ok(RawAccepted::Unix(stream))
            }
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            ServerListener::Vsock(l) => {
                let (stream, _addr) = l.accept()?;
                Ok(RawAccepted::Vsock(stream))
            }
            #[cfg(feature = "rpc-tls")]
            ServerListener::Tcp(l) => {
                let (stream, _addr) = l.accept()?;
                Ok(RawAccepted::Tcp(stream))
            }
        }
    }
}

impl RawAccepted {
    /// Blocking mode, plus `TCP_NODELAY` on TCP, for the worker; see module doc "Accept and wrap".
    fn prepare_for_worker(&self) -> std::io::Result<()> {
        match self {
            RawAccepted::Unix(s) => s.set_nonblocking(false),
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            RawAccepted::Vsock(s) => s.set_nonblocking(false),
            #[cfg(feature = "rpc-tls")]
            RawAccepted::Tcp(s) => {
                s.set_nonblocking(false)?;
                s.set_nodelay(true)
            }
        }
    }

    /// Bound the pre-wrap TLS handshake reads; see module doc "Accept and wrap".
    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> std::io::Result<()> {
        match self {
            RawAccepted::Unix(s) => s.set_read_timeout(timeout),
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            RawAccepted::Vsock(s) => s.set_read_timeout(timeout),
            #[cfg(feature = "rpc-tls")]
            RawAccepted::Tcp(s) => s.set_read_timeout(timeout),
        }
    }

    /// Bound the pre-wrap handshake writes to a peer that stops reading; see module doc.
    fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> std::io::Result<()> {
        match self {
            RawAccepted::Unix(s) => s.set_write_timeout(timeout),
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            RawAccepted::Vsock(s) => s.set_write_timeout(timeout),
            #[cfg(feature = "rpc-tls")]
            RawAccepted::Tcp(s) => s.set_write_timeout(timeout),
        }
    }

    /// Wrap on the worker: TLS handshake if `tls_config` is set, else native (plain TCP refused).
    #[cfg(feature = "rpc-tls")]
    fn into_transport(
        self,
        tls_config: Option<Arc<rustls::ServerConfig>>,
    ) -> RpcResult<Box<dyn RpcTransport>> {
        if let Some(cfg) = tls_config {
            // No `MSG_NOSIGNAL` on Apple; std sets `SO_NOSIGPIPE` only on sockets it creates.
            #[cfg(target_vendor = "apple")]
            {
                use std::os::fd::AsFd;
                let fd = match &self {
                    RawAccepted::Unix(s) => s.as_fd(),
                    RawAccepted::Tcp(s) => s.as_fd(),
                };
                rustix::net::sockopt::set_socket_nosigpipe(fd, true)
                    .map_err(std::io::Error::from)?;
            }
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

/// Default `set_handshake_timeout`: a silent peer cannot hold its slot or the server `Arc` forever.
const DEFAULT_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The single root (android RPC has one) behind `add_service`; its map is `RpcServer::named`.
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

/// `true` admits the peer; `Arc` so it is cloned out of the lock and called lock-free.
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
    /// `Some` ⇒ every accepted connection is TLS-handshaken on its worker; set by `setup_*_tls`.
    #[cfg(feature = "rpc-tls")]
    tls_config: TlsServerConfigCell,
    root: Mutex<Option<SIBinder>>,
    /// Services behind `add_service`; shared with the `directory` root, so no rebuild per insert.
    named: Arc<Mutex<HashMap<String, SIBinder>>>,
    /// Directory root over `named`, built once; `add_service` installs it as the root.
    directory: SIBinder,
    max_threads: Mutex<u32>,
    /// Whether sessions advertise `Unix` FD support (default false ⇒ every FD refused).
    fd_unix_supported: AtomicBool,
    /// `None` ⇒ android-12 r34 wire; `Some(max)` ⇒ AOSP handshake negotiating `min(max, client)`.
    wire_max_version: Mutex<Option<u32>>,
    /// Cap on live connection workers (`None` = none); see module doc "Admission and deadlines".
    max_connections: Mutex<Option<usize>>,
    /// Deadline through the handshake / r34 first frame (10 s); see `set_handshake_timeout`.
    handshake_timeout: Mutex<Option<std::time::Duration>>,
    /// Serve-phase deadline after the android-13+ handshake (default none); see `set_idle_timeout`.
    idle_timeout: Mutex<Option<std::time::Duration>>,
    reply_timeout: Mutex<Option<std::time::Duration>>,
    /// Admission hook run before any RPC byte; see module doc "Admission and deadlines".
    authorizer: Mutex<Option<Authorizer>>,
    /// Test barrier on the attach arm before the `shutdown` gate (`__set_attach_shutdown_probe`).
    #[doc(hidden)]
    attach_shutdown_probe: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Id → founding inner (AOSP `RpcServer::mSessions`); see module doc "Session registry".
    sessions: Mutex<HashMap<RpcSessionId, std::sync::Weak<RpcSessionInner>>>,
    /// Mints, attaches and refusals for the `*_count` getters; atomics off the transaction path.
    session_registered: AtomicUsize,
    attached_count: AtomicUsize,
    rejected_unknown_id: AtomicUsize,
    shutdown: Arc<AtomicBool>,
    /// Raised only by `terminate`, before its `live_sessions` take; see module doc "Termination".
    terminating: Arc<AtomicBool>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    /// Every minted session, r34 included, for `terminate`; `Weak`, pruned on push.
    live_sessions: Mutex<Vec<std::sync::Weak<RpcSessionInner>>>,
}

/// Clear `path` for a bind: only a socket that refuses connections is removed.
fn remove_stale_socket(path: &Path) -> Result<()> {
    use std::io::ErrorKind;
    use std::os::unix::fs::FileTypeExt;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if !meta.file_type().is_socket() {
        log::error!(
            "RpcServer::setup_unix_server: {path:?} exists and is not a socket; not removing it"
        );
        return Err(StatusCode::AlreadyExists);
    }
    remove_if_stale(path)
}

/// XNU refuses a live listener with a full backlog as it does a stale socket: keep it, as AOSP.
#[cfg(target_vendor = "apple")]
fn remove_if_stale(path: &Path) -> Result<()> {
    log::error!(
        "RpcServer::setup_unix_server: a socket exists at {path:?}; Apple platforms never \
         remove it (delete it once no server listens on it)"
    );
    Err(StatusCode::from(rustix::io::Errno::ADDRINUSE))
}

#[cfg(not(target_vendor = "apple"))]
fn remove_if_stale(path: &Path) -> Result<()> {
    use rustix::io::Errno;
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    use std::io::ErrorKind;
    // Non-blocking: Linux parks a blocking connect to a live server with a full backlog.
    let flags = SocketFlags::CLOEXEC | SocketFlags::NONBLOCK;
    let probe = rustix::net::socket_with(AddressFamily::UNIX, SocketType::STREAM, flags, None)
        .map_err(std::io::Error::from)?;
    let addr = SocketAddrUnix::new(path).map_err(std::io::Error::from)?;
    match rustix::net::connect(&probe, &addr) {
        // `EAGAIN`: a listener whose backlog is full (Linux `unix_stream_connect`).
        Ok(()) | Err(Errno::AGAIN) | Err(Errno::INPROGRESS) => {
            log::error!("RpcServer::setup_unix_server: another server is listening on {path:?}");
            Err(StatusCode::from(Errno::ADDRINUSE))
        }
        Err(Errno::CONNREFUSED) => std::fs::remove_file(path).or_else(|e| match e.kind() {
            ErrorKind::NotFound => Ok(()),
            _ => Err(StatusCode::from(e)),
        }),
        Err(e) => {
            log::error!("RpcServer::setup_unix_server: cannot probe {path:?}: {e}");
            Err(std::io::Error::from(e).into())
        }
    }
}

/// The (device, inode) of the file at `path`, without following a symlink.
fn socket_file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()))
}

impl RpcServer {
    /// Bind + listen on a Unix-domain socket path.
    ///
    /// Something already at `path` is removed only when it is a **stale**
    /// socket — a socket file nothing listens on, which a connect attempt
    /// finds refused (`ECONNREFUSED`), as a crashed server leaves behind.
    /// Anything else is left alone and refused:
    ///
    /// - a socket another server is listening on →
    ///   `StatusCode::Errno(-EADDRINUSE)`, so a second instance cannot
    ///   silently take over a running server's path;
    /// - a file that is not a socket → [`StatusCode::AlreadyExists`].
    ///
    /// AOSP `RpcServer::setupUnixDomainServer` never removes anything and
    /// fails on every existing path; the stale-socket case is kept so a
    /// restart after a crash needs no manual cleanup. The probe, the
    /// removal and the bind are separate steps, so two servers started at
    /// once on the same stale path can both succeed, the later removal
    /// leaving the earlier server listening on a path no client reaches.
    /// Start one server per path.
    ///
    /// **Apple platforms** keep AOSP's behavior instead: any socket at
    /// `path` is refused with `StatusCode::Errno(-EADDRINUSE)` and never
    /// removed, so delete a crashed server's socket before restarting.
    /// XNU refuses a connect to a live listener whose backlog is full with
    /// the same `ECONNREFUSED` a stale socket gives, so the probe would
    /// remove a running server's path there.
    ///
    /// Dropping the server removes the socket file only while it is still
    /// the one this server bound (same device and inode), so a server that
    /// has since been replaced at the same path keeps its socket.
    pub fn setup_unix_server(path: impl Into<PathBuf>) -> Result<Arc<RpcServer>> {
        let path = path.into();
        remove_stale_socket(&path)?;
        // `StatusCode: From<std::io::Error>` — `?` converts directly.
        let listener = UnixListener::bind(&path)?;
        let file = socket_file_id(&path);
        let listener = ServerListener::Unix(listener);
        // Non-blocking accept so the loop can observe `shutdown`.
        listener.set_nonblocking(true)?;
        Ok(Self::wrap(listener, BindAddress::Unix { path, file }))
    }

    /// Bind + listen on a Linux/Android abstract Unix-domain socket.
    /// Abstract sockets have no filesystem entry, so there is no stale
    /// path to remove and no drop-time unlink.
    ///
    /// **Security**: unlike a path-bound socket, an abstract socket has
    /// no filesystem permissions — *any* process in the same network
    /// namespace can connect (subject only to LSM policy such as
    /// SELinux). A path-bound [`setup_unix_server`](Self::setup_unix_server)
    /// can rely on directory modes for access control; an abstract
    /// server cannot. When exposure matters, install a
    /// [`set_authorizer`](Self::set_authorizer) hook and check the
    /// peer identity (uid/gid/pid) it receives.
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

    /// Common constructor behind every `setup_*` factory, so the field set lives in one place.
    fn wrap(listener: ServerListener, bind: BindAddress) -> Arc<RpcServer> {
        // One directory root over the shared `named` map; `add_service` needs no rebuild.
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
            reply_timeout: Mutex::new(None),
            authorizer: Mutex::new(None),
            attach_shutdown_probe: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
            session_registered: AtomicUsize::new(0),
            attached_count: AtomicUsize::new(0),
            rejected_unknown_id: AtomicUsize::new(0),
            shutdown: Arc::new(AtomicBool::new(false)),
            terminating: Arc::new(AtomicBool::new(false)),
            workers: Mutex::new(Vec::new()),
            live_sessions: Mutex::new(Vec::new()),
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
    /// [`super::rustls`] (re-export of the linked
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
    /// [`super::rustls`] to construct the config (server
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

    /// TLS config snapshot (`None` = plain), taken once per connection for the worker's lifetime.
    #[cfg(feature = "rpc-tls")]
    fn tls_snapshot(&self) -> Option<Arc<rustls::ServerConfig>> {
        self.tls_config.lock().expect("tls_config poisoned").clone()
    }

    /// Publish the single root object (android `setRootObject`).
    ///
    /// The root is copied into each session once, when it is accepted; AOSP
    /// instead reads `getRootObject()` on every `GET_ROOT` (`RpcState.cpp`).
    /// A session accepted before this call keeps the root it was given, so
    /// call it before [`run`](Self::run) / [`run_background`](Self::run_background).
    /// A later [`add_service`](Self::add_service) replaces it with the service directory.
    ///
    /// Refuses a **remote** binder with [`StatusCode::InvalidOperation`] — see
    /// [`add_service`](Self::add_service) for why.
    pub fn set_root(&self, binder: SIBinder) -> Result<()> {
        super::refuse_remote(&binder, "RpcServer::set_root")?;
        *self.root.lock().expect("root poisoned") = Some(binder);
        Ok(())
    }

    /// Register a named service. Every call installs the built-in
    /// `ServiceDirectory` as the root, replacing a root set by
    /// [`set_root`](Self::set_root); the directory shares this server's
    /// service map, so the insert itself needs no rebuild. Clients reach it via
    /// [`RpcSession::get_service`]. The *first* call installs a root, which,
    /// as with [`set_root`](Self::set_root), a session accepted before it
    /// does not see: make that first call before `run` / `run_background`.
    ///
    /// Refuses a **remote** binder with [`StatusCode::InvalidOperation`]:
    /// publishing a proxy here can never work. A proxy of *this* session would
    /// be handed straight back to its own peer; a proxy of another session or
    /// of kernel binder is refused when the parcel is written
    /// (AOSP `RpcState::onBinderLeaving`). Wrap it in a local `Bn*` instead —
    /// `BnFoo::new_binder(proxy)` — which is the gateway pattern.
    pub fn add_service(&self, name: &str, binder: SIBinder) -> Result<()> {
        super::refuse_remote(&binder, "RpcServer::add_service")?;
        self.named
            .lock()
            .expect("named poisoned")
            .insert(name.to_string(), binder);
        // Reinstall the shared directory as root, so `add_service` wins over any prior `set_root`.
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
    ///    Callback connections — the ones a client opens with
    ///    `ARpcSession_setMaxIncomingThreads` / rsbinder's client-side
    ///    incoming connections, on which this server *sends* — are
    ///    budgeted separately at `2 * n` per session; served slots do
    ///    not count against that budget. A callback slot has no read loop
    ///    and lives until the whole session tears down, as every slot does
    ///    (only an attach that never completed is un-pushed), so without that
    ///    budget a peer holding the session id could grow the slot pool —
    ///    and its held fds — without bound. AOSP opens symmetric incoming
    ///    and outgoing connections, each bounded by the negotiated
    ///    max-threads, so `2 * n` never refuses a well-behaved client. The
    ///    budget is checked and the slot pushed under one `conn_state`
    ///    lock, so concurrent attach workers cannot overshoot it. Likewise
    ///    the `n` cap check, the anti-resurrection gate
    ///    (`try_bump_live_conns`) and the slot push form one critical
    ///    section, and only served slots count toward `n`.
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
    /// (unset) is unbounded — byte-for-byte a server that never calls
    /// this, so it is purely additive. When set, the accept loop stops accepting
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
    /// loop. Default `DEFAULT_HANDSHAKE_TIMEOUT` (10s). `Some(d)` ⇒ a
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
    /// client's `int32` session-id preamble and the first serve-loop frame,
    /// and is cleared once that frame is read. Either
    /// way an established two-way session may then sit idle between requests
    /// unbounded (the per-call reply deadline is managed separately via
    /// [`RpcSession::set_timeout`](super::RpcSession::set_timeout)).
    ///
    /// `Some(Duration::ZERO)` is not a valid deadline (`SO_RCVTIMEO`
    /// rejects it, and every arming site would silently fail, leaving the
    /// admission phase *unbounded* — worse than never calling this) and is
    /// refused: it is logged and the default is kept. Pass `None` to
    /// disable the deadline deliberately.
    pub fn set_handshake_timeout(&self, timeout: Option<std::time::Duration>) {
        // Zero keeps the 10s default: here `None` (unbounded) is as wrong as zero.
        let timeout = match timeout {
            Some(d) if d.is_zero() => {
                log::error!(
                    "RpcServer::set_handshake_timeout: a zero duration is not a \
                     valid deadline; keeping the default"
                );
                Some(DEFAULT_HANDSHAKE_TIMEOUT)
            }
            other => other,
        };
        *self
            .handshake_timeout
            .lock()
            .expect("handshake_timeout poisoned") = timeout;
    }

    /// Set (or disable) the **idle timeout** of the android-13+ serve
    /// path, applied *after* the handshake completes. Default `None` ⇒ an
    /// established session may idle between requests unbounded
    /// (byte-identical to a server that never calls this).
    ///
    /// [`set_handshake_timeout`](Self::set_handshake_timeout) only bounds
    /// the handshake/first-contact phase; once a peer completes the
    /// handshake it can then go silent and hold its worker — and, under
    /// [`set_max_connections`](Self::set_max_connections), an admission
    /// slot — indefinitely (a post-handshake Slowloris that starves the
    /// accept loop).
    ///
    /// `Some(d)` ends a session, freeing its slot, only once for **at
    /// least `d` and less than `2d`** no byte crossed any of its
    /// connections in either direction and no call was open: no handler
    /// running for the peer, and no call to the peer awaiting its reply.
    /// The judgment is per session, not per connection: a fan-out client
    /// that keeps one connection busy and another quiet is not idle. Each
    /// serve connection waits for the peer's next frame under `d` and
    /// checks the whole session when that wait expires, which is why an
    /// eviction may come up to one more `d` after the session went quiet.
    /// Bytes read count as each transport read completes; a frame being
    /// written counts from its first byte until the write returns, however
    /// slowly the peer takes it; a connection joining the session counts
    /// as it joins, its handshake included. On a stream ring, the records
    /// this server's end writes or takes out and its waits that park count
    /// as they happen. (Honored on the android-13+ serve path only; the
    /// r34 profile bounds its first frame via the handshake deadline.)
    ///
    /// Two waits are bounded by a deadline directly rather than judged:
    ///
    /// - A gap longer than `d` **inside** a frame a serve connection reads
    ///   between calls ends the session as a lost stream
    ///   ([`EndReason::DeadlineMidFrame`](super::EndReason::DeadlineMidFrame)),
    ///   the same rule as on a one-connection session.
    /// - A twoway call on a session, made by a thread that is driving one
    ///   of that session's serve connections — a handler, twoway or
    ///   oneway, or anything else the serve loop runs on that thread, such
    ///   as a local object's `Drop` that runs when the client's release
    ///   drops the last reference to it — waits for its reply under
    ///   [`set_reply_timeout`](Self::set_reply_timeout), or under `d` when
    ///   that is `None`, and a frame read in that wait,
    ///   nested calls' included, is bounded by the same deadline. Its
    ///   expiry, between frames or inside one, is a reply timeout, which
    ///   ends the session as a fault. A call from any other thread, work a
    ///   handler hands to one included, waits under `set_reply_timeout`
    ///   alone. A call on another session follows that session's own
    ///   settings: a handler that calls back a *different* client (a
    ///   fan-out to stored callbacks) drives none of that client's serve
    ///   connections, so its wait there has `set_reply_timeout` alone.
    ///
    /// A peer that holds a call open, or trickles bytes, is not idle by
    /// this measure: `set_reply_timeout` bounds how long it may keep a
    /// callback of this server's waiting, and nothing here bounds a
    /// trickle. A client that only waits for callbacks is idle: it moves
    /// no byte and holds no call. So is a stream that waits outside a
    /// handler without pinging its peer, unless this server's end runs on
    /// a ring: there each of its waits that parks counts (a wait longer than
    /// the ring's short spin always parks), so such waits shorter than `d`,
    /// repeated, keep the session with no item moving (a client end's waits
    /// do not). A stream wait
    /// inside a handler holds that handler's call open for as long as it
    /// lasts, so the session is not idle, and only the wait's own deadline
    /// bounds it.
    ///
    /// It is for a server that admits unauthenticated TCP or TLS peers and
    /// caps them with `set_max_connections`: a peer that finishes the
    /// handshake and then stays silent is what nothing else catches —
    /// kernel keepalive does not, since the peer's kernel answers it, and
    /// [`set_reply_timeout`](Self::set_reply_timeout) measures only waits
    /// for an answer, which a client that sends nothing never causes. A
    /// server that picks its peers with
    /// [`set_authorizer`](Self::set_authorizer) or TLS client
    /// authentication needs it less. It fits a protocol with regular
    /// traffic.
    ///
    /// The serve phase arms this value on the **write** side too, so a peer
    /// that stops draining replies ends the session as well. The write half
    /// bounds each wait for socket buffer space, not a whole reply: a
    /// consumer that reads a large reply slowly but steadily is not cut,
    /// one that reads nothing for `d` is. With
    /// [`set_reply_timeout`](Self::set_reply_timeout) also set, the smaller
    /// of the two bounds the sends. The write half is the transport's
    /// [`RpcTransport::set_write_timeout`]:
    /// a transport that keeps that method's no-op default gives a stalled
    /// send no bound, and since a frame being written is activity, a
    /// session stuck in such a send never idles out.
    ///
    /// On callback connections (a client's incoming attaches, which this
    /// server only ever *sends* on) the **read** half is exempt: they
    /// have no serve loop, and a read deadline there would cut short the
    /// server's own reply wait on a callback made by a thread driving none
    /// of that session's serve connections.
    /// The **write** half still applies, and bounds a callback *send* to a
    /// peer that has stopped reading. The reply wait of a callback made by
    /// a thread driving none of that session's serve connections is
    /// bounded by
    /// [`set_reply_timeout`](Self::set_reply_timeout) alone; while it
    /// lasts the call is open, so the session is not idle.
    ///
    /// Read once per accepted android-13+ connection, after its handshake;
    /// that one value arms the connection's socket. A serve connection — a
    /// session's founding one, or an attach the session admits — also
    /// stores it into its session; a callback connection or a refused
    /// attach stores nothing. The value last stored is the one a session
    /// uses as the default reply deadline of calls from its serve threads
    /// (above) and restores its serve connections to after a reply
    /// deadline — one value per session, not per connection. A change
    /// made while the server runs applies to connections accepted after
    /// it. Call this *before* [`run`](Self::run) /
    /// [`run_background`](Self::run_background): changing it while a
    /// multi-connection session is live leaves that session restoring the
    /// newest value on connections whose socket carries an older one.
    ///
    /// `Some(Duration::ZERO)` is not a valid deadline (`SO_RCVTIMEO`
    /// rejects it) and is refused — logged and treated as `None`.
    pub fn set_idle_timeout(&self, timeout: Option<std::time::Duration>) {
        *self.idle_timeout.lock().expect("idle_timeout poisoned") =
            super::session::reject_zero_deadline(
                timeout,
                "RpcServer::set_idle_timeout: a zero duration is not a valid deadline; ignoring",
            );
    }

    /// Bound how long this server waits for a **reply to a callback** it
    /// issued to a client (`RpcSession::set_timeout` on every session this
    /// server builds). `None` (default) blocks forever on a callback
    /// issued *outside* a handler of that client's session (below says
    /// exactly which) — the case this setter exists for — which is
    /// byte-identical to not calling this at all.
    ///
    /// This is the only bound on **that** wait. The idle deadline cannot
    /// serve there: callback connections are exempt from its *read* half
    /// (see [`set_idle_timeout`](Self::set_idle_timeout)), because
    /// `SO_RCVTIMEO` is sticky and would cut short exactly this wait, and
    /// the wait counts as a call in progress, so the session is not idle
    /// while it lasts.
    ///
    /// A callback made by a thread that is driving one of the same
    /// session's serve connections — a handler, twoway or oneway, or
    /// anything else that session's serve loop runs on that thread — is
    /// not on that path. A twoway handler's callback reuses the serve
    /// connection it is answering on (the re-entrant nested-call pin); a
    /// oneway handler's leaves on a callback connection. Either way, with
    /// no value here, on the android-13+ serve path, its reply wait is
    /// bounded by [`set_idle_timeout`](Self::set_idle_timeout) — a client
    /// handler slower than that makes the callback fail with
    /// `StatusCode::TimedOut`, which ends the session. With neither value,
    /// and on the r34 profile (which arms no idle deadline) with no value
    /// here, nothing bounds it. A value set here is that wait's deadline
    /// instead, and the connection's own read deadline (the idle deadline
    /// on a serve connection, none on a callback one) is restored after.
    /// Every other callback has this value as its only bound: one from a
    /// thread driving none of that session's serve connections, work a
    /// handler hands to another thread and a handler's callback to a
    /// *different* client included. Without a value here, a client that
    /// attaches an incoming connection, accepts such a callback and then
    /// never replies pins the sending worker forever — and every later
    /// caller behind it, since they queue on the same session's connection
    /// pool.
    ///
    /// Set it on any server that issues callbacks to clients it does not
    /// control. Size it against the slowest legitimate handler, not the
    /// round-trip: it bounds the peer's *think time*, and its expiry ends
    /// the whole session with that client, as
    /// [`RpcSession::set_timeout`](super::RpcSession::set_timeout)
    /// describes. It also bounds the
    /// wait for a free connection slot on the same session
    /// ([`RpcSession::set_timeout`](super::RpcSession::set_timeout)), so a
    /// callback behind a busy pool can take up to twice this value
    /// end-to-end.
    ///
    /// Being the session's `set_timeout`, it also bounds each wait for
    /// socket buffer space on every connection of the session and, on TCP,
    /// sizes the kernel's check that the client's host still answers
    /// ([`RpcTransport::set_liveness`] has the values and platform limits).
    ///
    /// Read **once per session**, when the connection that founds it is
    /// accepted: call this *before* [`run`](Self::run) /
    /// [`run_background`](Self::run_background). Sessions already
    /// established keep the value that was in force when they were built.
    ///
    /// `Some(Duration::ZERO)` is not a valid deadline (`SO_RCVTIMEO`
    /// rejects it) and is refused — logged and treated as `None`.
    pub fn set_reply_timeout(&self, timeout: Option<std::time::Duration>) {
        *self.reply_timeout.lock().expect("reply_timeout poisoned") =
            super::session::reject_zero_deadline(
                timeout,
                "RpcServer::set_reply_timeout: a zero duration is not a valid deadline; ignoring",
            );
    }

    /// Reap finished worker handles and return the live count, for the accept-loop admission gate.
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
    /// `|p| p.uid() == Some(EXPECTED_UID)` or, over TLS,
    /// `matches!(p, PeerIdentity::Certificate(c) if c.fingerprint() == &EXPECTED_SHA256)`.
    /// Backend-independent (unix/mem/tls/vsock). The hook must not
    /// block indefinitely: it runs on the connection's own worker
    /// thread — concurrently across connections, not serialized by the
    /// accept loop — and holds that connection's
    /// [`set_max_connections`](Self::set_max_connections) admission slot
    /// for its whole duration.
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
    /// attach path without the probe. Same `__`-prefix unstable-API discipline
    /// as `__fuzz_decode_rpc_parcel`; not part of the supported API
    /// surface.
    #[cfg(feature = "test-util")]
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

    /// Run the attach-shutdown probe, if set, outside its lock so it may re-enter the server.
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
    ///
    /// On the android-13+ wire ([`set_android13plus`](Self::set_android13plus))
    /// a client that requests a mode this server does not support is
    /// refused when it founds its session: the connection is closed after
    /// the handshake response, so the client's first call fails (AOSP
    /// `RpcServer.cpp` "Rejecting connection: FileDescriptorTransportMode
    /// is not supported"). Passing `Unix` keeps accepting clients that
    /// request no fd mode, where AOSP would refuse them. The r34 wire
    /// negotiates the mode by `GET_FD_MODE` instead and agrees `None`
    /// rather than refusing.
    pub fn set_supported_fd_modes(&self, modes: &[crate::rpc::FileDescriptorTransportMode]) {
        let unix = modes.contains(&crate::rpc::FileDescriptorTransportMode::Unix);
        self.fd_unix_supported.store(unix, Ordering::SeqCst);
    }

    /// Opt in to the **android-13+ versioned RPC wire**.
    /// `max_version` is the highest `RPC_WIRE_PROTOCOL_VERSION`
    /// this server offers (`0` = android-13, `1` = android-14/15,
    /// **`2` = android-16**); each accepted connection
    /// then runs the AOSP connection handshake and negotiates
    /// `min(max_version, client_max)`. Default (unset) speaks the
    /// AOSP android-12 r34 wire. Has effect only on a
    /// transport with raw byte access (every built-in backend).
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
    ///
    /// A `max_version` this build does not implement is logged at `error`
    /// and clamped to the highest supported version, so a client offering
    /// a version above it (e.g. `EXPERIMENTAL`) still negotiates instead of
    /// failing before its session response.
    pub fn set_android13plus(&self, max_version: u32) {
        let max_version = if super::wire_android13::is_supported_protocol_version(max_version) {
            max_version
        } else {
            let max = super::wire_android13::SUPPORTED_MAX_VERSION;
            log::error!("set_android13plus({max_version}): unsupported version, using {max}");
            max
        };
        *self
            .wire_max_version
            .lock()
            .expect("wire_max_version poisoned") = Some(max_version);
    }

    /// Apply root, max threads, reply timeout and FD policy to a new r34 or android-13+ session.
    fn configure_session(&self, session: &RpcSession) {
        // Clone first: an `if let` scrutinee would hold `root` while taking the session's lock.
        let root = self.root.lock().expect("root poisoned").clone();
        if let Some(root) = root {
            // Unreachable (remote roots are refused at `set_root`); logged, not panicked.
            if let Err(e) = session.set_root(root) {
                log::error!("RPC: server root rejected by the new session: {e:?}");
            }
        }
        let max_threads = *self.max_threads.lock().expect("max_threads poisoned");
        session.set_max_threads(max_threads);
        let reply_timeout = *self.reply_timeout.lock().expect("reply_timeout poisoned");
        session.set_timeout(reply_timeout);
        if self.fd_unix_supported.load(Ordering::SeqCst) {
            session.set_supported_fd_modes(&[crate::rpc::FileDescriptorTransportMode::Unix]);
        }
    }

    /// Build, configure and track a new r34 session with its own fresh `RpcState`.
    fn make_session(&self, transport: Box<dyn RpcTransport>) -> super::RpcResult<RpcSession> {
        // The server accepted this connection ⇒ Acceptor subspace; the worker read the preamble.
        let session = RpcSession::new_accepted(transport, super::address::AddressSpace::Acceptor)?;
        self.configure_session(&session);
        self.track_session(&session.inner_arc());
        Ok(session)
    }

    /// List a minted session for `terminate`; the caller checks `minted_after_terminate` next.
    fn track_session(&self, inner: &Arc<RpcSessionInner>) {
        let mut live = self.live_sessions.lock().expect("live_sessions poisoned");
        live.retain(|w| w.strong_count() > 0);
        live.push(Arc::downgrade(inner));
    }

    /// Ends `session` once `terminating` is up; worker half of module doc "Termination".
    fn minted_after_terminate(&self, session: &RpcSession) -> bool {
        if self.terminating.load(Ordering::SeqCst) {
            log::debug!("RPC: connection accepted as the server was terminating; ending it");
            session.close_session();
            return true;
        }
        false
    }

    // --- session-id → shared-session registry

    /// Register a new session's founding inner as a `Weak`; see module doc "Session registry".
    fn register_session(&self, id: RpcSessionId, inner: &Arc<RpcSessionInner>) {
        let mut map = self.sessions.lock().expect("sessions poisoned");
        // Prune here to bound the map by live sessions; no single exit marks a session's death.
        map.retain(|_, w| w.strong_count() > 0);
        map.insert(id, Arc::downgrade(inner));
        drop(map);
        self.session_registered.fetch_add(1, Ordering::SeqCst);
    }

    /// Echoed id → live founding inner; `None` for a non-32-byte, unknown or stale id.
    fn resolve_session(&self, id: &[u8]) -> Option<Arc<RpcSessionInner>> {
        let key = RpcSessionId::try_from_slice(id)?;
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .get(&key)
            .and_then(std::sync::Weak::upgrade)
    }

    /// Observability counter: new-session ids registered. Every android-13+
    /// session counts here, including the default (empty-id) flow.
    pub fn session_registered_count(&self) -> usize {
        self.session_registered.load(Ordering::SeqCst)
    }
    /// Observability counter: **id-demux attaches** (a 2nd+ connection bound
    /// to a pre-existing shared session). Stays zero on the empty-id flow.
    pub fn attached_count(&self) -> usize {
        self.attached_count.load(Ordering::SeqCst)
    }
    /// Observability counter: **id-carrying connections refused for any
    /// reason** — an unknown/stale id, a codec-version mismatch with the
    /// founding session, an attach arriving after `shutdown`, or an attach
    /// past the incoming/callback slot cap. It therefore counts more than
    /// stale ids: a well-behaved client whose `incoming_connections` exceeds
    /// this server's callback budget also raises it. Stays zero on the
    /// empty-id flow.
    ///
    /// On the r34 wire it counts connections whose `int32` session-id
    /// preamble is not `-1` (a new session). The first word of an
    /// android-13+ client's `RpcConnectionHeader` (its version) and of a
    /// length-prefixed frame from an rsbinder before 0.12.0 both read as
    /// such an id.
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
    /// delegate). Avoids the nested-lock pattern (`sessions` → `state`), so
    /// a poisoned `state` lock in one session does not poison `sessions`
    /// as a side-effect.
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
    /// for the server-side serve loop's exit, which ends the whole session
    /// (the typed `SessionLifecycle` goes `Live(n) → Dying` whatever
    /// `n`), without a `sleep(N ms)`
    /// heuristic that races scheduler jitter.
    pub fn session_live_conns(&self, id: &[u8; 32]) -> Option<usize> {
        // The public API keeps raw bytes; the id newtype is internal-only.
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
    ///
    /// Lock ladder: upgrade the `Weak` **first** (releasing the `sessions`
    /// mutex), then take the session's `conn_state` mutex, as
    /// [`live_session_node_count`](Self::live_session_node_count) does — a
    /// poisoned `conn_state` in one session must not poison `sessions` as a
    /// side-effect and take every attach down with it.
    pub fn session_slot_count(&self, id: &[u8; 32]) -> Option<usize> {
        let key = RpcSessionId::new(*id);
        let inner = self
            .sessions
            .lock()
            .expect("sessions poisoned")
            .get(&key)
            .and_then(std::sync::Weak::upgrade);
        inner.map(|s| s.slot_count())
    }

    /// Serve one already-connected transport on its own worker thread
    /// (for transports the accept loop does not produce, e.g. in-memory tests).
    /// The accept loop uses the private `serve_connection_raw`
    /// to keep TLS handshake (if any) on the worker side.
    pub fn serve_connection(self: &Arc<Self>, transport: Box<dyn RpcTransport>) {
        let server = Arc::clone(self);
        let handle = match std::thread::Builder::new()
            .name("rpc-conn".into())
            .spawn(move || {
                Self::run_connection_in_worker(server, transport);
            }) {
            Ok(h) => h,
            Err(e) => {
                log::warn!("RPC: failed to spawn connection worker, dropping: {e}");
                return;
            }
        };
        let mut workers = self.workers.lock().expect("workers poisoned");
        // Reap finished handles so `workers` tracks concurrent, not cumulative, connections.
        workers.retain(|h| !h.is_finished());
        workers.push(handle);
    }

    /// Spawn a worker that wraps `raw` and serves it; see module doc "Accept and wrap".
    fn serve_connection_raw(self: &Arc<Self>, raw: RawAccepted) {
        let server = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("rpc-conn".into())
            .spawn(move || {
                // Set up here: a failure drops only this connection, not the accept loop.
                if let Err(e) = raw.prepare_for_worker() {
                    log::warn!("RPC: failed to prepare accepted stream, dropping: {e:?}");
                    return;
                }
                // The TLS handshake in `wrap_accepted` precedes the worker's own deadline.
                let handshake_timeout = *server
                    .handshake_timeout
                    .lock()
                    .expect("handshake_timeout poisoned");
                if let Some(d) = handshake_timeout {
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
        // Spawn can fail (EAGAIN): drop the connection rather than panic in the accept loop.
        let handle = match spawned {
            Ok(h) => h,
            Err(e) => {
                log::warn!("RPC: failed to spawn connection worker, dropping: {e}");
                return;
            }
        };
        let mut workers = self.workers.lock().expect("workers poisoned");
        workers.retain(|h| !h.is_finished());
        workers.push(handle);
    }

    /// Wrap on the worker with the TLS config snapshot; see module doc "Accept and wrap".
    #[cfg(feature = "rpc-tls")]
    fn wrap_accepted(&self, raw: RawAccepted) -> RpcResult<Box<dyn RpcTransport>> {
        raw.into_transport(self.tls_snapshot())
    }
    #[cfg(not(feature = "rpc-tls"))]
    fn wrap_accepted(&self, raw: RawAccepted) -> RpcResult<Box<dyn RpcTransport>> {
        raw.into_transport()
    }

    /// Swap the handshake deadline for the idle timeout `idle` (read once per connection).
    fn arm_serve_timeouts(transport: &dyn RpcTransport, idle: Option<std::time::Duration>) {
        if let Err(e) = transport.set_read_timeout(idle) {
            log::debug!("RPC: failed to set serve-phase read timeout: {e:?}");
        }
        // Mirror onto writes: a peer that idles and stops reading can't pin us on a reply.
        Self::arm_write_timeout(transport, idle);
    }

    /// Write half of `arm_serve_timeouts`, for callback slots that must leave reads unbounded.
    fn arm_write_timeout(transport: &dyn RpcTransport, idle: Option<std::time::Duration>) {
        if let Err(e) = transport.set_write_timeout(idle) {
            log::debug!("RPC: failed to set serve-phase write timeout: {e:?}");
        }
    }

    /// Worker body after the wrap: authorize, then serve the r34 or android-13+ path inline.
    fn run_connection_in_worker(server: Arc<Self>, transport: Box<dyn RpcTransport>) {
        // Authorization gate (`authorizer` field doc); a TLS peer identity is already final.
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
        // Bound the handshake phase; `arm_serve_timeouts` swaps in the serve deadline later.
        let handshake_timeout = *server
            .handshake_timeout
            .lock()
            .expect("handshake_timeout poisoned");
        if let Some(d) = handshake_timeout {
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
                // A new session requesting `Unix` without `set_supported_fd_modes` is refused.
                let fd_unix = server.fd_unix_supported.load(Ordering::SeqCst);
                // Handshake apart from build: branch on the client's session id and direction.
                let (transport, codec, client_fd_mode, client_id, incoming) =
                    match RpcSession::android13plus_accept_handshake(transport, max) {
                        Ok(parts) => parts,
                        Err(e) => {
                            // Interop failure: `warn!`; `{e}` names a profile mismatch.
                            log::warn!("android-13+ RPC handshake failed: {e}");
                            return;
                        }
                    };
                // Read once: this socket's deadlines and the session baseline must be one value.
                let idle = *server.idle_timeout.lock().expect("idle_timeout poisoned");
                if incoming {
                    // Callback slot: no serve loop, so only the write deadline is armed.
                    if let Err(e) = transport.set_read_timeout(None) {
                        log::debug!("RPC: failed to clear callback-slot read timeout: {e:?}");
                    }
                    Self::arm_write_timeout(transport.as_ref(), idle);
                    // No read loop: clients call only on outgoing conns (AOSP `mOutgoing`).
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
                            // Test barrier (`__set_attach_shutdown_probe`); no-op unless set.
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
                            // Cap `2 * max_threads`, checked atomically (`set_max_threads`).
                            let incoming_cap =
                                (inner.max_threads_value() as usize).saturating_mul(2);
                            let session = RpcSession::wrap_inner(inner);
                            if let Err(e) =
                                session.add_callback_slot_and_init(transport, incoming_cap, &codec)
                            {
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
                // Handshake done: swap the admission deadline for the serve-phase one.
                Self::arm_serve_timeouts(transport.as_ref(), idle);
                if client_id.is_empty() {
                    // New session: mint, register (a `Weak`, pruned on a later register), serve.
                    let session = match RpcSession::from_android13plus(
                        transport,
                        codec,
                        client_fd_mode,
                        fd_unix,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            log::warn!("android-13+ RPC: from_android13plus failed: {e:?}");
                            return;
                        }
                    };
                    let id = RpcSessionId::new(session.session_id());
                    server.register_session(id, &session.inner_arc());
                    server.track_session(&session.inner_arc());
                    if server.minted_after_terminate(&session) {
                        return;
                    }
                    server.configure_session(&session);
                    // Callback reply deadlines restore to this, else idle eviction would end.
                    session.set_serve_read_deadline(idle);
                    session.serve_blocking().log("RPC session ended");
                } else if let Some(inner) = server.resolve_session(&client_id) {
                    // Attach onto the founding inner; its codec version is fixed for the session.
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
                    // Test barrier; the race window runs from here to the `load` below.
                    server.run_attach_shutdown_probe();
                    // Refuse attaches once shutting down; the worker pool is winding down.
                    if server.shutdown.load(Ordering::SeqCst) {
                        server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                        log::warn!("android-13+ RPC: attach after server shutdown; rejecting");
                        drop(transport);
                        return;
                    }
                    // Cap, live-conn bump and push are one critical section (`set_max_threads`).
                    let cap = inner.max_threads_value() as usize;
                    let session = RpcSession::wrap_inner(inner);
                    let slot_id = match session.add_incoming_slot_capped(transport, cap) {
                        Ok(id) => id,
                        Err(StatusCode::FailedTransaction) => {
                            server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                            log::warn!(
                                "android-13+ RPC: attach refused (incoming slot \
                                 cap reached: max_threads={cap})"
                            );
                            return;
                        }
                        Err(StatusCode::BadType) => {
                            server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                            log::warn!(
                                "android-13+ RPC: attach refused (transport differs \
                                 from the session's founding connection)"
                            );
                            return;
                        }
                        Err(e) => {
                            server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                            log::warn!(
                                "android-13+ RPC: session torn down between \
                                 resolve and attach; rejecting: {e:?}"
                            );
                            return;
                        }
                    };
                    // Only an admitted attach sets the baseline; this re-arms the new slot too.
                    session.set_serve_read_deadline(idle);
                    // Bump only once the slot reached the pool.
                    server.attached_count.fetch_add(1, Ordering::SeqCst);
                    session
                        .serve_blocking_on(slot_id)
                        .log("RPC attached connection ended");
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
                // AOSP `RpcServer::establishConnection`: the client's session id comes first.
                let id = super::wire_android13::read_r34_session_preamble(
                    &mut super::wire_android13::RawTransportIo(&*transport),
                );
                match id {
                    Ok(super::address::RPC_SESSION_ID_NEW) => {}
                    Ok(id) => {
                        server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                        log::warn!(
                            "RPC r34: client asked to join unknown session id {id}; \
                             rejecting connection"
                        );
                        return;
                    }
                    Err(e) => {
                        log::debug!("RPC r34: no session-id preamble: {e:?}");
                        return;
                    }
                }
                // The serve loop lifts only the read deadline; r34 writes nothing before frame 1.
                if let Err(e) = transport.set_write_timeout(None) {
                    log::debug!("RPC r34: failed to lift handshake write deadline: {e:?}");
                }
                let session = match server.make_session(transport) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("RPC r34: make_session failed: {e:?}");
                        return;
                    }
                };
                if server.minted_after_terminate(&session) {
                    return;
                }
                session
                    // No deadline armed ⇒ no first-frame `TimedOut` is an eviction of ours.
                    .serve_blocking_clearing_admission_deadline(handshake_timeout.is_some())
                    .log("RPC session ended");
            }
        }
    }

    /// Run the accept loop until [`RpcServer::stop_accepting`]. Each accepted
    /// connection gets its own session + worker thread.
    ///
    /// With [`set_max_connections`](Self::set_max_connections) at capacity the
    /// loop does not accept: pending clients wait in the kernel listen
    /// backlog and `shutdown` is still re-checked every tick. The cap is
    /// copied out before the worker count and the sleep, so a
    /// `set_max_connections` caller never waits on the poll interval.
    ///
    /// Accept errors: a reset between SYN and `accept` (`ECONNABORTED`,
    /// `ECONNRESET`), `EINTR`, and the pending network errors `accept(2)`
    /// documents as retry-like (`EPROTO`, `ENETDOWN`, `ENETUNREACH`,
    /// `EHOSTUNREACH`, `ETIMEDOUT`) are logged and skipped. Resource
    /// exhaustion (`EMFILE`/`ENFILE`/`ENOMEM`/`ENOBUFS`; a peer that churns
    /// connections can drive the process to `RLIMIT_NOFILE`) maps to
    /// `ErrorKind::Uncategorized`/`OutOfMemory`, so it has its own arm: it
    /// heals as in-flight sessions close their fds, and the loop backs off
    /// longer than for `EINTR` and keeps serving rather than turn an
    /// overload into a permanent outage. Any other error (e.g. the listener
    /// was closed) ends the loop with `Err`, logged at `error`.
    pub fn run(self: &Arc<Self>) -> Result<()> {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            // Cap copied out (guard dropped before sleep); at capacity clients wait in the backlog.
            let max_connections = *self
                .max_connections
                .lock()
                .expect("max_connections poisoned");
            if let Some(max) = max_connections {
                if self.live_worker_count() >= max {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
            }
            match self.listener.accept_raw() {
                Ok(raw) => {
                    // The worker wraps (TLS handshake included), never this loop.
                    self.serve_connection_raw(raw);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Non-blocking only to poll `shutdown`; nothing pending.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::Interrupted
                    ) || matches!(
                        e.raw_os_error(),
                        Some(code)
                            if code == libc::EPROTO
                                || code == libc::ENETDOWN
                                || code == libc::ENETUNREACH
                                || code == libc::EHOSTUNREACH
                                || code == libc::ETIMEDOUT
                    ) =>
                {
                    // Transient per `accept(2)`: continue; these must not end the server.
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
                    // Exhaustion heals as sessions close: back off, don't end the server.
                    log::warn!("accept resource exhaustion, backing off: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    // Fatal: surface it, or `run_background` dies silently.
                    log::error!("accept loop ending (fatal): {e}");
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
            // Log a fatal accept-loop error; otherwise the server dies with no trace.
            if let Err(e) = me.run() {
                log::error!("RPC accept loop terminated with error: {e:?}");
            }
        })
    }

    /// Stop accepting and let in-flight sessions drain as their peers
    /// disconnect — the graceful form. Only the accept flag is raised;
    /// nothing is joined. This reaches nothing a peer keeps open: a
    /// worker parked in `recv` never reads the flag, so a client that
    /// stays connected keeps its worker alive. To end those too, use
    /// [`terminate`](Self::terminate).
    ///
    /// **The android-13+ attach gate closes with it.** From this point a
    /// connection echoing a live session's id is refused — both the
    /// further connections a client transacts on and the ones it opens
    /// for callbacks — so a multi-connection session already established
    /// cannot grow, and a client still fanning its connections out when
    /// the flag goes up fails to build one. Each such refusal is counted
    /// by [`rejected_unknown_id_count`](Self::rejected_unknown_id_count).
    /// The connections a session already holds keep serving.
    pub fn stop_accepting(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// End the server now: stop accepting, end every session it serves,
    /// and join the workers. This is what a dropped
    /// [`ServerGuard`](crate::ServerGuard) does. Where
    /// [`stop_accepting`](Self::stop_accepting) waits for peers to leave,
    /// this ends each session as [`RpcSession::close_session`] would — whatever its
    /// connection count — so every slot's transport is shut down, the
    /// workers serving those slots wake out of `recv`, exit, and are
    /// joined. Peers see the connection end.
    ///
    /// **A worker that has no session yet is not reachable that way, and
    /// this call waits for it.** Ending a session shuts the transports of
    /// *its* slots down, and a connection still in the handshake has
    /// neither: its transport is a local of the worker thread. What bounds such a
    /// worker is [`set_handshake_timeout`](Self::set_handshake_timeout),
    /// so this call is bounded by that deadline; with it set to `None`
    /// nothing bounds it, and a peer that connects and then says nothing
    /// holds this call — and the [`ServerGuard`](crate::ServerGuard) drop
    /// or `stop_and_join` behind it — for as long as it stays silent.
    ///
    /// Repeats until a pass finds no session and no worker, so a
    /// connection accepted just as the flag went up is ended too (its
    /// worker ends the session itself on seeing the flag). Callable from
    /// a handler — a service stopping its own server: the worker it runs
    /// on is skipped rather than self-joined and exits when the handler
    /// returns. Idempotent.
    ///
    /// **The join is guaranteed only against a stopped accept loop.** Its
    /// thread is the caller's to join
    /// ([`run_background`](Self::run_background) returned it), and joining
    /// it *before* this call is the precondition, as `ServerGuard` does:
    /// a still-running accept loop spawns a worker before it registers the
    /// handle, so a connection accepted mid-pass can leave a worker behind
    /// that no pass ever sees. Such a worker is not joined, so the
    /// `Arc<RpcServer>` it holds (and this server's `Drop`, which unlinks
    /// the bound socket path) outlives the call; it ends itself once it
    /// reaches the point of minting a session and finds the flag, and
    /// until then it is bounded only by
    /// [`set_handshake_timeout`](Self::set_handshake_timeout).
    pub fn terminate(&self) {
        // `terminating` before the `live_sessions` take (field doc); no order vs `shutdown`.
        self.terminating.store(true, Ordering::SeqCst);
        self.shutdown.store(true, Ordering::SeqCst);
        loop {
            let live: Vec<Arc<RpcSessionInner>> =
                std::mem::take(&mut *self.live_sessions.lock().expect("live_sessions poisoned"))
                    .iter()
                    .filter_map(std::sync::Weak::upgrade)
                    .collect();
            let handles: Vec<JoinHandle<()>> =
                std::mem::take(&mut *self.workers.lock().expect("workers poisoned"));
            if live.is_empty() && handles.is_empty() {
                return;
            }
            for session in &live {
                session.close();
            }
            drop(live);
            Self::join_handles(handles);
        }
    }

    /// Join all session workers (call after the clients disconnect, or
    /// use [`terminate`](Self::terminate), which ends the sessions first
    /// and then joins).
    ///
    /// `Drop` only flips the shutdown flag and removes the socket — it
    /// deliberately does **not** join in-flight session workers (they
    /// drain on peer close).
    ///
    /// Panic observability is **best-effort**. Every accept and every
    /// [`serve_connection`](RpcServer::serve_connection) reaps
    /// already-finished handles out of `workers` — that is what bounds
    /// the vector by *concurrent* rather than cumulative connections —
    /// and reaping drops the `JoinHandle`, which detaches the thread;
    /// `JoinHandle::is_finished` cannot tell a panicked worker from a
    /// clean one. This call therefore warns for exactly the workers
    /// still registered when it runs: a worker that panicked and was
    /// then reaped by a later connection is not reported.
    pub fn join_workers(&self) {
        let handles: Vec<_> = std::mem::take(&mut *self.workers.lock().expect("workers poisoned"));
        Self::join_handles(handles);
    }

    /// Join `handles` except the caller's own: a handler ending its server must not self-join.
    fn join_handles(handles: Vec<JoinHandle<()>>) {
        let me = std::thread::current().id();
        for h in handles {
            if h.thread().id() == me {
                log::debug!(
                    "RPC: server ended from a connection worker; not joining its own thread"
                );
                continue;
            }
            if h.join().is_err() {
                log::warn!("RPC: connection worker panicked");
            }
        }
    }

    /// The bound socket path for a Unix-domain server. `None` for other
    /// backends (vsock, TCP+TLS) — the listener has no filesystem entry
    /// to expose.
    pub fn path(&self) -> Option<&Path> {
        match &self.bind {
            BindAddress::Unix { path, .. } => Some(path.as_path()),
            #[cfg(any(target_os = "linux", target_os = "android"))]
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
            BindAddress::Unix { .. } => None,
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
            BindAddress::Unix { .. } => None,
            #[cfg(any(target_os = "linux", target_os = "android"))]
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
    /// etc.).
    ///
    /// [`RpcServer::stop_accepting`] does not help there: the flag it flips is
    /// polled by the accept loop and read by the android-13+ attach arms
    /// (which refuse a late attach), but a worker already blocked in
    /// `recv` never reaches a gate that reads it. Nor does
    /// [`RpcServer::join_workers`], which waits for exactly the workers
    /// that are hung. [`RpcServer::terminate`] is the answer: it shuts
    /// every session's transports down, which wakes those workers.
    /// Short of that, what bounds a stalled peer is a deadline on its
    /// own connection — [`set_handshake_timeout`](Self::set_handshake_timeout)
    /// for a peer that stalls at first contact, and
    /// [`set_idle_timeout`](Self::set_idle_timeout) for one that goes
    /// silent afterwards (android-13+ serve path only; the default r34
    /// profile clears its read deadline after the first frame and then
    /// blocks unbounded) — or closing socket-level paths so the kernel
    /// times the peer out. (Using `Weak<Self>` plus periodic
    /// upgrade-checks in worker hot paths would remove the hold
    /// entirely, at the cost of a larger refactor.)
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Best-effort backend-specific cleanup; never panic in Drop.
        match &self.bind {
            BindAddress::Unix { path, file } => {
                // Only our own socket: another server may have bound this path since.
                if file.is_some() && socket_file_id(path) == *file {
                    let _ = std::fs::remove_file(path);
                }
            }
            #[cfg(any(target_os = "linux", target_os = "android"))]
            BindAddress::UnixAbstract => {}
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            BindAddress::Vsock { .. } => {
                // No filesystem entry; closing the listener fd releases the (cid, port).
            }
            #[cfg(feature = "rpc-tls")]
            BindAddress::Tcp(_) => {
                // No filesystem entry; closing the listener fd releases the port.
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
