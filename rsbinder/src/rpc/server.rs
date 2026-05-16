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
use super::transport::{RpcTransport, UnixTransport};

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

/// A Unix-domain RPC server.
pub struct RpcServer {
    listener: UnixListener,
    path: PathBuf,
    root: Mutex<Option<SIBinder>>,
    named: Mutex<HashMap<String, SIBinder>>,
    max_threads: Mutex<u32>,
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
    pub fn set_max_threads(&self, n: u32) {
        *self.max_threads.lock().expect("max_threads poisoned") = n.max(1);
    }

    /// Build a per-connection session sharing this server's root +
    /// negotiated max-threads (its `RpcState` is fresh — P6 isolation).
    fn make_session(&self, transport: Box<dyn RpcTransport>) -> RpcSession {
        // The server accepted this connection ⇒ Acceptor subspace.
        let session = RpcSession::new(transport, super::address::AddressSpace::Acceptor);
        if let Some(root) = self.root.lock().expect("root poisoned").clone() {
            session.set_root(root);
        }
        session.set_max_threads(*self.max_threads.lock().expect("max_threads poisoned"));
        session
    }

    /// Serve one already-connected transport on its own worker thread
    /// (used by the accept loop and by in-memory tests).
    pub fn serve_connection(self: &Arc<Self>, transport: Box<dyn RpcTransport>) {
        let session = self.make_session(transport);
        let handle = std::thread::spawn(move || {
            if let Err(e) = session.serve_blocking() {
                log::debug!("RPC session ended: {e:?}");
            }
        });
        self.workers.lock().expect("workers poisoned").push(handle);
    }

    /// Run the accept loop until [`RpcServer::shutdown`]. Each accepted
    /// connection gets its own session + worker thread.
    pub fn run(self: &Arc<Self>) -> Result<()> {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
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
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => {
                    log::debug!("accept loop ending: {e}");
                    break;
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

    /// Join all finished session workers (best effort — call after the
    /// clients disconnect).
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
