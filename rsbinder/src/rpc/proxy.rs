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
//! In subplan 2-2 the *typed* client stub is hand-written and drives
//! this proxy directly (the generator emits `as_proxy().unwrap()` which
//! hard-downcasts to `ProxyHandle` — generalising that is subplan 2-6).

use std::any::Any;
use std::mem::ManuallyDrop;
use std::sync::{self, Weak};

use crate::binder::{DeathRecipient, IBinder, SIBinder, Stability, Transactable, WIBinder};
use crate::binder::{TransactionCode, TransactionFlags};
use crate::error::{Result, StatusCode};
use crate::parcel::Parcel;

use super::address::RpcAddress;
use super::session::RpcSessionInner;

/// A handle to a remote object reachable over an RPC session.
pub struct RpcProxy {
    addr: RpcAddress,
    descriptor: String,
    session: Weak<RpcSessionInner>,
}

impl RpcProxy {
    pub(crate) fn new(
        addr: RpcAddress,
        descriptor: String,
        session: Weak<RpcSessionInner>,
    ) -> Self {
        RpcProxy {
            addr,
            descriptor,
            session,
        }
    }

    /// The remote object's RPC address.
    pub fn address(&self) -> RpcAddress {
        self.addr
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
/// `ProxyHandle`, so one generated `Bp*` stub can drive either stack
/// once the generator emits `as_remote()` (2-6.B). `prepare_transact`
/// writes the interface token using this proxy's descriptor (stamped
/// by the typed-stub constructor in 2-6.B).
impl crate::binder::RemoteProxy for RpcProxy {
    fn prepare_transact(&self, write_header: bool) -> Result<Parcel> {
        let inner = self.session.upgrade().ok_or(StatusCode::DeadObject)?;
        let mut data = Parcel::new();
        data.attach_rpc_ops(inner.parcel_ops());
        data.set_rpc_fd_mode(inner.fd_mode());
        if write_header {
            super::session::write_rpc_interface_token(&mut data, &self.descriptor)?;
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
            inner.forget_remote(&self.addr);
        }
    }
}

impl IBinder for RpcProxy {
    fn link_to_death(&self, _r: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        // Death notification over RPC is a later subplan; reject
        // explicitly rather than silently succeed.
        Err(StatusCode::InvalidOperation)
    }

    fn unlink_to_death(&self, _r: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        Err(StatusCode::InvalidOperation)
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
        &self.descriptor
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
