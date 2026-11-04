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
//! (`RpcSessionInner::send_session_obituaries`). The recipient list and its
//! `obituary_sent` latch are the kernel proxy's type (`death::DeathRecipients`),
//! with nothing run under its lock: RPC has no
//! `BC_CLEAR_DEATH_NOTIFICATION` / `flush_commands`. A second obituary (a
//! serve loop ending after a transact already observed the close) sees the
//! latch set and returns.

use std::any::Any;
use std::sync::{self, Arc, OnceLock};

use crate::binder::{
    DeathRecipient, IBinder, ReceivedStability, Stability, Transactable, WIBinder,
};
use crate::binder::{TransactionCode, TransactionFlags};
use crate::death::DeathRecipients;
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
    /// The recipient list and `obituary_sent`, shared with the kernel proxy (`death` module).
    death: DeathRecipients,
    /// AOSP `BpBinder::mStability`: filled by `read_binder` alone; see `ReceivedStability`.
    received_level: ReceivedStability,
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
            death: DeathRecipients::new(),
            received_level: ReceivedStability::unset(),
        }
    }

    /// AOSP `Stability::setRepr` on a proxy: the first received declared level sticks.
    pub(crate) fn record_received_stability(&self, level: i32) {
        self.received_level.fill(level);
    }

    /// Fire `binder_died` on each recipient (AOSP `BpBinder::sendObituary`, RPC); idempotent.
    pub(crate) fn send_obituary(&self, who: &WIBinder) {
        // No registration to clear: RPC has no death wire message.
        let Some(snapshot) = self.death.take(|| {}) else {
            return;
        };
        // Unlocked so a recipient may re-link/unlink (AOSP `reportOneDeath`).
        DeathRecipients::dispatch(&snapshot, who, format_args!("RPC addr {:?}", self.addr));
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

    /// Not part of the API: a oneway of a parcel from
    /// [`prepare_request`](crate::RemoteProxy::prepare_request), which waits for nothing and runs
    /// no death sequence on this thread.
    ///
    /// `Ok` is sent; [`StatusCode::WouldBlock`] is not sent, with the session as it was. Anything
    /// else is a session that ended or a send that no retry can make. A caller sends one at a
    /// time per proxy, in frames up to the transport's limit; `rsbinder-topic`'s hub is the one
    /// caller (session doc "Deferred `DEC_STRONG`").
    #[doc(hidden)]
    pub fn __try_oneway(
        &self,
        code: TransactionCode,
        data: &Parcel,
        flags: TransactionFlags,
    ) -> Result<()> {
        if data.rpc_session_id().is_none() {
            return Err(StatusCode::BadType);
        }
        self.session.try_oneway(self.addr, code, data, flags)
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
        // Identity-checked; on Linux and Android's Unix socket it waits for nothing (session doc).
        self.session
            .release_proxy(self.addr, self as *const RpcProxy as *const ());
    }
}

impl std::fmt::Debug for RpcProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcProxy")
            .field("addr", &self.addr)
            .field("descriptor", &self.descriptor.get())
            .field("obituary_sent", &self.death.is_dead())
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
    ///   ([`RpcClientConfig::incoming_connections`][incoming] `≥ 1`), whose threads observe the
    ///   drop at once;
    /// - a session whose serve loop has been started —
    ///   [`RpcSession::spawn_serve`](super::session::RpcSession::spawn_serve),
    ///   or a call already inside
    ///   [`serve_blocking`](super::session::RpcSession::serve_blocking).
    ///
    /// Anywhere else a recipient would hear nothing until a later call
    /// happened to fail, so it is not registered. A call that fails on a
    /// lost connection still runs the session's death sequence, which is
    /// what fires the recipients registered on a session that qualifies.
    ///
    /// The checks run in this order: `InvalidOperation` above, then
    /// [`StatusCode::DeadObject`] after the obituary, then
    /// [`StatusCode::BadValue`] for a recipient with no strong reference left.
    ///
    /// [incoming]: super::session::RpcClientConfig::incoming_connections
    fn link_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        if !self.session.notices_connection_loss() {
            unwatched_session_link();
            return Err(StatusCode::InvalidOperation);
        }
        // No kernel registration: nothing runs under the lock.
        self.death.link(recipient, || Ok(()))
    }

    /// Unregister a death recipient (single-position, order-preserving
    /// — kernel `removeAt(i)` parity, *not* `retain`, so a duplicate
    /// registration keeps its remaining subscriptions).
    fn unlink_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        self.death.unlink(recipient, || Ok(()))
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
        crate::binder::descriptor_or_empty(
            self.try_descriptor(),
            format_args!("RPC addr {:?}", self.addr),
        )
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

    /// AOSP `getRepr` on a `BpBinder`: the first declared level received, this process's before.
    fn stability(&self) -> Stability {
        self.received_level.get()
    }
}
