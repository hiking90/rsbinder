// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcProxy` — client-side handle to a remote RPC object.
//!
//! A **distinct `IBinder` type** from `proxy::ProxyHandle`. It never
//! goes through the u32 kernel handle / `handle_to_proxy` / cache-pin
//! machinery — RPC has its own `RpcAddress` identity space and
//! its own ref-count. Android made `BpBinder` a dual-mode
//! `variant<BinderHandle, RpcHandle>`; because rsbinder's `IBinder` is
//! a trait, a separate type is cleaner.
//!
//! The generated `Bp*` stub calls through a
//! [`RemoteProxy`](crate::RemoteProxy) trait object (`as_remote()`)
//! rather than the kernel-only `as_proxy()`, so it drives this proxy
//! directly — the same single stub also drives the kernel `ProxyHandle`.
//!
//! # Descriptor
//!
//! The RPC wire transmits only an address, not a descriptor string, so a
//! proxy resolved from `read_binder`/`get_root` does not know its interface.
//! As a kernel proxy, it asks the remote with a two-way
//! `INTERFACE_TRANSACTION` when something first needs the descriptor
//! (`try_descriptor`, AOSP `BpBinder::getInterfaceDescriptor`, which serves
//! RPC binders too), caches the first successful answer, and caches no
//! failure. Unlike a kernel proxy, the query is not subject to the thread's
//! `CallRestriction`: AOSP's RPC `transact` does not pass through
//! `IPCThreadState`. The cache lives as long as this proxy; the session's
//! table holds proxies weakly, so a proxy received after the last one died
//! asks again, as AOSP's RPC `BpBinder` does.
//!
//! A typed cast (`from_binder`) neither asks nor records anything, so one
//! address may be cast to several interfaces; a wrong one is refused by the
//! server's interface-token check on the first two-way call.
//!
//! # Session
//!
//! A proxy holds its session strongly (AOSP `BpBinder::RpcSessionBinder`
//! holds `sp<RpcSession>`), so a proxy alone keeps its session, and thus the
//! connection, alive. The session's `remote_proxies` table is `Weak`, so the
//! only cycle back to the session runs through a local object the peer
//! holds. `RpcSessionInner::on_session_dead` breaks it; it runs when a serve
//! loop ends, when a transaction fails on a lost connection, or on an
//! explicit [`RpcSession::close_session`](super::RpcSession::close_session),
//! the last being the only break available to a session that neither serves
//! nor transacts again.
//!
//! # Death notification
//!
//! Death state mirrors the kernel `ProxyHandle`. RPC has no death wire
//! message (AOSP `RpcState::sendObituaries`): an RPC object dies when its
//! session connection drops, so the session fires every cached proxy's
//! obituary when its serve loop ends
//! (`RpcSessionInner::send_session_obituaries`). `send_obituary` follows
//! the kernel `ProxyHandle::send_obituary` state machine minus the
//! kernel-only `BC_CLEAR_DEATH_NOTIFICATION` / `flush_commands`.
//! `obituary_sent` is set once the obituary is dispatched and is touched only
//! under the `recipients` lock (kernel `mLock` parity), which supplies
//! the happens-before, hence `Relaxed` throughout. A second call (a serve
//! loop ending after a transact already observed the close) sees it set and
//! returns.

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{self, Arc, Mutex, OnceLock};

use crate::binder::{DeathRecipient, IBinder, Stability, Transactable, WIBinder};
use crate::binder::{TransactionCode, TransactionFlags};
use crate::error::{Result, StatusCode};
use crate::parcel::Parcel;

use super::address::RpcAddress;
use super::session::RpcSessionInner;

/// A handle to a remote object reachable over an RPC session.
pub struct RpcProxy {
    addr: RpcAddress,
    /// The first successful `INTERFACE_TRANSACTION` answer; module doc "Descriptor".
    descriptor: OnceLock<String>,
    /// Strong, as AOSP `sp<RpcSession>`: a proxy keeps its session alive; see module doc "Session".
    session: Arc<RpcSessionInner>,
    /// Set once the obituary is dispatched; only under the `recipients` lock, so `Relaxed`.
    obituary_sent: AtomicBool,
    recipients: Mutex<Vec<sync::Weak<dyn DeathRecipient>>>,
}

impl RpcProxy {
    /// Identity of the owning session, for `write_binder`'s same-session check.
    pub(crate) fn session_ptr(&self) -> *const RpcSessionInner {
        Arc::as_ptr(&self.session)
    }

    pub(crate) fn new(addr: RpcAddress, session: Arc<RpcSessionInner>) -> Self {
        RpcProxy {
            addr,
            descriptor: OnceLock::new(),
            session,
            obituary_sent: AtomicBool::new(false),
            recipients: Mutex::new(Vec::new()),
        }
    }

    /// Fire `binder_died` on each recipient (AOSP `BpBinder::sendObituary`, RPC); idempotent.
    pub(crate) fn send_obituary(&self, who: &WIBinder) {
        let snapshot: Vec<sync::Weak<dyn DeathRecipient>> = {
            // `obituary_sent` is touched only under this lock; poison logs, never panics.
            let mut recipients = match self.recipients.lock() {
                Ok(g) => g,
                Err(_) => {
                    log::error!(
                        "RPC obituary skipped: recipients lock poisoned for addr {:?}",
                        self.addr
                    );
                    return;
                }
            };
            if self.obituary_sent.load(Ordering::Relaxed) {
                return;
            }
            let snapshot = std::mem::take(&mut *recipients);
            self.obituary_sent.store(true, Ordering::Relaxed);
            snapshot
        };
        // Unlocked so a recipient may re-link/unlink (AOSP `reportOneDeath`); panic-isolated.
        for weak in &snapshot {
            let Some(recipient) = weak.upgrade() else {
                continue;
            };
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                recipient.binder_died(who);
            }));
            if let Err(payload) = r {
                let msg = payload
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string panic payload>");
                log::error!(
                    "DeathRecipient panicked during binder_died for RPC addr {:?}: {msg}",
                    self.addr
                );
            }
        }
    }

    /// The remote object's RPC address.
    pub fn address(&self) -> RpcAddress {
        self.addr
    }

    /// The session this proxy calls over; the reconnect helper watches it.
    pub(crate) fn session(&self) -> super::RpcSession {
        super::RpcSession::wrap_inner(self.session.clone())
    }

    /// The remote object's interface descriptor, asked for on the first call (AOSP
    /// `BpBinder::getInterfaceDescriptor`).
    ///
    /// The first successful answer is cached for this proxy, and later calls make no
    /// transaction. A failed query is returned as its error and not cached, so the next call
    /// asks again; a closed session fails with [`StatusCode::DeadObject`]. The query is a
    /// two-way call, so it waits for the remote and needs a connection to send on, as any
    /// two-way call does; the thread's `CallRestriction` does not apply (module doc
    /// "Descriptor").
    pub fn try_descriptor(&self) -> Result<&str> {
        if let Some(descriptor) = self.descriptor.get() {
            return Ok(descriptor);
        }
        // Not under any lock; two first callers may both ask, and the first answer is kept.
        let reply = self.session.client_transact(
            self.addr,
            crate::binder::INTERFACE_TRANSACTION,
            &Parcel::new(),
            0,
        )?;
        let descriptor = crate::binder::read_interface_reply(reply)?;
        Ok(self.descriptor.get_or_init(|| descriptor))
    }

    /// Build an RPC-mode request `Parcel` for `descriptor`, with the
    /// session's object hooks attached and the interface token written.
    ///
    /// The same call as [`RemoteProxy::prepare_request`](crate::RemoteProxy::prepare_request),
    /// which a kernel proxy answers too.
    #[deprecated(
        since = "0.13.0",
        note = "use `RemoteProxy::prepare_request` (`use rsbinder::RemoteProxy;`), the same call on either stack"
    )]
    pub fn build_request(&self, descriptor: &str) -> Result<Parcel> {
        self.request_parcel(descriptor)
    }

    /// `prepare_request`'s body, for the crate's own callers.
    pub(crate) fn request_parcel(&self, descriptor: &str) -> Result<Parcel> {
        let inner = &self.session;
        let mut data = Parcel::new();
        // FD policy defaults to `None`, so `ParcelFileDescriptor::serialize` rejects FDs.
        data.configure_rpc(
            inner.parcel_ops(),
            inner.fd_mode(),
            inner.records_fd_positions(),
        );
        super::session::write_rpc_interface_token(&mut data, descriptor)?;
        Ok(data)
    }

    /// `RpcSession::caps` for a caller holding only the binder (e.g. from `read_binder`).
    pub(crate) fn session_caps(&self) -> crate::TransportCaps {
        self.session.caps()
    }

    /// This proxy's session's activity counter, for traffic that does not cross its sockets.
    pub(crate) fn session_activity(&self) -> super::session::SessionActivity {
        self.session.ring_activity()
    }

    /// The reply deadline of this proxy's session, for a caller holding only the binder.
    pub(crate) fn session_timeout(&self) -> Option<std::time::Duration> {
        self.session.timeout()
    }

    /// Send an outbound transaction to the remote object. Returns the
    /// reply parcel (`None` for oneway).
    ///
    /// The same call as [`RemoteProxy::submit_transact`](crate::RemoteProxy::submit_transact),
    /// whose doc on this type says which parcels it refuses.
    #[deprecated(
        since = "0.13.0",
        note = "use `RemoteProxy::submit_transact` (`use rsbinder::RemoteProxy;`), the same call on either stack"
    )]
    pub fn transact(
        &self,
        code: TransactionCode,
        data: &Parcel,
        flags: TransactionFlags,
    ) -> Result<Option<Parcel>> {
        self.send(code, data, flags)
    }

    /// `submit_transact`'s body, for the crate's own callers.
    pub(crate) fn send(
        &self,
        code: TransactionCode,
        data: &Parcel,
        flags: TransactionFlags,
    ) -> Result<Option<Parcel>> {
        // AOSP `validateParcel`; internal specials send `Parcel::new()` via `client_transact`.
        if data.rpc_session_id().is_none() {
            log::error!("RPC: the parcel was built for no session; use `prepare_request`");
            return Err(StatusCode::BadType);
        }
        self.session.client_transact(self.addr, code, data, flags)
    }
}

/// The RPC proxy implements the same generalized
/// [`RemoteProxy`](crate::RemoteProxy) trait as the kernel
/// `ProxyHandle`, so the one generated `Bp*` stub drives either stack
/// (through `as_remote()`). `prepare_transact(true)` writes
/// the remote's descriptor as the interface token, asking for it first
/// if it is not cached (`try_descriptor`).
///
/// [`submit_transact`](crate::RemoteProxy::submit_transact) sends a parcel
/// at most once: after `Ok` the same parcel is refused with
/// [`StatusCode::InvalidOperation`], as is a reply received from a
/// session. A parcel built by another session's proxy is refused with
/// [`StatusCode::BadType`] (AOSP `RpcState::validateParcel`), and so is a
/// parcel built for no session at all, such as [`Parcel::new`]: build the
/// request with [`prepare_request`](crate::RemoteProxy::prepare_request)
/// or [`prepare_transact`](crate::RemoteProxy::prepare_transact). After an
/// error raised before the send (`WouldBlock`, `DeadObject`, an encode
/// failure) it may be sent again as is, or dropped, which releases the
/// binders it reserved.
impl crate::binder::RemoteProxy for RpcProxy {
    fn prepare_transact(&self, write_header: bool) -> Result<Parcel> {
        let inner = &self.session;
        let mut data = Parcel::new();
        data.configure_rpc(
            inner.parcel_ops(),
            inner.fd_mode(),
            inner.records_fd_positions(),
        );
        if write_header {
            super::session::write_rpc_interface_token(&mut data, self.try_descriptor()?)?;
        }
        Ok(data)
    }

    fn prepare_request(&self, interface: &str) -> Result<Parcel> {
        self.request_parcel(interface)
    }

    fn submit_transact(
        &self,
        code: TransactionCode,
        data: &Parcel,
        flags: TransactionFlags,
    ) -> Result<Option<Parcel>> {
        self.send(code, data, flags)
    }
}

impl Drop for RpcProxy {
    fn drop(&mut self) {
        // Identity-checked; never waits on a slot (session doc "Deferred `DEC_STRONG`").
        self.session
            .release_proxy(self.addr, self as *const RpcProxy as *const ());
    }
}

impl std::fmt::Debug for RpcProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcProxy")
            .field("addr", &self.addr)
            .field("descriptor", &self.descriptor.get())
            .field("obituary_sent", &self.obituary_sent.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Why an RPC `link_to_death` was refused; logged each time (the RPC stack keeps no globals).
fn unwatched_session_link() {
    log::error!(
        "link_to_death over RPC refused: nothing reads this session's connections, so its \
         loss would go unnoticed. Open incoming connections \
         (RpcClientConfig::incoming_connections / ClientOptions::incoming_connections), \
         or start a serve loop with RpcSession::spawn_serve first if this session makes no \
         further calls of its own: the loop holds the only connection."
    );
}

impl IBinder for RpcProxy {
    /// Register a death recipient. Death over RPC = the **session
    /// connection dropping** (AOSP `RpcState::sendObituaries`): the
    /// recipient fires when the session's serve loop ends. Mirrors the
    /// kernel [`ProxyHandle::link_to_death`](crate::proxy::ProxyHandle)
    /// minus the kernel `requestDeathNotification` IPC (RPC has no
    /// death wire message).
    ///
    /// **Refused with [`StatusCode::InvalidOperation`] on a session that
    /// would not notice the loss** — AOSP `BpBinder::linkToDeath` refuses an
    /// RPC binder the same way unless its session has incoming threads. A
    /// session notices a connection loss when something reads its
    /// connections at all times:
    ///
    /// - the server side of a session (its workers serve every connection);
    /// - a client with incoming connections
    ///   ([`RpcClientConfig::incoming_connections`](super::session::RpcClientConfig::incoming_connections)
    ///   `≥ 1`), whose threads observe the drop at once;
    /// - a session whose serve loop has been started —
    ///   [`RpcSession::spawn_serve`](super::session::RpcSession::spawn_serve),
    ///   or a call already inside
    ///   [`serve_blocking`](super::session::RpcSession::serve_blocking).
    ///
    /// Anywhere else a recipient would hear nothing until a later call
    /// happened to fail, so it is not registered. A call that fails on a
    /// lost connection still runs the session's death sequence, which is
    /// what fires the recipients registered on a session that qualifies.
    fn link_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        if !self.session.notices_connection_loss() {
            unwatched_session_link();
            return Err(StatusCode::InvalidOperation);
        }
        // Check `obituary_sent` under the lock, as AOSP `linkToDeath` checks `mObitsSent`.
        let mut recipients = self.recipients.lock().map_err(|_| {
            // Poison = a prior panic on the death path; `DeadObject` lets the caller reconnect.
            StatusCode::DeadObject
        })?;
        if self.obituary_sent.load(Ordering::Relaxed) {
            // Connection already dropped — AOSP returns DEAD_OBJECT.
            return Err(StatusCode::DeadObject);
        }
        recipients.push(recipient);
        Ok(())
    }

    /// Unregister a death recipient (single-position, order-preserving
    /// — kernel `removeAt(i)` parity, *not* `retain`, so a duplicate
    /// registration keeps its remaining subscriptions).
    fn unlink_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        let mut recipients = self.recipients.lock().map_err(|_| {
            // Poison = a prior panic on the death path; `DeadObject` lets the caller reconnect.
            StatusCode::DeadObject
        })?;
        if self.obituary_sent.load(Ordering::Relaxed) {
            return Err(StatusCode::DeadObject);
        }
        let Some(i) = recipients
            .iter()
            .position(|r| sync::Weak::ptr_eq(r, &recipient))
        else {
            return Err(StatusCode::NameNotFound);
        };
        recipients.remove(i);
        Ok(())
    }

    fn ping_binder(&self) -> Result<()> {
        // PING_TRANSACTION round-trip (no payload, no reply body).
        let data = Parcel::new();
        self.session
            .client_transact(self.addr, crate::binder::PING_TRANSACTION, &data, 0)?;
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_transactable(&self) -> Option<&dyn Transactable> {
        None
    }

    fn descriptor(&self) -> &str {
        self.try_descriptor().unwrap_or_else(|err| {
            log::warn!(
                "RPC addr {:?}: interface descriptor query failed: {err:?}",
                self.addr
            );
            ""
        })
    }

    fn try_descriptor(&self) -> Result<&str> {
        RpcProxy::try_descriptor(self)
    }

    fn cached_descriptor(&self) -> Option<&str> {
        self.descriptor.get().map(String::as_str)
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn stability(&self) -> Stability {
        Stability::default()
    }
}
