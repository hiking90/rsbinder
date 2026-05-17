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

use std::cell::RefCell;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use super::fd_mode::FileDescriptorTransportMode;
use crate::binder::{
    SIBinder, FLAG_ONEWAY, INTERFACE_HEADER, INTERFACE_TRANSACTION, PING_TRANSACTION,
};
use crate::error::{Result, StatusCode};
use crate::parcel::{Parcel, RpcParcelOps};

use super::address::{AddressSpace, RpcAddress, SpecialTransaction, RPC_ADDR_LEN};
use super::proxy::RpcProxy;
use super::state::RpcState;
use super::transport::RpcTransport;
use super::wire::{R34Codec, WireCodec, WireMessage, WireReply, WireTransaction};
use super::wire_android13::{
    client_connect, read_aosp_message, server_accept, write_aosp_message, Android13PlusCodec,
    RawTransportIo, A13_ADDR_LEN, FD_MODE_NONE,
};
use super::{RpcError, RpcResult};

/// Which RPC wire profile a session speaks.
///
/// **G4(a) (subplan 2-5b).** The default [`WireProfile::R34`] arm is the
/// android-12 r34 path *verbatim* — rsbinder's `u32` length-prefix
/// framing ([`RpcTransport::send_frame`]/`recv_frame`) + [`R34Codec`],
/// no connection handshake. It is byte-unchanged; the green R34 suite
/// (`rpc_e2e`/`rpc_server`/`rpc_fd`/`rpc_generated_stub`) is its
/// no-regression gate.
///
/// The opt-in [`WireProfile::Android13Plus`] arm speaks the real
/// android-13+ versioned wire: **AOSP-faithful framing** (no length
/// prefix — the genuine android peer writes `RpcWireHeader` + body
/// directly) over the transport's raw byte channel
/// ([`RpcTransport::send_raw`]/`recv_raw` via [`RawTransportIo`]) +
/// the version-keyed [`Android13PlusCodec`] finalized by the connection
/// handshake (`client_connect`/`server_accept`). The reusable framing /
/// handshake / codec primitives are proven hermetically in
/// `wire_android13` (G4 Layer-1); this enum is where they become a live
/// `RpcSession`/`RpcServer` dispatch path, reusing the existing
/// per-session [`RpcState`], `client_transact`/`serve_blocking` and
/// re-entrancy machinery unchanged.
enum WireProfile {
    /// android-12 r34 — length-prefix framing + `R34Codec` (default,
    /// byte-unchanged).
    R34(R34Codec),
    /// android-13+ — AOSP-faithful framing + version-keyed codec
    /// (`PROTOCOL_V0` = android-13, `PROTOCOL_V1` = android-14/15),
    /// negotiated by the connection handshake.
    Android13Plus(Android13PlusCodec),
}

impl WireProfile {
    /// The wire codec for this profile (`R34Codec` is zero-sized, the
    /// dynamic call is trivial and byte-identical to the static call —
    /// the green R34 suite is the proof).
    fn codec(&self) -> &dyn WireCodec {
        match self {
            WireProfile::R34(c) => c,
            WireProfile::Android13Plus(c) => c,
        }
    }

    /// `true` for the android-13+ profile, which frames AOSP-faithfully
    /// (no rsbinder `u32` length prefix) over the transport's raw byte
    /// channel instead of [`RpcTransport::send_frame`]/`recv_frame`.
    fn aosp_framing(&self) -> bool {
        matches!(self, WireProfile::Android13Plus(_))
    }
}

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

/// A per-session opaque 32-byte id (AOSP `kSessionIdBytes`). The
/// content is opaque to the protocol — AOSP fills it from a CSPRNG and
/// only ever *compares* it; rsbinder's single-connection model never
/// re-presents it, so a process-/time-/address-mixed value is
/// sufficient and, crucially, **global-free** (P6 — no `static`
/// counter, so the `rpc_stack_has_no_globals` gate stays clean).
fn gen_rpc_session_id() -> [u8; 32] {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix in this thread's id (a fresh server worker per accepted
    // connection) so two sessions created within the same clock tick
    // still differ.
    let tid: u64 = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::thread::current().id().hash(&mut h);
        h.finish()
    };
    // Fold the full 128-bit nanos (both halves) into the mix word so
    // it depends on the high bits too — the id is opaque (AOSP only
    // `memcmp`s it), this just avoids a silent low-64 truncation.
    let nanos_mix = (nanos ^ (nanos >> 64)) as u64;
    let mut id = [0u8; 32];
    id[0..16].copy_from_slice(&nanos.to_le_bytes());
    id[16..24].copy_from_slice(&tid.to_le_bytes());
    id[24..32].copy_from_slice(&(nanos_mix ^ tid ^ 0x9E37_79B9_7F4A_7C15).to_le_bytes());
    id
}

/// RAII clear for the `client_transact` reply read-deadline (AC-3.8).
///
/// `set_read_timeout(Some(d))` sets a sticky `SO_RCVTIMEO` on the
/// shared connection. This guard clears it on **every** exit from the
/// reply wait — normal return, `?`-propagation, or panic — so the
/// deadline can never leak onto the next `client_transact`, a nested
/// inbound dispatch, or a subsequent server-side `recv` on the same
/// connection.
struct ReplyDeadlineGuard<'a> {
    transport: &'a dyn RpcTransport,
    armed: bool,
}

impl<'a> ReplyDeadlineGuard<'a> {
    fn arm(transport: &'a dyn RpcTransport, deadline: Option<Duration>) -> RpcResult<Self> {
        let armed = deadline.is_some();
        if let Some(d) = deadline {
            transport.set_read_timeout(Some(d))?;
        }
        Ok(Self { transport, armed })
    }
}

impl Drop for ReplyDeadlineGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort: a failure to clear cannot be surfaced from
            // Drop, and the next caller re-arms/clears explicitly anyway.
            let _ = self.transport.set_read_timeout(None);
        }
    }
}

/// RAII pair for the *nested-dispatch* deadline window inside
/// `client_transact` (T1-2 / Major-1).
///
/// The reply deadline (AC-3.8) bounds **only the outermost reply
/// wait**. A nested inbound call dispatched while we wait (a server
/// callback — AC-3.6) is legitimate, potentially long-running forward
/// progress, not a stall: bounding it would break valid re-entrancy,
/// and time-bounding the nested *reply write* could leave a half-frame
/// on the wire. So the deadline is lifted for the nested dispatch and
/// restored for the continued wait — but **symmetrically via Drop**, so
/// an early `?`/panic out of `dispatch_transact` can never leave the
/// sticky `SO_RCVTIMEO` desynchronized for the rest of the reply loop
/// (the pre-T1-2 manual clear/re-arm pair could).
struct NestedDeadlineGuard<'a> {
    transport: &'a dyn RpcTransport,
    restore: Option<Duration>,
}

impl<'a> NestedDeadlineGuard<'a> {
    fn lift(transport: &'a dyn RpcTransport, deadline: Option<Duration>) -> RpcResult<Self> {
        if deadline.is_some() {
            transport.set_read_timeout(None)?;
        }
        Ok(Self {
            transport,
            restore: deadline,
        })
    }
}

impl Drop for NestedDeadlineGuard<'_> {
    fn drop(&mut self) {
        if let Some(d) = self.restore {
            // Best-effort (Drop): the continued reply loop's next
            // `recv` would itself surface a transport error anyway.
            let _ = self.transport.set_read_timeout(Some(d));
        }
    }
}

thread_local! {
    /// Addresses of the `RpcSessionInner`s whose connection *this
    /// thread* is currently driving (outermost `client_transact` /
    /// `serve_once`). It lets a same-thread **nested** call (a server
    /// handler calling back while a transaction is in flight — AC-3.6)
    /// bypass the per-connection lock instead of self-deadlocking on
    /// it.
    ///
    /// This is a per-thread *recursion marker*, **not** session or
    /// protocol state (P6): it holds no node / address / ref-count
    /// data — those stay per-session in [`RpcState`]. It mirrors
    /// kernel binder's thread-local `IPCThreadState`. Documented P6
    /// exception in the `rpc_stack_has_no_globals` gate.
    static DRIVING: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// RAII guard for the per-session connection-driver lock with
/// same-thread reentrancy bypass (see [`RpcSessionInner::enter_connection`]).
struct ConnGuard<'a> {
    _lock: Option<std::sync::MutexGuard<'a, ()>>,
    key: usize,
    pushed: bool,
}

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        if self.pushed {
            DRIVING.with(|d| {
                let mut v = d.borrow_mut();
                if let Some(pos) = v.iter().rposition(|&k| k == self.key) {
                    v.remove(pos);
                }
            });
        }
        // `_lock` (if held) releases here, after the marker is popped.
    }
}

/// Shared session state. Held behind `Arc`; never global (P6).
pub struct RpcSessionInner {
    transport: Box<dyn RpcTransport>,
    /// Wire profile: R34 (default, byte-unchanged) or the opt-in
    /// android-13+ versioned wire (G4(a)). Fixed for the session's
    /// lifetime — the android-13+ codec version is finalized by the
    /// connection handshake *before* the session is constructed, so no
    /// interior mutability is needed.
    profile: WireProfile,
    /// Serializes connection *driving* across threads. One
    /// `RpcSession` connection is driven by a single logical role;
    /// `RpcSession` is `Clone` + `Send`/`Sync`, so a generated `Bp*`
    /// proxy shared across threads would otherwise let two
    /// `client_transact`s interleave the framed stream or
    /// cross-deliver replies (the r34 wire has no request/response
    /// correlation id — Major-2 / AC-3.2). Concurrent calls are
    /// serialized here; parallelism comes from *multiple connections*
    /// (the documented model). Same-thread nested calls (AC-3.6)
    /// bypass it via the `DRIVING` marker.
    conn_lock: Mutex<()>,
    state: Mutex<RpcState>,
    async_counter: AtomicU64,
    root: Mutex<Option<SIBinder>>,
    self_weak: Mutex<Weak<RpcSessionInner>>,
    /// Max-threads value advertised to the peer on `GET_MAX_THREADS`
    /// (server side) — subplan 2-3 negotiation.
    max_threads: AtomicU32,
    /// `min(local, remote)` after the client handshake (0 until done).
    negotiated: AtomicU32,
    /// Optional reply/handshake wait deadline (AC-3.8).
    timeout: Mutex<Option<Duration>>,
    /// Negotiated FD-over-RPC mode (subplan 2-7). Default `None` ⇒ the
    /// 2-2 reject path, and `send/recv` use the unchanged framed calls
    /// (AC-7.1 bit-identical).
    fd_mode: Mutex<crate::rpc::FileDescriptorTransportMode>,
    /// Server role: does this endpoint advertise `Unix` FD support on
    /// `GET_FD_MODE`. Default false.
    fd_unix_supported: std::sync::atomic::AtomicBool,
    /// Opaque 32-byte session id returned by the `GET_SESSION_ID`
    /// special transact. AOSP `RpcServer` assigns a random
    /// `kSessionIdBytes == 32` id; the libbinder client reads it with
    /// `Parcel::readByteVector` and would `BAD_VALUE` on any other size
    /// (G4(b): real-peer-validated). Per-session, never global (P6) —
    /// generated global-free in [`RpcSession::with_profile`].
    rpc_session_id: [u8; 32],
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
    /// Acquire the per-connection driver lock — unless *this thread* is
    /// already driving *this* session (a nested in-flight call:
    /// AC-3.6), in which case re-locking would self-deadlock so we
    /// bypass and let the outer frame's lock cover the nested traffic
    /// (same thread ⇒ still strictly FIFO on the connection). Across
    /// *different* threads the lock fully serializes connection driving
    /// (Major-2 / AC-3.2). The guard releases the lock and pops the
    /// recursion marker on every exit (return / `?` / panic).
    fn enter_connection(&self) -> ConnGuard<'_> {
        let key = self as *const RpcSessionInner as usize;
        let reentrant = DRIVING.with(|d| d.borrow().contains(&key));
        if reentrant {
            ConnGuard {
                _lock: None,
                key,
                pushed: false,
            }
        } else {
            let lock = self.conn_lock.lock().expect("conn_lock poisoned");
            DRIVING.with(|d| d.borrow_mut().push(key));
            ConnGuard {
                _lock: Some(lock),
                key,
                pushed: true,
            }
        }
    }

    pub(crate) fn parcel_ops(&self) -> Arc<dyn RpcParcelOps> {
        Arc::new(SessionParcelOps(
            self.self_weak.lock().expect("self_weak").clone(),
        ))
    }

    pub(crate) fn fd_mode(&self) -> FileDescriptorTransportMode {
        *self.fd_mode.lock().expect("fd_mode poisoned")
    }

    /// Send one wire frame. Only a `Unix`-mode connection routes fds
    /// via `SCM_RIGHTS`; the default (`None`) uses the unchanged
    /// framed send and never carries fds (AC-7.1 bit-identical).
    fn send_msg(&self, frame: &[u8], fds: &[OwnedFd]) -> RpcResult<()> {
        if self.profile.aosp_framing() {
            // android-13+ (G4(a)): the real AOSP wire has **no** length
            // prefix — write `frame` (= the codec's
            // `[RpcWireHeader|body]`) raw over the transport's byte
            // channel, exactly what a genuine android-13/14/15 peer
            // reads. FD-over-RPC on android-13+ is negotiated in the
            // connection header (out of G4(a) no-FD scope), so `fd_mode`
            // stays `None` and no fds are ever produced here.
            debug_assert!(
                fds.is_empty(),
                "android-13+ G4(a) is no-FD scope (header-negotiated FD mode is G4(b))"
            );
            let _ = fds; // release: `debug_assert!` is compiled out, so `fds` is otherwise unused
            let mut io = RawTransportIo(self.transport.as_ref());
            return write_aosp_message(&mut io, frame);
        }
        if self.fd_mode() == FileDescriptorTransportMode::Unix {
            let borrowed: Vec<_> = fds.iter().map(|f| f.as_fd()).collect();
            self.transport.send_frame_with_fds(frame, &borrowed)
        } else {
            self.transport.send_frame(frame)
        }
    }

    /// Receive one wire frame (+ any `SCM_RIGHTS` fds in `Unix` mode).
    /// A connection never mixes the `Read` and `recvmsg` paths because
    /// the mode is fixed by negotiation before any RPC traffic.
    fn recv_msg(&self) -> RpcResult<(Vec<u8>, Vec<OwnedFd>)> {
        if self.profile.aosp_framing() {
            // android-13+ (G4(a)): read `RpcWireHeader` then exactly
            // `bodySize` bytes (capped vs `MAX_FRAME_LEN` — V4); a clean
            // EOF before any byte surfaces as `PeerClosed` so the
            // `serve_blocking` loop terminates exactly like the R34
            // path. No out-of-band fds in no-FD scope.
            let mut io = RawTransportIo(self.transport.as_ref());
            let frame = read_aosp_message(&mut io)?;
            return Ok((frame, Vec::new()));
        }
        if self.fd_mode() == FileDescriptorTransportMode::Unix {
            self.transport.recv_frame_with_fds()
        } else {
            Ok((self.transport.recv_frame()?, Vec::new()))
        }
    }

    fn self_weak(&self) -> Weak<RpcSessionInner> {
        self.self_weak.lock().expect("self_weak").clone()
    }

    /// Profile-aware in-parcel binder address (the `flattenBinder` /
    /// `unflattenBinder` RPC branch payload after the `i32` present
    /// flag):
    /// * **R34** — the 32-byte opaque `RpcAddress` (verbatim,
    ///   byte-unchanged);
    /// * **android-13+** — the 8-byte `RpcWireAddress`
    ///   (`{u32 options; u32 address}`), i.e. AOSP `Parcel::flattenBinder`'s
    ///   `writeUint64(address)`. r34's 32-byte form here was rejected by
    ///   a real libbinder peer (`"unrecognized address … we should own
    ///   the creation of"`) — G4(b)-pinned, the `kCurrentRepr`
    ///   Parcel-body conformance.
    fn wire_write_binder_addr(&self, p: &mut Parcel, addr: &RpcAddress) {
        match &self.profile {
            WireProfile::R34(_) => write_addr(p, addr),
            WireProfile::Android13Plus(_) => {
                p.write_aligned_data(&Android13PlusCodec::encode_addr(addr));
            }
        }
    }

    fn wire_read_binder_addr(&self, p: &mut Parcel) -> Result<RpcAddress> {
        match &self.profile {
            WireProfile::R34(_) => read_addr(p),
            WireProfile::Android13Plus(_) => {
                let slice = p.read_aligned_data(A13_ADDR_LEN)?;
                Android13PlusCodec::decode_addr(slice, 0).map_err(StatusCode::from)
            }
        }
    }

    /// android `flattenBinder` (RPC branch): `i32` present flag, then
    /// the profile's `RpcWireAddress` ([`wire_write_binder_addr`]) for
    /// non-null.
    ///
    /// [`wire_write_binder_addr`]: RpcSessionInner::wire_write_binder_addr
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
                self.wire_write_binder_addr(parcel, &addr);
                if matches!(self.profile, WireProfile::Android13Plus(_)) {
                    // AOSP `Parcel::finishFlattenBinder` →
                    // `writeInt32(Stability::getRepr(binder))`. r34's
                    // rsbinder↔rsbinder path is symmetric and omits it;
                    // the real libbinder peer's `finishUnflattenBinder`
                    // *requires* it (else a short read ⇒ null root —
                    // G4(b)-pinned). We send the binder's *actual*
                    // declared stability (`getRepr`-faithful), not a
                    // hardcoded 0: rsbinder's default is
                    // `Stability::System` (= `0b001100`; +`0x0c000000`
                    // on android sdk 31/32), which libbinder accepts as
                    // a declared level for an RPC binder.
                    let rep: i32 = b.stability().into();
                    parcel.write(&rep)?;
                }
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
        let addr = self.wire_read_binder_addr(parcel)?;
        if matches!(self.profile, WireProfile::Android13Plus(_)) {
            // Symmetric to `write_binder`: consume AOSP
            // `finishUnflattenBinder`'s trailing stability `int32`.
            let _stability: i32 = parcel.read()?;
        }
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
                SIBinder::new(Arc::new(RpcProxy::new(addr, weak))).expect("SIBinder::new(RpcProxy)")
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
        // Serialize this outbound call against any concurrent
        // client_transact/serve on a shared session; a same-thread
        // nested call (server callback) bypasses it (AC-3.2 / AC-3.6).
        let _conn = self.enter_connection();
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
        let frame = self.profile.codec().encode_transact(&txn);
        // Out-of-band fds collected while serializing the request
        // (empty unless `Unix` fd-mode — subplan 2-7).
        self.send_msg(&frame, data.rpc_out_fds())?;
        if oneway {
            return Ok(None);
        }
        // Apply the configured reply deadline (AC-3.8) for the duration
        // of the reply wait only. `ReplyDeadlineGuard` clears the sticky
        // `SO_RCVTIMEO` on every exit (return / `?` / panic) so it never
        // leaks onto the next call or a later recv on this connection.
        let deadline = *self.timeout.lock().expect("timeout poisoned");
        let _deadline_guard = ReplyDeadlineGuard::arm(self.transport.as_ref(), deadline)?;
        loop {
            let (frame, in_fds) = self.recv_msg()?;
            match self.profile.codec().decode_message(&frame)? {
                WireMessage::Reply(WireReply { status, data }) => {
                    if status != 0 {
                        return Err(StatusCode::from(status));
                    }
                    let mut reply = Parcel::from_vec(data);
                    reply.attach_rpc_ops(self.parcel_ops());
                    reply.set_rpc_fd_mode(self.fd_mode());
                    reply.rpc_set_in_fds(in_fds);
                    reply.set_data_position(0);
                    return Ok(Some(reply));
                }
                WireMessage::DecStrong(a) => {
                    self.state
                        .lock()
                        .expect("rpc state poisoned")
                        .dec_strong_local(&a);
                }
                WireMessage::Transact(t) => {
                    // Nested / re-entrant call: the peer is calling
                    // back into one of *our* objects while we wait for
                    // our own reply. Dispatch it inline on this call
                    // stack over the same connection (single thread per
                    // connection ⇒ correct FIFO nesting, no deadlock —
                    // AC-3.6). The reply deadline is lifted for the
                    // (unbounded) nested dispatch and restored for the
                    // continued wait *symmetrically via Drop* — a `?` /
                    // panic out of `dispatch_transact` can no longer
                    // leave the timeout desynchronized (T1-2).
                    let _restore = NestedDeadlineGuard::lift(self.transport.as_ref(), deadline)?;
                    self.dispatch_transact(t, in_fds)?;
                }
            }
        }
    }

    pub(crate) fn send_dec_strong(&self, addr: RpcAddress) -> Result<()> {
        // `RpcProxy::drop` can fire this from any thread; serialize the
        // frame against an in-flight transaction (bypassed when the
        // dropping thread is itself the connection driver).
        let _conn = self.enter_connection();
        let frame = self.profile.codec().encode_dec_strong(&addr);
        self.send_msg(&frame, &[])?;
        Ok(())
    }

    pub(crate) fn forget_remote_if(&self, addr: &RpcAddress, who: *const ()) {
        self.state
            .lock()
            .expect("rpc state poisoned")
            .forget_remote_if(addr, who);
    }

    /// Send a `REPLY` (status + parcel bytes + any out-of-band fds).
    fn send_reply(&self, status: i32, data: &[u8], fds: &[OwnedFd]) -> Result<()> {
        let frame = self.profile.codec().encode_reply(&WireReply {
            status,
            data: data.to_vec(),
        });
        Ok(self.send_msg(&frame, fds)?)
    }

    /// Dispatch one inbound `TRANSACT` (server role, or a nested
    /// callback while a client call is in flight) and send its reply.
    /// Shared by [`RpcSessionInner::serve_once`] and the nested-call
    /// arm of [`RpcSessionInner::client_transact`].
    fn dispatch_transact(&self, t: WireTransaction, in_fds: Vec<OwnedFd>) -> Result<()> {
        let oneway = (t.flags & FLAG_ONEWAY) != 0;
        if t.address.is_zero() {
            return self.serve_special(&t, oneway);
        }
        let target = self
            .state
            .lock()
            .expect("rpc state poisoned")
            .lookup_local(&t.address);
        let Some(target) = target else {
            if oneway {
                // Oneway is best-effort by definition, but a drop to a
                // GC'd/unknown address is otherwise indistinguishable
                // from delivery — log it for diagnosability (Minor-2).
                log::debug!(
                    "RPC oneway to unknown/released address {:?} dropped",
                    t.address
                );
            } else {
                self.send_reply(StatusCode::DeadObject.into(), &[], &[])?;
            }
            return Ok(());
        };

        // Standard binder control transactions that libbinder's
        // `BBinder::transact` answers *before* `onTransact`, sent with
        // **no interface token** (so they must bypass
        // `consume_rpc_interface_token`). The kernel `Binder` handles
        // these internally; the RPC server adapter must too, or a real
        // libbinder client can't e.g. `getInterfaceDescriptor()` (which
        // `AIBinder_associateClass` needs — G4(b) STAGE3) or `ping`.
        if !oneway {
            match t.code {
                INTERFACE_TRANSACTION => {
                    let mut reply = Parcel::new();
                    reply.attach_rpc_ops(self.parcel_ops());
                    reply.write(&target.descriptor())?;
                    return self.send_reply(0, reply.rpc_data_bytes(), &[]);
                }
                PING_TRANSACTION => {
                    return self.send_reply(0, &[], &[]);
                }
                _ => {}
            }
        }

        let mut reader = Parcel::from_vec(t.data);
        reader.attach_rpc_ops(self.parcel_ops());
        reader.set_rpc_fd_mode(self.fd_mode());
        reader.rpc_set_in_fds(in_fds);
        reader.set_data_position(0);
        let mut reply = Parcel::new();
        reply.attach_rpc_ops(self.parcel_ops());
        reply.set_rpc_fd_mode(self.fd_mode());

        let result = consume_rpc_interface_token(&mut reader, target.descriptor())
            .and_then(|()| target.rpc_transact(t.code, &mut reader, &mut reply));

        if oneway {
            if let Err(e) = result {
                log::error!("oneway RPC transaction failed (dropped): {e:?}");
            }
            return Ok(());
        }
        match result {
            Ok(()) => self.send_reply(0, reply.rpc_data_bytes(), reply.rpc_out_fds()),
            Err(e) => self.send_reply(e.into(), &[], &[]),
        }
    }

    /// Handle one inbound message. `Ok(false)` ⇒ peer closed (stop).
    fn serve_once(&self) -> Result<bool> {
        // The server worker owns the connection while it serves one
        // message; a nested outbound callback from the handler runs on
        // this same thread and bypasses the lock (AC-3.6).
        let _conn = self.enter_connection();
        let (frame, in_fds) = match self.recv_msg() {
            Ok(f) => f,
            Err(RpcError::PeerClosed) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        match self.profile.codec().decode_message(&frame)? {
            WireMessage::Transact(t) => {
                self.dispatch_transact(t, in_fds)?;
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
    /// `GET_ROOT`/`GET_MAX_THREADS`/`GET_SESSION_ID`, plus the
    /// rsbinder/2-7 `GET_FD_MODE` extension).
    fn serve_special(&self, t: &WireTransaction, oneway: bool) -> Result<()> {
        if oneway {
            // Special transactions are never oneway.
            return Ok(());
        }
        match SpecialTransaction::from_code(t.code) {
            Some(SpecialTransaction::GetRoot) => {
                let root = self.root.lock().expect("root poisoned").clone();
                let mut reply = Parcel::new();
                reply.attach_rpc_ops(self.parcel_ops());
                // SIBinder::serialize → RPC branch → write_binder.
                match &root {
                    Some(b) => reply.write(b)?,
                    None => reply.write(&0i32)?,
                }
                self.send_reply(0, reply.rpc_data_bytes(), &[])
            }
            Some(SpecialTransaction::GetMaxThreads) => {
                let n = self.max_threads.load(Ordering::SeqCst) as i32;
                let mut reply = Parcel::new();
                reply.write(&n)?;
                self.send_reply(0, reply.rpc_data_bytes(), &[])
            }
            Some(SpecialTransaction::GetSessionId) => {
                // AOSP `RpcState` server replies `reply.writeByteVector(
                // session->mId)` and the libbinder client reads it with
                // `Parcel::readByteVector` — a 32-byte (`kSessionIdBytes`)
                // opaque id. rsbinder's `Vec<u8>`/`&[u8]` serializer is
                // the AIDL `byte[]` path (`i32 len` + packed bytes +
                // 4-pad) == libbinder `writeByteVector` byte-for-byte.
                // (Was a bare `i32` ⇒ libbinder `BAD_VALUE` — found by
                // the real-peer round-trip, G4(b).)
                let mut reply = Parcel::new();
                reply.write(&self.rpc_session_id[..])?;
                self.send_reply(0, reply.rpc_data_bytes(), &[])
            }
            Some(SpecialTransaction::GetFdMode) => {
                // Body: i32 — does the client want `Unix`. Agree only
                // if this endpoint also supports it (else `None`, never
                // an error — AC-7.3). The reply (0=None,1=Unix) is sent
                // in the *current* (None) mode; both sides switch only
                // after this exchange completes, so framing stays
                // consistent.
                let mut req = Parcel::from_vec(t.data.clone());
                req.set_data_position(0);
                // A malformed body safely defaults to "no FD support"
                // (AC-7.3 — never an error), but log the protocol
                // violation rather than swallow it silently (Minor-1).
                let want_unix = match req.read::<i32>() {
                    Ok(v) => v == 1,
                    Err(e) => {
                        log::debug!("RPC GET_FD_MODE: malformed body ({e:?}); defaulting to None");
                        false
                    }
                };
                let agreed = if want_unix && self.fd_unix_supported.load(Ordering::SeqCst) {
                    FileDescriptorTransportMode::Unix
                } else {
                    FileDescriptorTransportMode::None
                };
                let mut reply = Parcel::new();
                reply.write(
                    &(if agreed == FileDescriptorTransportMode::Unix {
                        1i32
                    } else {
                        0i32
                    }),
                )?;
                self.send_reply(0, reply.rpc_data_bytes(), &[])?;
                // Switch AFTER the reply is on the wire (None-mode).
                *self.fd_mode.lock().expect("fd_mode poisoned") = agreed;
                Ok(())
            }
            None => self.send_reply(StatusCode::UnknownTransaction.into(), &[], &[]),
        }
    }
}

/// A single-connection RPC session (client and/or server role).
#[derive(Clone)]
pub struct RpcSession {
    inner: Arc<RpcSessionInner>,
}

impl RpcSession {
    /// Wrap a connected transport in a session. `space` is this
    /// endpoint's address subspace — [`AddressSpace::Initiator`] for
    /// the side that connected, [`AddressSpace::Acceptor`] for the
    /// side that accepted (so the two peers never mint colliding
    /// addresses on the shared connection).
    pub fn new(transport: Box<dyn RpcTransport>, space: AddressSpace) -> RpcSession {
        // Default = android-12 r34, byte-unchanged.
        RpcSession::with_profile(transport, space, WireProfile::R34(R34Codec))
    }

    /// Build a session over a connected transport with an explicit wire
    /// profile. The android-13+ codec is finalized by the handshake
    /// *before* this is called, so the profile is immutable for the
    /// session's lifetime (no interior mutability — G4(a)).
    fn with_profile(
        transport: Box<dyn RpcTransport>,
        space: AddressSpace,
        profile: WireProfile,
    ) -> RpcSession {
        let inner = Arc::new(RpcSessionInner {
            transport,
            profile,
            conn_lock: Mutex::new(()),
            state: Mutex::new(RpcState::new(space)),
            async_counter: AtomicU64::new(0),
            root: Mutex::new(None),
            self_weak: Mutex::new(Weak::new()),
            max_threads: AtomicU32::new(1),
            negotiated: AtomicU32::new(0),
            timeout: Mutex::new(None),
            fd_mode: Mutex::new(FileDescriptorTransportMode::None),
            fd_unix_supported: AtomicBool::new(false),
            rpc_session_id: gen_rpc_session_id(),
        });
        *inner.self_weak.lock().expect("self_weak") = Arc::downgrade(&inner);
        RpcSession { inner }
    }

    /// Client role, **opt-in android-13+ versioned wire** (subplan 2-5b
    /// / G4(a)). Runs the AOSP connection handshake on `transport`
    /// (`RpcConnectionHeader → RpcNewSessionResponse → "cci"`,
    /// negotiating `min(max_version, server_max)`), then returns a
    /// session that speaks the negotiated version with AOSP-faithful
    /// framing — reusing the existing per-session [`RpcState`] and
    /// `client_transact`/dispatch unchanged. `max_version` is the
    /// highest `RPC_WIRE_PROTOCOL_VERSION` to offer (0 = android-13,
    /// 1 = android-14/15).
    ///
    /// Requires a transport with raw byte access (`unix`); the
    /// frame-only `mem`/`tls`/`vsock` backends reject it by type
    /// (`RpcError::Protocol`). The default [`RpcSession::new`] /
    /// [`RpcSession::setup_unix_client`] keep the r34 wire — this never
    /// changes the byte-unchanged R34 path.
    pub fn connect_android13plus(
        transport: Box<dyn RpcTransport>,
        max_version: u32,
    ) -> Result<RpcSession> {
        let codec = {
            let mut io = RawTransportIo(transport.as_ref());
            client_connect(&mut io, max_version, false, FD_MODE_NONE).map_err(StatusCode::from)?
        };
        Ok(RpcSession::with_profile(
            transport,
            AddressSpace::Initiator,
            WireProfile::Android13Plus(codec),
        ))
    }

    /// Server role, **opt-in android-13+ versioned wire** (G4(a)). Runs
    /// the AOSP accept handshake on an already-accepted `transport`
    /// (negotiates `min(server_max_version, client_max)`), then returns
    /// an [`AddressSpace::Acceptor`] session speaking the negotiated
    /// version. Called by [`super::RpcServer`] on its worker thread (the
    /// handshake is blocking I/O on the accepted socket). The session id
    /// / FD-mode byte from the client header are read for wire fidelity
    /// but, in no-FD G4(a) scope, not yet acted on (a real-peer
    /// refinement is G4(b)).
    pub fn accept_android13plus(
        transport: Box<dyn RpcTransport>,
        server_max_version: u32,
    ) -> Result<RpcSession> {
        let codec = {
            let mut io = RawTransportIo(transport.as_ref());
            let (codec, _fd_mode, _session_id) =
                server_accept(&mut io, server_max_version).map_err(StatusCode::from)?;
            codec
        };
        Ok(RpcSession::with_profile(
            transport,
            AddressSpace::Acceptor,
            WireProfile::Android13Plus(codec),
        ))
    }

    /// The negotiated android-13+ wire protocol version
    /// (`0` = android-13, `1` = android-14/15), or `None` for the
    /// default android-12 r34 profile. Lets a caller assert the
    /// `min(client_max, server_max)` handshake outcome (G4(a)).
    pub fn wire_protocol_version(&self) -> Option<u32> {
        match &self.inner.profile {
            WireProfile::Android13Plus(c) => Some(c.version()),
            WireProfile::R34(_) => None,
        }
    }

    /// Server role: advertise that this endpoint will accept the
    /// `Unix` FD-over-RPC mode on `GET_FD_MODE` (subplan 2-7). Default
    /// is *not* advertised, so the FD reject (2-2) is the default
    /// everywhere. Has no effect on a non-UDS transport (the transport
    /// fd methods reject by type regardless).
    pub fn set_supported_fd_modes(&self, modes: &[FileDescriptorTransportMode]) {
        let unix = modes.contains(&FileDescriptorTransportMode::Unix);
        self.inner.fd_unix_supported.store(unix, Ordering::SeqCst);
    }

    /// Client role: negotiate the FD-over-RPC mode (subplan 2-7).
    /// Sends exactly one `GET_FD_MODE` packet; the agreed mode is
    /// `Unix` iff *both* peers opted in, else `None` (never an error —
    /// AC-7.3). Must be called before any FD-bearing call, like
    /// [`RpcSession::negotiate`].
    pub fn negotiate_fd_transport(
        &self,
        want: FileDescriptorTransportMode,
    ) -> Result<FileDescriptorTransportMode> {
        let want_unix = want == FileDescriptorTransportMode::Unix;
        let mut req = Parcel::new();
        req.write(&(if want_unix { 1i32 } else { 0i32 }))?;
        let mut reply = self
            .inner
            .client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetFdMode.code(),
                &req,
                0,
            )?
            .ok_or(StatusCode::UnexpectedNull)?;
        let agreed = if reply.read::<i32>()? == 1 {
            FileDescriptorTransportMode::Unix
        } else {
            FileDescriptorTransportMode::None
        };
        // Switch AFTER the reply has been fully read in None-mode.
        *self.inner.fd_mode.lock().expect("fd_mode poisoned") = agreed;
        Ok(agreed)
    }

    /// The negotiated FD-over-RPC mode (default `None`).
    pub fn fd_transport_mode(&self) -> FileDescriptorTransportMode {
        self.inner.fd_mode()
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

    /// Server role: the max-threads value advertised to a client on
    /// `GET_MAX_THREADS` (subplan 2-3 negotiation). Default 1.
    pub fn set_max_threads(&self, n: u32) {
        self.inner.max_threads.store(n.max(1), Ordering::SeqCst);
    }

    /// Set the client reply/handshake wait deadline (AC-3.8). `None`
    /// (default) blocks forever.
    pub fn set_timeout(&self, timeout: Option<Duration>) {
        *self.inner.timeout.lock().expect("timeout poisoned") = timeout;
    }

    /// `min(local, remote)` worker count established by
    /// [`RpcSession::negotiate`] (0 if not negotiated).
    pub fn negotiated_max_threads(&self) -> u32 {
        self.inner.negotiated.load(Ordering::SeqCst)
    }

    /// Client role: exchange `GET_MAX_THREADS` with the server and
    /// record `min(local_max, remote_max)` (android
    /// `getRemoteMaxThreads`, AC-3.4). Exactly one negotiation packet.
    pub fn negotiate(&self, local_max: u32) -> Result<u32> {
        let data = Parcel::new();
        let mut reply = self
            .inner
            .client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetMaxThreads.code(),
                &data,
                0,
            )?
            .ok_or(StatusCode::UnexpectedNull)?;
        let remote: i32 = reply.read()?;
        if remote < 1 {
            return Err(StatusCode::BadValue);
        }
        let negotiated = local_max.min(remote as u32).max(1);
        self.inner.negotiated.store(negotiated, Ordering::SeqCst);
        Ok(negotiated)
    }

    /// Client: connect to a Unix-domain RPC server. Thread negotiation
    /// is a separate, explicit step ([`RpcSession::negotiate`]) so a
    /// caller that negotiates does so with exactly one packet (AC-3.4).
    pub fn setup_unix_client(path: impl AsRef<std::path::Path>) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect(path)?;
        Ok(RpcSession::new(Box::new(t), AddressSpace::Initiator))
    }

    /// Client: connect to a Unix-domain RPC server speaking the
    /// **android-13+ versioned wire** (subplan 2-5b / G4(a)). Connects
    /// the UDS, then runs the AOSP handshake via
    /// [`RpcSession::connect_android13plus`] negotiating
    /// `min(max_version, server_max)`. The r34
    /// [`RpcSession::setup_unix_client`] is unchanged.
    pub fn setup_unix_client_android13plus(
        path: impl AsRef<std::path::Path>,
        max_version: u32,
    ) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect(path)?;
        RpcSession::connect_android13plus(Box::new(t), max_version)
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
