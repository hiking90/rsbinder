// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcSession` — single-connection RPC session (subplan 2-2 driver).
//!
//! Ties one [`RpcTransport`] + [`R34Codec`] + per-session [`RpcState`]
//! together and provides:
//! * client outbound transactions ([`RpcSession::get_root`], and
//!   [`super::proxy::RpcProxy::transact`]),
//! * a blocking server serve loop ([`RpcSession::serve_blocking`]),
//! * the [`RpcParcelOps`] bridge that lets the `SIBinder`
//!   (de)serializers marshal binders as `RpcAddress`.
//!
//! Per **P6** all state is owned here (no global). The multi-connection
//! / threaded / negotiated session is subplan 2-3; this is the minimal
//! single-connection request/reply driver 2-2 needs for its e2e.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::binder::{SIBinder, FLAG_ONEWAY, INTERFACE_HEADER};
use crate::error::{Result, StatusCode};
use crate::parcel::{Parcel, RpcParcelOps};

use super::address::{RpcAddress, SpecialTransaction, RPC_ADDR_LEN};
use super::proxy::RpcProxy;
use super::state::RpcState;
use super::transport::RpcTransport;
use super::wire::{R34Codec, WireCodec, WireMessage, WireReply, WireTransaction};
use super::RpcError;

/// `strict_mode_policy() == 0 | STRICT_MODE_PENALTY_GATHER`, written
/// without touching `thread_state` (RPC must never couple to the
/// kernel thread state — master §4.1.1).
const STRICT_MODE_PENALTY_GATHER: i32 = 1 << 31;
/// `thread_state::UNSET_WORK_SOURCE` (mirrored as a constant).
const UNSET_WORK_SOURCE: i32 = -1;

/// Write the AIDL interface token in the same byte layout as
/// `Parcel::write_interface_token`/`thread_state::check_interface`, but
/// with constants instead of `thread_state` reads (RPC decoupling).
pub(crate) fn write_rpc_interface_token(p: &mut Parcel, descriptor: &str) -> Result<()> {
    p.write(&STRICT_MODE_PENALTY_GATHER)?;
    p.write(&UNSET_WORK_SOURCE)?;
    if crate::sdk_at_least(30) {
        p.write(&INTERFACE_HEADER)?;
    }
    p.write(&descriptor)?;
    Ok(())
}

/// Consume + validate the interface token the RPC server adapter must
/// strip before calling `IBinder::rpc_transact` (the RPC equivalent of
/// what `check_interface` did, minus the `THREAD_STATE` mutation).
fn consume_rpc_interface_token(reader: &mut Parcel, expected: &str) -> Result<()> {
    let _strict: i32 = reader.read()?;
    let _work_source: i32 = reader.read()?;
    if crate::sdk_at_least(30) {
        let header: u32 = reader.read()?;
        if header != INTERFACE_HEADER {
            return Err(StatusCode::BadType);
        }
    }
    let got: String = reader.read()?;
    if got != expected {
        log::error!("RPC interface token mismatch: expected '{expected}', got '{got}'");
        return Err(StatusCode::BadType);
    }
    Ok(())
}

fn write_addr(p: &mut Parcel, addr: &RpcAddress) {
    // 32 bytes, already 4-aligned (no padding) — matches the r34
    // Parcel RPC binder encoding (i32 present flag handled by caller).
    p.write_aligned_data(addr.as_wire_bytes().as_slice());
}

fn read_addr(p: &mut Parcel) -> Result<RpcAddress> {
    let slice = p.read_aligned_data(RPC_ADDR_LEN)?;
    let mut bytes = [0u8; RPC_ADDR_LEN];
    bytes.copy_from_slice(slice);
    Ok(RpcAddress::from_wire_bytes(bytes))
}

/// Shared session state. Held behind `Arc`; never global (P6).
pub struct RpcSessionInner {
    transport: Box<dyn RpcTransport>,
    codec: R34Codec,
    state: Mutex<RpcState>,
    async_counter: AtomicU64,
    root: Mutex<Option<SIBinder>>,
    self_weak: Mutex<Weak<RpcSessionInner>>,
}

/// The [`RpcParcelOps`] implementation bound to one session.
struct SessionParcelOps(Weak<RpcSessionInner>);

impl RpcParcelOps for SessionParcelOps {
    fn write_binder(&self, binder: Option<&SIBinder>, parcel: &mut Parcel) -> Result<()> {
        let inner = self.0.upgrade().ok_or(StatusCode::DeadObject)?;
        inner.write_binder(binder, parcel)
    }
    fn read_binder(&self, parcel: &mut Parcel) -> Result<Option<SIBinder>> {
        let inner = self.0.upgrade().ok_or(StatusCode::DeadObject)?;
        inner.read_binder(parcel)
    }
}

impl RpcSessionInner {
    pub(crate) fn parcel_ops(&self) -> Arc<dyn RpcParcelOps> {
        Arc::new(SessionParcelOps(
            self.self_weak.lock().expect("self_weak").clone(),
        ))
    }

    fn self_weak(&self) -> Weak<RpcSessionInner> {
        self.self_weak.lock().expect("self_weak").clone()
    }

    /// android `flattenBinder` (RPC branch): `i32` present flag, then a
    /// 32B `RpcWireAddress` for non-null.
    fn write_binder(&self, binder: Option<&SIBinder>, parcel: &mut Parcel) -> Result<()> {
        match binder {
            None => parcel.write(&0i32),
            Some(b) => {
                let addr = if let Some(rp) = (**b).as_any().downcast_ref::<RpcProxy>() {
                    // A remote object travelling back to its origin —
                    // reuse its existing address (no new local node).
                    rp.address()
                } else {
                    // A local object leaving this process.
                    self.state
                        .lock()
                        .expect("rpc state poisoned")
                        .on_binder_leaving(b)
                };
                parcel.write(&1i32)?;
                write_addr(parcel, &addr);
                Ok(())
            }
        }
    }

    /// android `unflattenBinder` (RPC branch).
    fn read_binder(&self, parcel: &mut Parcel) -> Result<Option<SIBinder>> {
        let present: i32 = parcel.read()?;
        if present == 0 {
            return Ok(None);
        }
        let addr = read_addr(parcel)?;
        // An address that is one of *our* local nodes means the object
        // is coming home — return the original local binder.
        if let Some(local) = self
            .state
            .lock()
            .expect("rpc state poisoned")
            .lookup_local(&addr)
        {
            return Ok(Some(local));
        }
        let weak = self.self_weak();
        let sib = self
            .state
            .lock()
            .expect("rpc state poisoned")
            .remote_proxy(addr, || {
                SIBinder::new(Arc::new(RpcProxy::new(addr, String::new(), weak)))
                    .expect("SIBinder::new(RpcProxy)")
            });
        Ok(Some(sib))
    }

    /// Client outbound transaction. Returns the reply parcel (or `None`
    /// for oneway). Applies any interleaved `DEC_STRONG` and loops to
    /// the matching `REPLY`.
    pub(crate) fn client_transact(
        &self,
        addr: RpcAddress,
        code: u32,
        data: &Parcel,
        flags: u32,
    ) -> Result<Option<Parcel>> {
        let oneway = (flags & FLAG_ONEWAY) != 0;
        let async_number = if oneway {
            self.async_counter.fetch_add(1, Ordering::SeqCst)
        } else {
            0
        };
        let txn = WireTransaction {
            address: addr,
            code,
            flags,
            async_number,
            data: data.rpc_data_bytes().to_vec(),
        };
        let frame = self.codec.encode_transact(&txn);
        self.transport.send_frame(&frame)?;
        if oneway {
            return Ok(None);
        }
        loop {
            let frame = self.transport.recv_frame()?;
            match self.codec.decode_message(&frame)? {
                WireMessage::Reply(WireReply { status, data }) => {
                    if status != 0 {
                        return Err(StatusCode::from(status));
                    }
                    let mut reply = Parcel::from_vec(data);
                    reply.attach_rpc_ops(self.parcel_ops());
                    reply.set_data_position(0);
                    return Ok(Some(reply));
                }
                WireMessage::DecStrong(a) => {
                    self.state
                        .lock()
                        .expect("rpc state poisoned")
                        .dec_strong_local(&a);
                }
                WireMessage::Transact(_) => {
                    // Nested re-entrant calls are subplan 2-3.
                    return Err(StatusCode::InvalidOperation);
                }
            }
        }
    }

    pub(crate) fn send_dec_strong(&self, addr: RpcAddress) -> Result<()> {
        let frame = self.codec.encode_dec_strong(&addr);
        self.transport.send_frame(&frame)?;
        Ok(())
    }

    pub(crate) fn forget_remote(&self, addr: &RpcAddress) {
        self.state
            .lock()
            .expect("rpc state poisoned")
            .forget_remote(addr);
    }

    /// Send a `REPLY` (status + parcel bytes).
    fn send_reply(&self, status: i32, data: &[u8]) -> Result<()> {
        let frame = self.codec.encode_reply(&WireReply {
            status,
            data: data.to_vec(),
        });
        Ok(self.transport.send_frame(&frame)?)
    }

    /// Handle one inbound message. `Ok(false)` ⇒ peer closed (stop).
    fn serve_once(&self) -> Result<bool> {
        let frame = match self.transport.recv_frame() {
            Ok(f) => f,
            Err(RpcError::PeerClosed) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        match self.codec.decode_message(&frame)? {
            WireMessage::Transact(t) => {
                let oneway = (t.flags & FLAG_ONEWAY) != 0;
                if t.address.is_zero() {
                    self.serve_special(t.code, oneway)?;
                    return Ok(true);
                }
                let target = self
                    .state
                    .lock()
                    .expect("rpc state poisoned")
                    .lookup_local(&t.address);
                let Some(target) = target else {
                    if !oneway {
                        self.send_reply(StatusCode::DeadObject.into(), &[])?;
                    }
                    return Ok(true);
                };

                let mut reader = Parcel::from_vec(t.data);
                reader.attach_rpc_ops(self.parcel_ops());
                reader.set_data_position(0);
                let mut reply = Parcel::new();
                reply.attach_rpc_ops(self.parcel_ops());

                let result = consume_rpc_interface_token(&mut reader, target.descriptor())
                    .and_then(|()| target.rpc_transact(t.code, &mut reader, &mut reply));

                if oneway {
                    if let Err(e) = result {
                        log::error!("oneway RPC transaction failed (dropped): {e:?}");
                    }
                    return Ok(true);
                }
                match result {
                    Ok(()) => self.send_reply(0, reply.rpc_data_bytes())?,
                    Err(e) => self.send_reply(e.into(), &[])?,
                }
                Ok(true)
            }
            WireMessage::DecStrong(a) => {
                self.state
                    .lock()
                    .expect("rpc state poisoned")
                    .dec_strong_local(&a);
                Ok(true)
            }
            WireMessage::Reply(_) => {
                log::warn!("RPC server received an unexpected REPLY; ignoring");
                Ok(true)
            }
        }
    }

    /// Special zero-address transactions (android `RpcState`
    /// `GET_ROOT`/`GET_MAX_THREADS`/`GET_SESSION_ID`). Full multi-conn
    /// negotiation is subplan 2-3; 2-2 needs `GET_ROOT`.
    fn serve_special(&self, code: u32, oneway: bool) -> Result<()> {
        if oneway {
            // Special transactions are never oneway.
            return Ok(());
        }
        match SpecialTransaction::from_code(code) {
            Some(SpecialTransaction::GetRoot) => {
                let root = self.root.lock().expect("root poisoned").clone();
                let mut reply = Parcel::new();
                reply.attach_rpc_ops(self.parcel_ops());
                // SIBinder::serialize → RPC branch → write_binder.
                match &root {
                    Some(b) => reply.write(b)?,
                    None => reply.write(&0i32)?,
                }
                self.send_reply(0, reply.rpc_data_bytes())
            }
            Some(SpecialTransaction::GetMaxThreads) => {
                let mut reply = Parcel::new();
                reply.write(&1i32)?; // single-connection in 2-2
                self.send_reply(0, reply.rpc_data_bytes())
            }
            Some(SpecialTransaction::GetSessionId) => {
                let mut reply = Parcel::new();
                reply.write(&1i32)?;
                self.send_reply(0, reply.rpc_data_bytes())
            }
            None => self.send_reply(StatusCode::UnknownTransaction.into(), &[]),
        }
    }
}

/// A single-connection RPC session (client and/or server role).
#[derive(Clone)]
pub struct RpcSession {
    inner: Arc<RpcSessionInner>,
}

impl RpcSession {
    /// Wrap a connected transport in a session.
    pub fn new(transport: Box<dyn RpcTransport>) -> RpcSession {
        let inner = Arc::new(RpcSessionInner {
            transport,
            codec: R34Codec,
            state: Mutex::new(RpcState::new()),
            async_counter: AtomicU64::new(0),
            root: Mutex::new(None),
            self_weak: Mutex::new(Weak::new()),
        });
        *inner.self_weak.lock().expect("self_weak") = Arc::downgrade(&inner);
        RpcSession { inner }
    }

    /// Publish the server's root object (returned by `get_root`).
    pub fn set_root(&self, binder: SIBinder) {
        *self.inner.root.lock().expect("root poisoned") = Some(binder);
    }

    /// Client: fetch the peer's root object as an [`RpcProxy`]-backed
    /// `SIBinder`.
    pub fn get_root(&self) -> Result<SIBinder> {
        let data = Parcel::new();
        let reply = self
            .inner
            .client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetRoot.code(),
                &data,
                0,
            )?
            .ok_or(StatusCode::UnexpectedNull)?;
        let mut reply = reply;
        reply
            .read::<SIBinder>()
            .map_err(|_| StatusCode::UnexpectedNull)
    }

    /// Server: process inbound messages until the peer closes.
    pub fn serve_blocking(&self) -> Result<()> {
        while self.inner.serve_once()? {}
        Ok(())
    }

    /// Test/diagnostic: live local-node count (AC-2.5 leak check).
    pub fn local_node_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .expect("rpc state poisoned")
            .local_node_count()
    }
}
