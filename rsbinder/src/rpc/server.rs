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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// Subplan 2-12 **Phase A0b**: session-id → shared-session registry
    /// (AOSP `RpcServer::mSessions`). The android-13+ accept handshake
    /// reads the client's `RpcConnectionHeader.sessionId`:
    ///  - **empty** id (the default — every pre-2-12 client) ⇒ a
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
    /// Live (written on every mint, resolved on every non-empty id) —
    /// the AC-12.0b mutant (attach → fresh session = pre-A0b code) is
    /// observably caught (`attached_count` stays 0, the 2nd connection
    /// can't reach the founding connection's binder).
    sessions: Mutex<HashMap<[u8; 32], std::sync::Weak<crate::rpc::session::SharedSession>>>,
    /// A0a/A0b observability (also the AC-12.0b mutant gate). Plain
    /// atomics off the per-transaction path — zero-cost on the default
    /// (empty-id) flow. `session_registered` = new-session mints;
    /// `attached_count` = id-demux attaches (Phase A0b); `rejected
    /// _unknown_id` = non-empty ids that resolved to no live session.
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
            sessions: Mutex::new(HashMap::new()),
            session_registered: AtomicUsize::new(0),
            attached_count: AtomicUsize::new(0),
            rejected_unknown_id: AtomicUsize::new(0),
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

    // --- Subplan 2-12 Phase A0b: session-id → shared-session registry

    /// Register a newly-minted session's shared state under its 32-byte
    /// id (new-session / empty-id accept path). Stored as a `Weak` so a
    /// fully-torn-down session does not pin memory and its id, if later
    /// echoed, resolves to "unknown".
    fn register_session(&self, id: [u8; 32], shared: &Arc<crate::rpc::session::SharedSession>) {
        let mut map = self.sessions.lock().expect("sessions poisoned");
        // Opportunistically prune fully-dead sessions so the map is
        // bounded by *live* sessions, not cumulative over the server's
        // lifetime (random 32-byte ids never collide in practice, so a
        // dead `Weak` would otherwise linger forever).
        map.retain(|_, w| w.strong_count() > 0);
        map.insert(id, Arc::downgrade(shared));
        drop(map);
        self.session_registered.fetch_add(1, Ordering::SeqCst);
    }

    /// Drop a session's id on full teardown so the map keeps tracking
    /// **live** sessions (no unbounded growth over a long-lived
    /// server). Unconditional `remove`: 32-byte CSPRNG ids are
    /// collision-free in practice, so an entry being removed is always
    /// the session that just exited (no later same-id session can have
    /// been minted in the meantime). The F4 race window between this
    /// `remove` and the founding worker's `live_conns.fetch_sub` is
    /// closed by [`SharedSession::try_bump_live_conns`] at the attach
    /// side, not by an identity check here.
    fn unregister_session(&self, id: &[u8; 32]) {
        self.sessions.lock().expect("sessions poisoned").remove(id);
    }

    /// Resolve a client-echoed id to a **live** shared session
    /// (Phase A0b id-demux). `None` for any non-32-byte id (AOSP
    /// `kSessionIdBytes == 32`), an unknown id, or a stale `Weak`
    /// (session fully torn down) — all of which the caller rejects.
    fn resolve_session(&self, id: &[u8]) -> Option<Arc<crate::rpc::session::SharedSession>> {
        let key = <[u8; 32]>::try_from(id).ok()?;
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .get(&key)
            .and_then(std::sync::Weak::upgrade)
    }

    /// A0a/A0b observability (and the AC-12.0b mutant gate).
    /// Respectively: new-session ids registered; **id-demux attaches**
    /// (Phase A0b — a 2nd+ connection bound to a pre-existing shared
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

    /// Phase A **F7** leak observability: total live local-node count
    /// across all currently-live registered sessions (dead `Weak`s
    /// skipped). The AOSP `timesSent`/`flushExcessBinderRefs` books
    /// must net to **0** once every client proxy is dropped — a value
    /// stuck above baseline is the F7 leak the no-excess-DEC mutant
    /// reintroduces.
    ///
    /// Lock ladder: collect the live `Arc<SharedSession>` snapshot
    /// **first** (releasing the `sessions` mutex), then walk each
    /// session's `state` mutex. Avoids the nested-lock pattern
    /// (`sessions` → `state`); a poisoned `state` lock in one session
    /// no longer poisons `sessions` as a side-effect.
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
                // Subplan 2-11 Phase A0: the AOSP handshake reads the
                // client's `RpcConnectionHeader.fileDescriptorTransport
                // Mode`; honor `Unix` only if this server opted in
                // (`set_supported_fd_modes`) — else degrade to `None`
                // (the fd write then `BAD_TYPE`-rejects). `false` keeps
                // the byte-identical no-FD android-13+ path.
                let fd_unix = server.fd_unix_supported.load(Ordering::SeqCst);
                std::thread::spawn(move || {
                    // Subplan 2-12 Phase A0b: split the handshake from
                    // the session build so the server can read the
                    // client-supplied `RpcConnectionHeader.sessionId`
                    // and decide **new-session vs. id-demux attach vs.
                    // reject** *before* binding the connection to a
                    // `SharedSession`.
                    let (transport, codec, client_fd_mode, client_id) =
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
                    if client_id.is_empty() {
                        // Default / new-session path — byte-identical
                        // to pre-2-12 (empty id ⇒ fresh `SharedSession`,
                        // `live_conns == 1`). Register a `Weak` of it so
                        // a later echo of its id can demux-attach; the
                        // id is *never resolved* on this path, so the
                        // default behavior is unchanged.
                        let session = RpcSession::from_android13plus(
                            transport,
                            codec,
                            client_fd_mode,
                            fd_unix,
                            None,
                        );
                        let id = session.session_id();
                        server.register_session(id, &session.shared());
                        server.configure_session(&session);
                        if let Err(e) = session.serve_blocking() {
                            log::debug!("RPC session ended: {e:?}");
                        }
                        server.unregister_session(&id);
                    } else if let Some(shared) = server.resolve_session(&client_id) {
                        // Phase A0b **id-demux attach**: bind this
                        // connection to the *pre-existing* session, so a
                        // binder published over the founding connection
                        // is reachable here (shared `state`/`root`).
                        //
                        // **F4 anti-resurrection gate**: the founding
                        // worker may have raced past `live_conns 1→0`
                        // and fired obituaries between
                        // `resolve_session.upgrade()` and now (the
                        // founding session's `Arc<SharedSession>` is
                        // still alive on the founding closure stack, so
                        // the `Weak::upgrade` succeeded — but the
                        // session is already dead). `try_bump_live_conns`
                        // closes that window: on rollback the attach
                        // path falls through to the unknown-id reject
                        // arm so the attaching client gets `DeadObject`
                        // instead of silently joining a session whose
                        // `binder_died` already fired (or, worse, never
                        // fires again for this connection's
                        // `DeathRecipient`s).
                        if !shared.try_bump_live_conns() {
                            server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                            log::warn!(
                                "android-13+ RPC: session torn down between resolve \
                                 and attach (F4 race); rejecting"
                            );
                            drop(transport);
                            return;
                        }
                        // Bump `attached_count` **immediately** after
                        // the successful live-conns bump so an external
                        // observer (e.g. AC-12.0b's `poll_until`) never
                        // sees the intermediate state where
                        // `live_conns` is incremented but
                        // `attached_count` isn't. The original ordering
                        // (`from_android13plus` between the two
                        // counters) opened a tiny window — harmless
                        // for current tests but contradicts the
                        // "attached_count tracks id-demux attaches"
                        // doc-claim.
                        server.attached_count.fetch_add(1, Ordering::SeqCst);
                        // No re-`configure_session`: the founding
                        // connection already set the session's
                        // root/max-threads; re-applying would clobber a
                        // customized session.
                        let session = RpcSession::from_android13plus(
                            transport,
                            codec,
                            client_fd_mode,
                            fd_unix,
                            Some(shared),
                        );
                        if let Err(e) = session.serve_blocking() {
                            log::debug!("RPC attached connection ended: {e:?}");
                        }
                        // The founding connection owns
                        // register/unregister of the id; a stale `Weak`
                        // is reclaimed by `resolve_session`/`register`
                        // pruning, so the attached connection need not
                        // unregister.
                    } else {
                        // Non-empty id resolving to no live session
                        // (unknown / stale / non-32-byte) ⇒ reject
                        // (AOSP `ALOGE`+return). The handshake already
                        // completed (rsbinder's accept is split only at
                        // the *build* level, not the wire level — a
                        // documented A0a/A0b residual); dropping
                        // `transport` closes the socket and the
                        // client's next op is `DeadObject`.
                        server.rejected_unknown_id.fetch_add(1, Ordering::SeqCst);
                        log::warn!(
                            "android-13+ RPC: client supplied an unknown/stale \
                             session id; rejecting connection"
                        );
                        drop(transport);
                    }
                })
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
