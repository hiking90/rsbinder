// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcProxy` — client-side handle to a remote RPC object
//! (subplan 2-2 S-d, **P5**).
//!
//! A **distinct `IBinder` type** from `proxy::ProxyHandle`. It never
//! goes through the u32 kernel handle / `handle_to_proxy` / cache-pin
//! machinery (AC-2.6) — RPC has its own `RpcAddress` identity space and
//! its own ref-count. Android made `BpBinder` a dual-mode
//! `variant<BinderHandle, RpcHandle>`; because rsbinder's `IBinder` is
//! a trait, a separate type is cleaner (master §4 P5).
//!
//! Subplan 2-2 drove this proxy from a hand-written typed stub.
//! Subplan 2-6.B generalised the generator to emit
//! `as_remote().ok_or(BadType)?` (a [`RemoteProxy`](crate::RemoteProxy)
//! trait object) instead of the kernel-only `as_proxy().unwrap()`, so
//! the **generated** `Bp*` stub now drives this proxy directly — the
//! same single stub also drives the kernel `ProxyHandle`.

use std::any::Any;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{self, OnceLock, RwLock, Weak};

use crate::binder::{DeathRecipient, IBinder, SIBinder, Stability, Transactable, WIBinder};
use crate::binder::{TransactionCode, TransactionFlags};
use crate::error::{Result, StatusCode};
use crate::parcel::Parcel;

use super::address::RpcAddress;
use super::session::RpcSessionInner;

/// A handle to a remote object reachable over an RPC session.
pub struct RpcProxy {
    addr: RpcAddress,
    /// The interface descriptor. The RPC wire transmits only an
    /// address (not a descriptor string), so a proxy resolved from
    /// `read_binder`/`get_root` starts empty and is stamped **once,
    /// in place** by the generated typed stub's `from_binder`
    /// (subplan 2-6.B, via [`stamp_descriptor`](Self::stamp_descriptor)).
    /// In-place — never a replacement proxy — keeps the dedup-cache
    /// identity and the single `DEC_STRONG` intact (AC-2.5 / P5).
    descriptor: OnceLock<String>,
    session: Weak<RpcSessionInner>,
    /// Death-notification state, mirroring the kernel
    /// [`ProxyHandle`](crate::proxy::ProxyHandle) exactly. RPC has no
    /// death *wire* message (AOSP `RpcState::sendObituaries`): an RPC
    /// object dies when its **session connection drops**, so the
    /// session fires every cached proxy's obituary when its serve loop
    /// ends (see [`RpcSessionInner::send_session_obituaries`]). Set
    /// once when the obituary is dispatched (`Acquire`/`Release`
    /// publishes the recipients teardown to lock-free readers).
    obituary_sent: AtomicBool,
    recipients: RwLock<Vec<sync::Weak<dyn DeathRecipient>>>,
}

impl RpcProxy {
    pub(crate) fn new(addr: RpcAddress, session: Weak<RpcSessionInner>) -> Self {
        RpcProxy {
            addr,
            descriptor: OnceLock::new(),
            session,
            obituary_sent: AtomicBool::new(false),
            recipients: RwLock::new(Vec::new()),
        }
    }

    /// Fire `binder_died` on every registered recipient — called by the
    /// owning session when its connection drops (AOSP
    /// `BpBinder::sendObituary`, the RPC branch). Mirrors the kernel
    /// [`ProxyHandle::send_obituary`](crate::proxy::ProxyHandle) state
    /// machine **minus** the kernel-only `BC_CLEAR_DEATH_NOTIFICATION`
    /// / `flush_commands` (death over RPC is connection-drop-driven,
    /// not a wire command). Idempotent: a second call (e.g. a serve
    /// loop ending after a transact already observed the close) sees
    /// `obituary_sent == true` and returns.
    pub(crate) fn send_obituary(&self, who: &WIBinder) {
        let snapshot: Vec<sync::Weak<dyn DeathRecipient>> = {
            // All `obituary_sent` reads/writes that race `link`/`unlink`
            // happen under this write lock (kernel parity: `mLock`).
            let mut recipients = self.recipients.write().expect("recipients lock poisoned");
            if self.obituary_sent.load(Ordering::Relaxed) {
                return;
            }
            let snapshot = std::mem::take(&mut *recipients);
            // `Release` so a lock-free `link_to_death` Acquire-load that
            // observes `true` also sees the drained vector.
            self.obituary_sent.store(true, Ordering::Release);
            snapshot
        };
        // Callbacks outside the lock so a recipient may re-enter
        // `unlink_to_death`/`link_to_death` without self-deadlock
        // (AOSP unlocks before `reportOneDeath`). Panic-isolated so one
        // buggy recipient cannot abort the serve thread or starve the
        // rest.
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

    /// Subplan 2-6.B: stamp the interface descriptor — known only to
    /// the generated typed stub at compile time — onto this
    /// **already-cached** proxy, in place. First write wins and is
    /// idempotent for the same interface (one wire address identifies
    /// one remote object = one interface). Never replaces the proxy:
    /// a replacement would send a second `DEC_STRONG` on drop and
    /// split the per-address dedup cache (AC-2.5 / P5).
    ///
    /// Because the generated `from_binder` stamps *its own*
    /// `$descriptor` here **before** its `binder.descriptor() !=
    /// $descriptor` check, that check is self-referential for a fresh
    /// RPC proxy: unlike the kernel `ProxyHandle` path it does **not**
    /// validate the remote's actual interface. A wrong-interface cast
    /// is not rejected at `from_binder`; it surfaces as a transact-time
    /// `StatusCode` when the server rejects the interface token. This
    /// is inherent to the Android RPC wire (no descriptor transmitted),
    /// not a defect — `first write wins` additionally protects an
    /// in-use proxy from a later differing cast (the second
    /// `from_binder`'s descriptor check then returns `None`).
    pub fn stamp_descriptor(&self, descriptor: &str) {
        let _ = self.descriptor.set(descriptor.to_string());
    }

    /// The stamped descriptor, or `""` if not yet stamped (a proxy
    /// fresh off the wire before its typed stub is built).
    fn descriptor_str(&self) -> &str {
        self.descriptor.get().map(String::as_str).unwrap_or("")
    }

    /// Build an RPC-mode request `Parcel` for `descriptor`, with the
    /// session's object hooks attached and the interface token written.
    /// Hand-written typed stubs (2-2) call this, write their args, then
    /// [`RpcProxy::transact`].
    pub fn build_request(&self, descriptor: &str) -> Result<Parcel> {
        let inner = self.session.upgrade().ok_or(StatusCode::DeadObject)?;
        let mut data = Parcel::new();
        data.attach_rpc_ops(inner.parcel_ops());
        // So `ParcelFileDescriptor::serialize` applies the negotiated
        // FD policy (default `None` ⇒ the 2-2 reject — subplan 2-7).
        data.set_rpc_fd_mode(inner.fd_mode());
        data.set_rpc_record_fd_positions(inner.records_fd_positions());
        super::session::write_rpc_interface_token(&mut data, descriptor)?;
        Ok(data)
    }

    /// Send an outbound transaction to the remote object. Returns the
    /// reply parcel (`None` for oneway).
    pub fn transact(
        &self,
        code: TransactionCode,
        data: &Parcel,
        flags: TransactionFlags,
    ) -> Result<Option<Parcel>> {
        let inner = self.session.upgrade().ok_or(StatusCode::DeadObject)?;
        inner.client_transact(self.addr, code, data, flags)
    }
}

/// Subplan 2-6 (D1): the RPC proxy implements the same generalized
/// [`RemoteProxy`](crate::RemoteProxy) trait as the kernel
/// `ProxyHandle`, so the one generated `Bp*` stub drives either stack
/// (generator emits `as_remote()`, 2-6.B). `prepare_transact` writes
/// the interface token from the descriptor stamped in place by the
/// generated `from_binder` ([`stamp_descriptor`](RpcProxy::stamp_descriptor)).
impl crate::binder::RemoteProxy for RpcProxy {
    fn prepare_transact(&self, write_header: bool) -> Result<Parcel> {
        let inner = self.session.upgrade().ok_or(StatusCode::DeadObject)?;
        let mut data = Parcel::new();
        data.attach_rpc_ops(inner.parcel_ops());
        data.set_rpc_fd_mode(inner.fd_mode());
        data.set_rpc_record_fd_positions(inner.records_fd_positions());
        if write_header {
            super::session::write_rpc_interface_token(&mut data, self.descriptor_str())?;
        }
        Ok(data)
    }

    fn submit_transact(
        &self,
        code: TransactionCode,
        data: &Parcel,
        flags: TransactionFlags,
    ) -> Result<Option<Parcel>> {
        RpcProxy::transact(self, code, data, flags)
    }
}

impl Drop for RpcProxy {
    fn drop(&mut self) {
        // Last `Arc<RpcProxy>` for this address is going away: tell the
        // peer to drop its strong ref (AC-2.5). Best-effort — never
        // panic in Drop, and a dead session simply means the peer is
        // already gone.
        if let Some(inner) = self.session.upgrade() {
            let _ = inner.send_dec_strong(self.addr);
            // Identity-checked: if this proxy's `Arc` already hit 0 and
            // a concurrent `read_binder` re-cached a fresh live proxy
            // for the same address, this stale `Drop` must NOT evict
            // that successor (AC-2.5 / P5 — see `forget_remote_if`).
            inner.forget_remote_if(&self.addr, self as *const RpcProxy as *const ());
        }
    }
}

impl IBinder for RpcProxy {
    /// Register a death recipient. Death over RPC = the **session
    /// connection dropping** (AOSP `RpcState::sendObituaries`): the
    /// recipient fires when the session's serve loop ends. Mirrors the
    /// kernel [`ProxyHandle::link_to_death`](crate::proxy::ProxyHandle)
    /// minus the kernel `requestDeathNotification` IPC (RPC has no
    /// death wire message).
    ///
    /// **Detection requires the session to be served.** AOSP rejects an
    /// RPC `linkToDeath` outright unless `getMaxIncomingThreads() >= 1`;
    /// the rsbinder analogue is that the obituary is delivered by
    /// [`RpcSession::serve_blocking`](super::session::RpcSession) on
    /// connection loss, so a peer that wants death notification must
    /// run a serve loop (it already does for nested callbacks). A
    /// session that is never served still registers the recipient but
    /// will not deliver until something drives the connection — a
    /// documented rsbinder model property, faithful to AOSP's
    /// incoming-thread requirement.
    fn link_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        // Lock first, then check `obituary_sent` — kernel/AOSP ordering
        // (`BpBinder::linkToDeath` checks `mObitsSent` under `mLock`).
        let mut recipients = self.recipients.write().expect("recipients lock poisoned");
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
        let mut recipients = self.recipients.write().expect("recipients lock poisoned");
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
        let inner = self.session.upgrade().ok_or(StatusCode::DeadObject)?;
        let data = Parcel::new();
        inner.client_transact(self.addr, crate::binder::PING_TRANSACTION, &data, 0)?;
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_transactable(&self) -> Option<&dyn Transactable> {
        None
    }

    fn descriptor(&self) -> &str {
        self.descriptor_str()
    }

    fn is_remote(&self) -> bool {
        true
    }

    // RPC ref-count is driven by the wire `DEC_STRONG` (sent from
    // `Drop`), not by these `SIBinder`/`WIBinder` clone/drop hooks —
    // same no-op shape as `ProxyHandle` under the cache-pin model.
    fn inc_strong(&self, _strong: &SIBinder) -> Result<()> {
        Ok(())
    }

    fn attempt_inc_strong(&self) -> bool {
        true
    }

    fn dec_strong(&self, _strong: Option<ManuallyDrop<SIBinder>>) -> Result<()> {
        Ok(())
    }

    fn inc_weak(&self, _weak: &WIBinder) -> Result<()> {
        Ok(())
    }

    fn dec_weak(&self) -> Result<()> {
        Ok(())
    }

    fn stability(&self) -> Stability {
        Stability::default()
    }
}
