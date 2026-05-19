// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcServer` — bind / listen / accept, one session per connection
//! (subplan 2-3).
//!
//! Model: **one connection ⇒ one [`RpcSession`] ⇒ one worker thread**,
//! each with its own [`super::state::RpcState`] (P6 — no global, so
//! sessions are isolated and the suite is parallel-safe). Concurrent
//! clients use independent connections; nested re-entrant calls run
//! inline on a connection's worker (the `client_transact` recv loop
//! dispatches inbound `TRANSACT`s — AC-3.6). The exact android-12 r34
//! multi-connection-per-session thread pool is a faithful future
//! refinement; the *semantics* (concurrency-correct, isolated, oneway
//! FIFO, negotiated, timed-out) are met here.
//!
//! Naming follows the §7-4 decision: android semantics, snake_case
//! (`setup_unix_server`, `get_root`, `add_service`, `set_max_threads`).
//!
//! **P1:** kernel files untouched — `RpcServer` is new code only.

use std::collections::HashMap;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::binder::{Interface, Remotable, SIBinder, TransactionCode};
use crate::error::{Result, StatusCode};
use crate::native::Binder;
use crate::parcel::Parcel;

use super::session::RpcSession;
use super::transport::{PeerIdentity, RpcTransport, UnixTransport};

/// Built-in directory interface descriptor + its single transaction.
const DIRECTORY_DESC: &str = "rsbinder.rpc.IServiceDirectory";
const TX_GET_SERVICE: TransactionCode = crate::binder::FIRST_CALL_TRANSACTION;

/// Built-in name → binder directory, used to back [`RpcServer::add_service`]
/// (android RPC has a single root object; this *is* that root when
/// named services are registered). Reused, unmodified, via the same
/// `Remotable::on_transact` server path as any AIDL stub.
struct ServiceDirectory {
    services: HashMap<String, SIBinder>,
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
                match self.services.get(&name) {
                    Some(b) => {
                        reply.write(&crate::Status::from(StatusCode::Ok))?;
                        reply.write(b)
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

/// Authorization hook (subplan 2-9 Phase B): given the connecting
/// peer's [`PeerIdentity`], return `true` to admit, `false` to refuse
/// (the connection is closed before any RPC byte). `Arc` so it can be
/// cloned out of the lock and invoked lock-free.
type Authorizer = Arc<dyn Fn(&PeerIdentity) -> bool + Send + Sync>;

/// A Unix-domain RPC server.
pub struct RpcServer {
    listener: UnixListener,
    path: PathBuf,
    root: Mutex<Option<SIBinder>>,
    named: Mutex<HashMap<String, SIBinder>>,
    max_threads: Mutex<u32>,
    /// Whether per-connection sessions advertise `Unix` FD support
    /// (subplan 2-7; default false ⇒ FD reject everywhere).
    fd_unix_supported: AtomicBool,
    /// Opt-in android-13+ versioned wire (subplan 2-5b / G4(a)):
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
    /// (that would require I/O multiplexing — explicitly out of scope,
    /// see `plans/2-10-async-rpc-io.md`).
    max_connections: Mutex<Option<usize>>,
    /// Opt-in authorization hook (subplan 2-9 Phase B). `None`
    /// (default) ⇒ accept-all = byte-for-byte the prior behavior
    /// (additive invariant — AC-9.4). When set, it runs at
    /// [`serve_connection`](RpcServer::serve_connection) entry —
    /// **before** the wire-profile branch, session build, handshake,
    /// or any `recv_frame` — so a rejected peer receives **zero RPC
    /// bytes** (the connection is closed). Backend-independent: it is
    /// pure on [`RpcTransport::peer_identity`] (unix `SO_PEERCRED`/
    /// `getpeereid`, tls cert, vsock cid, …). `Arc` (not `Box`) so the
    /// hook is cloned out of the lock and invoked **lock-free**, so a
    /// hook may itself touch the server without self-deadlock (same
    /// discipline as `RpcProxy::send_obituary`). This is the
    /// *enforcement point* the 2-9 §0/G2 gap identified as missing
    /// (`peer_identity()` was computed but read nowhere).
    authorizer: Mutex<Option<Authorizer>>,
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
        // Non-blocking accept so the loop can observe `shutdown`.
        listener.set_nonblocking(true)?;
        Ok(Arc::new(RpcServer {
            listener,
            path,
            root: Mutex::new(None),
            named: Mutex::new(HashMap::new()),
            max_threads: Mutex::new(1),
            fd_unix_supported: AtomicBool::new(false),
            wire_max_version: Mutex::new(None),
            max_connections: Mutex::new(None),
            authorizer: Mutex::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
            workers: Mutex::new(Vec::new()),
        }))
    }

    /// Publish the single root object (android `setRootObject`).
    pub fn set_root(&self, binder: SIBinder) {
        *self.root.lock().expect("root poisoned") = Some(binder);
    }

    /// Register a named service. The first call makes the root a
    /// built-in `ServiceDirectory` (rebuilt on each call so the set
    /// is consistent); clients reach it via
    /// [`RpcSession::get_service`].
    pub fn add_service(&self, name: &str, binder: SIBinder) -> Result<()> {
        let mut named = self.named.lock().expect("named poisoned");
        named.insert(name.to_string(), binder);
        let dir = ServiceDirectory {
            services: named.clone(),
        };
        let root = Interface::as_binder(&Binder::new(dir));
        *self.root.lock().expect("root poisoned") = Some(root);
        Ok(())
    }

    /// Advertised max-threads (negotiation, AC-3.4). Default 1.
    ///
    /// This is **only** the value advertised to clients on
    /// `GET_MAX_THREADS`; it does *not* bound server-side resources.
    /// To cap concurrent connection workers use
    /// [`set_max_connections`](RpcServer::set_max_connections).
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
    /// multiplexing) make workers fewer than connections — see the
    /// `plans/2-10-async-rpc-io.md` decision record.
    pub fn set_max_connections(&self, n: usize) {
        *self
            .max_connections
            .lock()
            .expect("max_connections poisoned") = Some(n.max(1));
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

    /// Opt-in **authorization hook** (subplan 2-9 Phase B). `f` is
    /// invoked once per accepted connection with the peer's
    /// [`PeerIdentity`] **before any RPC byte is exchanged**; returning
    /// `false` closes the connection immediately (the peer's next op
    /// sees `DeadObject` — RPC payload zero bytes, the local-transport
    /// analogue of subplan 2-4's TLS reject). Unset (default) =
    /// accept-all = the prior behavior, byte-for-byte (AC-9.4) — so
    /// this is purely additive and satisfies the user's *opt-in*
    /// "mutual authentication when needed" constraint with no cost
    /// when off.
    ///
    /// rsbinder provides only the gate; the policy is the caller's
    /// closure (subplan 2-4 philosophy), e.g.
    /// `|p| p.uid() == Some(EXPECTED_UID)` or, with the
    /// `rpc-macos-codesign` feature (Phase C),
    /// `matches!(p, PeerIdentity::CodeSigned(c) if c.team_id() == Some("TEAMID"))`.
    /// Backend-independent (unix/mem/tls/vsock). The hook must not
    /// block indefinitely (it runs on the accept path).
    pub fn set_authorizer<F>(&self, f: F)
    where
        F: Fn(&PeerIdentity) -> bool + Send + Sync + 'static,
    {
        *self.authorizer.lock().expect("authorizer poisoned") = Some(Arc::new(f));
    }

    /// Advertise the FD-over-RPC modes this server will accept
    /// (subplan 2-7). Default: only `None` (the 2-2 reject). Pass
    /// `&[FileDescriptorTransportMode::Unix]` to opt in to UDS
    /// `SCM_RIGHTS` fd passing for clients that also opt in.
    pub fn set_supported_fd_modes(&self, modes: &[crate::rpc::FileDescriptorTransportMode]) {
        let unix = modes.contains(&crate::rpc::FileDescriptorTransportMode::Unix);
        self.fd_unix_supported.store(unix, Ordering::SeqCst);
    }

    /// Opt in to the **android-13+ versioned RPC wire** (subplan 2-5b /
    /// G4(a)). `max_version` is the highest `RPC_WIRE_PROTOCOL_VERSION`
    /// this server offers (`0` = android-13, `1` = android-14/15,
    /// **`2` = android-16** — subplan 2-8); each accepted connection
    /// then runs the AOSP connection handshake and negotiates
    /// `min(max_version, client_max)`. Default (unset) keeps the
    /// android-12 r34 wire, byte-unchanged. Has effect only on a
    /// transport with raw byte access (`unix`).
    ///
    /// **Sequencing (subplan 2-8 §0.3/§9):** advertising `2` is sound
    /// only because the Parcel binder/FD object-position producer
    /// (Phase B — [`Parcel::rpc_record_object_position`], the
    /// `records_binder_positions`/`records_fd_positions` profile gate)
    /// is compiled in unconditionally here. Were this a Phase-A-only
    /// build, `2` would frame a *binder-bearing* parcel with an empty
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
    /// is fresh — P6 isolation). Shared by the r34 and android-13+
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
    /// negotiated max-threads (its `RpcState` is fresh — P6 isolation).
    fn make_session(&self, transport: Box<dyn RpcTransport>) -> RpcSession {
        // The server accepted this connection ⇒ Acceptor subspace.
        let session = RpcSession::new(transport, super::address::AddressSpace::Acceptor);
        self.configure_session(&session);
        session
    }

    /// Serve one already-connected transport on its own worker thread
    /// (used by the accept loop and by in-memory tests).
    pub fn serve_connection(self: &Arc<Self>, transport: Box<dyn RpcTransport>) {
        // Subplan 2-9 Phase B: authorization gate. The single
        // chokepoint common to r34, android-13+, AND in-memory test
        // direct calls — *before* the wire-profile branch, session
        // build, handshake, or any `recv_frame`, so a rejected peer
        // gets zero RPC bytes. The hook is cloned out of the lock and
        // called lock-free (a hook may touch the server without
        // self-deadlock). Default (unset) ⇒ this whole block is a
        // no-op and behavior is byte-identical (additive — AC-9.4).
        let authorizer = self.authorizer.lock().expect("authorizer poisoned").clone();
        if let Some(authz) = authorizer {
            let peer = transport.peer_identity();
            if !authz(&peer) {
                // `transport` drops here → socket closed, no worker
                // spawned; the peer's next op is `DeadObject`.
                log::warn!("RPC connection rejected by authorizer: peer {peer:?}");
                return;
            }
        }
        let a13_max = *self
            .wire_max_version
            .lock()
            .expect("wire_max_version poisoned");
        let handle = match a13_max {
            Some(max) => {
                // android-13+ (G4(a)): the AOSP connection handshake is
                // blocking I/O on the accepted socket, so it must run on
                // the worker — never the accept loop. Build + configure
                // the session AFTER the handshake, then serve. A
                // handshake failure ends just this connection (the
                // accept loop and other sessions are unaffected).
                let server = Arc::clone(self);
                std::thread::spawn(
                    move || match RpcSession::accept_android13plus(transport, max) {
                        Ok(session) => {
                            server.configure_session(&session);
                            if let Err(e) = session.serve_blocking() {
                                log::debug!("RPC session ended: {e:?}");
                            }
                        }
                        Err(e) => {
                            // Abnormal interop/security event (version
                            // mismatch, truncated header, hostile peer)
                            // — not the routine post-handshake
                            // peer-close drain, so `warn!` not `debug!`.
                            log::warn!("android-13+ RPC handshake failed: {e:?}")
                        }
                    },
                )
            }
            None => {
                // r34 (default) — unchanged: session built here, served
                // on the worker.
                let session = self.make_session(transport);
                std::thread::spawn(move || {
                    if let Err(e) = session.serve_blocking() {
                        log::debug!("RPC session ended: {e:?}");
                    }
                })
            }
        };
        let mut workers = self.workers.lock().expect("workers poisoned");
        // Reap handles of already-finished sessions so `workers` is
        // bounded by *concurrent* (not cumulative) connections — a
        // long-lived server otherwise leaks one JoinHandle per
        // connection ever accepted. Dropping a finished handle detaches
        // without blocking, so this cannot stall the accept loop.
        workers.retain(|h| !h.is_finished());
        workers.push(handle);
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
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    // The listener is non-blocking so the accept loop
                    // can poll `shutdown`; accepted connections must be
                    // blocking for the worker's `recv_frame`.
                    stream.set_nonblocking(false)?;
                    let t = UnixTransport::from_stream(stream)?;
                    self.serve_connection(Box::new(t));
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
    /// than relying on `Drop` (Minor-3).
    pub fn join_workers(&self) {
        let handles: Vec<_> = std::mem::take(&mut *self.workers.lock().expect("workers poisoned"));
        for h in handles {
            let _ = h.join();
        }
    }

    /// The bound socket path.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for RpcServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Best-effort socket file cleanup; never panic in Drop.
        let _ = std::fs::remove_file(&self.path);
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
}
