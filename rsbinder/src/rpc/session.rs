// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcSession` — single-connection RPC session driver.
//!
//! Ties one [`RpcTransport`] + [`R34Codec`] + per-session [`RpcState`]
//! together and provides:
//! * client outbound transactions ([`RpcSession::get_root`], and
//!   [`super::proxy::RpcProxy::transact`]),
//! * a blocking server serve loop ([`RpcSession::serve_blocking`]),
//! * the [`RpcParcelOps`] bridge that lets the `SIBinder`
//!   (de)serializers marshal binders as `RpcAddress`.
//!
//! All state is owned here (no global).

use std::cell::RefCell;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use super::fd_mode::FileDescriptorTransportMode;
use super::lifecycle::SessionLifecycle;
use crate::binder::{SIBinder, FLAG_ONEWAY, INTERFACE_TRANSACTION, PING_TRANSACTION};
use crate::error::{Result, StatusCode};
use crate::parcel::{Parcel, RpcParcelOps};

use super::address::{AddressSpace, RpcAddress, SpecialTransaction, RPC_ADDR_LEN};
use super::proxy::RpcProxy;
use super::state::RpcState;
use super::transport::{PeerIdentity, RpcTransport};
use super::wire::{R34Codec, WireCodec, WireMessage, WireReply, WireTransaction};
use super::wire_android13::{
    client_connect_with_id, read_aosp_message, read_aosp_message_with_fds, server_accept,
    write_aosp_message, write_aosp_message_with_fds, Android13PlusCodec, RawTransportIo,
    A13_ADDR_LEN, FD_MODE_NONE, FD_MODE_UNIX, PROTOCOL_V1, PROTOCOL_V2,
};
use super::{RpcError, RpcResult};

/// Result of the android-13+ server accept handshake: the unconsumed
/// transport plus the negotiated codec, the client's requested FD
/// mode, the client-supplied `session_id`, and the
/// `RPC_CONNECTION_OPTION_INCOMING` flag.
type Android13PlusAccept = (Box<dyn RpcTransport>, Android13PlusCodec, u8, Vec<u8>, bool);

/// Which RPC wire profile a session speaks.
///
/// The default [`WireProfile::R34`] arm is the
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
/// `wire_android13`; this enum is where they become a live
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

    /// The negotiated wire protocol version, or `None` for the
    /// pre-versioning R34 (android-12) profile (which has no object
    /// table at all).
    fn wire_version(&self) -> Option<u32> {
        match self {
            WireProfile::R34(_) => None,
            WireProfile::Android13Plus(c) => Some(c.version()),
        }
    }

    /// Does a *binder* flattened into an RPC parcel get its position
    /// recorded in the object table? AOSP `Parcel::flattenBinder`:
    /// only at `>= RPC_WIRE_PROTOCOL_VERSION_RPC_HEADER_INCLUDES_
    /// BINDER_POSITIONS` (v2 = android-16). v0/v1/R34: no.
    fn records_binder_positions(&self) -> bool {
        matches!(self.wire_version(), Some(v) if v >= PROTOCOL_V2)
    }

    /// Does an *FD* flattened into an RPC parcel get its position
    /// recorded? AOSP `Parcel::writeFileDescriptor` records it
    /// version-independently, but `validateParcel` rejects a v0
    /// parcel that carries any object ⇒ effectively v1+ (FD over RPC
    /// is itself v1+ negotiated). R34 has no object table.
    fn records_fd_positions(&self) -> bool {
        matches!(self.wire_version(), Some(v) if v >= PROTOCOL_V1)
    }
}

/// Write the RPC interface token — **byte-exact to AOSP
/// `Parcel::writeInterfaceToken` on an RPC parcel**, verified against
/// `android-12.0.0_r34` … `android-16.0.0_r4`: for an RPC parcel
/// (`isForRpc()` / no `kernelFields`) the strict-mode / work-source /
/// `kHeader` triple is **skipped entirely** — it is kernel-binder-only.
/// "the interface identification token is just its name as a string"
/// ⇒ exactly `writeString16(descriptor)` and nothing else.
///
/// rsbinder's `&str` serializer is already byte-identical to AOSP
/// `writeString16` (`[i32 char16_count][UTF-16 LE][u16 0][pad 4]`), so
/// this is now wire-correct against a real libbinder RPC peer for
/// **every** profile (r34 / android-13 v0 / v1 / android-16 v2). The
/// prior 3-int header was an rsbinder-ism that only ever round-tripped
/// hermetically (rsbinder↔rsbinder, symmetric) — now resolved. RPC
/// never touches `thread_state`.
pub(crate) fn write_rpc_interface_token(p: &mut Parcel, descriptor: &str) -> Result<()> {
    p.write(&descriptor)?;
    Ok(())
}

/// Consume + validate the RPC interface token (AOSP RPC
/// `enforceInterface`: just the `String16` descriptor — no
/// strict-mode/work-source/`kHeader`, those are kernel-only).
fn consume_rpc_interface_token(reader: &mut Parcel, expected: &str) -> Result<()> {
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

/// 32-byte AOSP `kSessionIdBytes` opaque session identifier — a
/// CSPRNG-minted **capability for attach**:
/// a peer that echoes this id in the connection header is bound to
/// the *same* `SharedSession` (shared `state`/`root`/cached proxies),
/// so the wire bytes are not just an opaque identifier but a
/// privilege token. Wrapping the `[u8; 32]` in a newtype makes that
/// "attach capability" semantic type-explicit at every internal touch
/// point ([`RpcServer.sessions`](super::RpcServer), `register/resolve/
/// unregister_session`, [`SharedSession::rpc_session_id`],
/// [`gen_rpc_session_id`]), so raw 32-byte values from unrelated
/// origins (e.g. hashes) can't be passed in by accident. Public
/// wire-facing APIs ([`RpcSession::session_id`],
/// [`connect_android13plus_fd_with_id`](RpcSession::connect_android13plus_fd_with_id),
/// [`add_outgoing_connection_android13plus`](RpcSession::add_outgoing_connection_android13plus))
/// keep the raw `[u8; 32]` / `&[u8]` shape for ergonomic compatibility.
///
/// `Debug` deliberately masks the bytes: this id is a capability —
/// logging it leaks the attach token.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RpcSessionId([u8; 32]);

impl RpcSessionId {
    pub(crate) fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Construct from a wire-bytes slice. `None` if `s.len() != 32`
    /// (AOSP `kSessionIdBytes` is a hard 32-byte invariant; anything
    /// else is wire-illegal — see `connect_*_with_id`'s length gate).
    pub(crate) fn try_from_slice(s: &[u8]) -> Option<Self> {
        <[u8; 32]>::try_from(s).ok().map(Self)
    }
}

impl std::fmt::Debug for RpcSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Mask the bytes — capability-leak hazard if a casual {:?} in
        // a log line dumps an attach token.
        f.write_str("RpcSessionId(...)")
    }
}

/// Mint a fresh [`RpcSessionId`] via the OS CSPRNG. AOSP fills it
/// from a CSPRNG and rsbinder must do the same: the id is a
/// **capability for attach**, so a predictable id would be a session-
/// hijack primitive for any same-host peer reachable on the UDS.
/// **Global-free** (no `static` counter); `getrandom` is a
/// stateless syscall (`getrandom(2)` on Linux, `SecRandomCopyBytes`
/// on macOS), so the `rpc_stack_has_no_globals` gate stays clean.
fn gen_rpc_session_id() -> RpcResult<RpcSessionId> {
    let mut id = [0u8; 32];
    // Surface a `getrandom` failure as a recoverable `RpcError::Io`
    // instead of panicking out of a public constructor. `getrandom(2)`
    // *can* fail in early-boot containers
    // (`EAGAIN`/`EINTR` mapping in the `getrandom` crate); a panic
    // would unwind through `RpcSession::new`'s infallible signature
    // with no way for callers to handle.
    getrandom::fill(&mut id).map_err(|e| {
        RpcError::Io(std::io::Error::other(format!(
            "CSPRNG getrandom failed for RPC session id: {e}"
        )))
    })?;
    Ok(RpcSessionId::new(id))
}

/// RAII clear for the `client_transact` reply read-deadline.
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
/// `client_transact`.
///
/// The reply deadline bounds **only the outermost reply
/// wait**. A nested inbound call dispatched while we wait (a server
/// callback) is legitimate, potentially long-running forward
/// progress, not a stall: bounding it would break valid re-entrancy,
/// and time-bounding the nested *reply write* could leave a half-frame
/// on the wire. So the deadline is lifted for the nested dispatch and
/// restored for the continued wait — but **symmetrically via Drop**, so
/// an early `?`/panic out of `dispatch_transact` can never leave the
/// sticky `SO_RCVTIMEO` desynchronized for the rest of the reply loop
/// (a manual clear/re-arm pair could).
///
/// **Cross-session escape caveat**: this guard
/// lifts only the *outer* transport's deadline. A user handler that
/// — during a same-thread nested dispatch — issues an outbound
/// transact on a *different* `RpcSession` will block on **that
/// session's** own deadline (or block forever if that session has
/// none). If the inner session's peer never replies, this guard's
/// `Drop` cannot restore the outer deadline until `dispatch_transact`
/// returns, so the outer caller also hangs indefinitely. Callers
/// driving multi-session relay logic should set a deadline on every
/// session they may transact through (`set_timeout(Some(d))`), not
/// only on the outer one. Same-session nested dispatch is unaffected.
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
    /// `(session_ptr, slot_id)` pairs this thread is currently driving
    /// (outermost `client_transact` / `serve_once_on_slot`). Lets a
    /// same-thread **nested** call (a server handler calling back
    /// while a transaction is in flight) re-enter the
    /// **same slot** the inbound transact arrived on, rather than
    /// either self-deadlocking on its `exclusive_tid` or routing the
    /// callback over a *different* available slot (which would break
    /// AOSP's `exclusiveIncoming->allowNested` ordering guarantee).
    /// The key is `(session, slot)`.
    ///
    /// Per-thread *recursion marker*, **not** session/protocol state:
    /// it holds no node / address / ref-count data — those stay
    /// per-session in [`RpcState`]. It mirrors kernel binder's
    /// thread-local `IPCThreadState`. Documented exception in the
    /// `rpc_stack_has_no_globals` gate.
    static DRIVING: RefCell<Vec<(usize, u64)>> = const { RefCell::new(Vec::new()) };
}

/// AOSP `RpcConnection::exclusiveTid` equivalent — `std::thread::
/// ThreadId` (`Copy + Eq`, opaque: NO process global, NO extra
/// thread_local needed beyond `std`'s own thread bookkeeping).
type Tid = std::thread::ThreadId;

#[inline]
fn current_tid() -> Tid {
    std::thread::current().id()
}

/// One connection slot of an `RpcSessionInner`'s pool —
/// the rsbinder equivalent of AOSP `RpcSession::RpcConnection`.
struct ConnSlot {
    /// The connection's transport, held as `Arc<dyn RpcTransport>` so a
    /// [`ConnGuard`] holding an `Arc::clone` keeps the heap object
    /// alive even if [`remove_slot`](RpcSessionInner::remove_slot)
    /// drops this slot from the pool while the guard is in flight.
    /// [`remove_slot`] retires a slot on its own worker's exit;
    /// refcounting via `Arc` is the liveness guarantee that keeps the
    /// heap object alive for any in-flight guard.
    transport: Arc<dyn RpcTransport>,
    /// AOSP `RpcConnection::exclusiveTid`: thread currently driving
    /// this slot, or `None` if available. [`find_conn`] picks the
    /// first available; pool exhaustion **blocks on the session's
    /// `Condvar`** (AOSP `mAvailableConnectionCv`) — never a busy
    /// try-loop.
    exclusive_tid: Option<Tid>,
    /// Monotonic local id (the [`DRIVING`] reentrancy key + the
    /// server-worker handle). Stable for the slot's life —
    /// [`remove_slot`](RpcSessionInner::remove_slot) drops the slot
    /// only on its own worker's exit, never re-using an id.
    id: u64,
}

/// The session's connection pool + its monotonic slot-id
/// counter. Behind the single per-session `Mutex` paired with the
/// `Condvar` on [`RpcSessionInner`] — *not* N independent mutexes
/// (AOSP `RpcSession::mMutex` + `mAvailableConnectionCv`).
struct ConnState {
    slots: Vec<ConnSlot>,
    next_slot_id: u64,
}

/// RAII guard for one selected connection slot. Built by
/// [`RpcSessionInner::find_conn`]. Holds the slot exclusive_tid==this
/// thread (unless reentrant) and an `Arc::clone` of the slot's
/// transport so subsequent `send_msg`/`recv_msg` do **no** locking —
/// concurrent client transacts on *other* slots run unimpeded.
/// On drop: clears `exclusive_tid` (unless reentrant),
/// pops the [`DRIVING`] marker, and notifies waiters.
struct ConnGuard<'a> {
    inner: &'a RpcSessionInner,
    slot_id: u64,
    /// `Arc::clone` of the chosen slot's transport. Pins the heap
    /// object alive for the guard's lifetime even if
    /// [`remove_slot`](RpcSessionInner::remove_slot) concurrently
    /// retires the slot from the pool — the underlying `Box`-equivalent
    /// allocation is only freed when the last `Arc` (slot Vec entry +
    /// any live guard) drops.
    transport: Arc<dyn RpcTransport>,
    /// `true` ⇒ same-thread nested call reused this slot via
    /// [`DRIVING`]; drop must NOT release `exclusive_tid` (the outer
    /// frame still holds it).
    reentrant: bool,
}

impl ConnGuard<'_> {
    /// Borrow the selected slot's transport. Stable for the guard's
    /// lifetime via the held `Arc::clone`.
    #[inline]
    fn transport(&self) -> &dyn RpcTransport {
        &*self.transport
    }
}

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        let key = (self.inner as *const _ as usize, self.slot_id);
        DRIVING.with(|d| {
            let mut v = d.borrow_mut();
            if let Some(pos) = v.iter().rposition(|&k| k == key) {
                v.remove(pos);
            }
        });
        if !self.reentrant {
            // Release exclusive ownership and wake waiters. We use
            // `notify_all` to match [`add_slot_inner`] / [`remove_slot`]:
            // a slot release benefits (a) `find_conn` any-available
            // waiters and (b) `find_conn_pinned(self.slot_id)` waiters,
            // but is irrelevant to `find_conn_pinned(other_id)`
            // waiters. `std::Condvar` makes no FIFO guarantee, so
            // `notify_one` could pick a pinned-elsewhere waiter that
            // then re-`wait`s — leaving the actually-relevant waiter
            // asleep. The thundering-herd cost is bounded by waiter
            // count (and is zero on the default single-slot path).
            let mut st = self.inner.conn_state.lock().expect("conn_state poisoned");
            if let Some(s) = st.slots.iter_mut().find(|s| s.id == self.slot_id) {
                s.exclusive_tid = None;
            }
            drop(st);
            self.inner.slot_cv.notify_all();
        }
    }
}

/// The state shared by *all connections of
/// one logical session* (AOSP `RpcSession` shares this across its
/// `mOutgoing`/`mIncoming` connections). One per session, behind `Arc`;
/// never global. Default single-connection sessions own exactly
/// one of these with `lifecycle == Live(1)` ⇒ behavior is byte-identical
/// to a single-`transport` `RpcSessionInner` (the `Arc`
/// indirection is the only structural change; the wire is unchanged).
/// The server attaches a 2nd+ connection to a *pre-existing* instance
/// (id-demux), so a binder published over one connection is reachable
/// over another (shared `state`/`root`). `pub(crate)` only so
/// [`super::RpcServer`] can keep a [`std::sync::Weak`] of it in its
/// id→session registry — an opaque handle, not public API.
pub(crate) struct SharedSession {
    state: Mutex<RpcState>,
    root: Mutex<Option<SIBinder>>,
    /// Max-threads value advertised to the peer on `GET_MAX_THREADS`
    /// (server side) — handshake negotiation.
    max_threads: AtomicU32,
    /// `min(local, remote)` after the client handshake (0 until done).
    negotiated: AtomicU32,
    /// Optional reply/handshake wait deadline.
    timeout: Mutex<Option<Duration>>,
    /// Negotiated FD-over-RPC mode. Default `None` ⇒ the
    /// categorical reject path, and `send/recv` use the unchanged
    /// framed calls (bit-identical).
    fd_mode: Mutex<crate::rpc::FileDescriptorTransportMode>,
    /// Server role: does this endpoint advertise `Unix` FD support on
    /// `GET_FD_MODE`. Default false.
    fd_unix_supported: AtomicBool,
    /// Opaque 32-byte session id returned by the `GET_SESSION_ID`
    /// special transact. AOSP `RpcServer` assigns a random
    /// `kSessionIdBytes == 32` id; the libbinder client reads it with
    /// `Parcel::readByteVector` and would `BAD_VALUE` on any other size
    /// (real-peer-validated). Per-session, never global —
    /// generated global-free in [`RpcSession::with_profile`]. Shared
    /// across the session's connections (an attached 2nd
    /// connection reports the *same* id). [`RpcSessionId`] newtype
    /// makes the "attach capability" semantic type-explicit.
    rpc_session_id: RpcSessionId,
    /// Typed lifecycle. Atomic-backed
    /// `Live(n: NonZeroUsize) / Dying / Dead` state machine; replaces
    /// a `live_conns: AtomicUsize` + `obituary_sent: AtomicBool`
    /// pair. The founding connection starts in `Live(1)`; the server's
    /// id-demux attach calls
    /// [`try_bump_live`](super::lifecycle::SessionLifecycle::try_bump_live)
    /// (CAS-loop — closes the multi-attacker hole at the type
    /// level). `serve_blocking_on` exit calls `drop_connection`; on the
    /// `1→0` edge (full session teardown) the caller fires the session
    /// obituaries and then `mark_dead`s. The intermediate `Dying` state
    /// surfaces as
    /// [`is_torn_down()`](super::lifecycle::SessionLifecycle::is_torn_down)
    /// `== true` *before* the obituary completes, so `RpcProxy::drop`
    /// best-effort reapers skip immediately instead of blocking on an
    /// empty slot pool — a strict improvement over the prior
    /// `obituary_sent.load()` (which only flipped after the callback
    /// returned).
    lifecycle: SessionLifecycle,
}

impl SharedSession {
    /// Live local-node count of this (possibly multi-connection)
    /// session's shared `RpcState` — leak observability
    /// (the AOSP `timesSent` books must net to 0 nodes once every
    /// proxy is dropped). Used by [`super::RpcServer`]'s test helper.
    pub(crate) fn local_node_count(&self) -> usize {
        self.state
            .lock()
            .expect("rpc state poisoned")
            .local_node_count()
    }

    /// Current live connection count — the lifecycle ledger primitive.
    /// Used by tests as a *deterministic* witness that a connection-drop
    /// has been fully reaped by its server worker (`serve_blocking_on`
    /// exit's `drop_connection`), instead of a `sleep` heuristic that
    /// races server-scheduler jitter. `0` in both `Dying` and `Dead`.
    pub(crate) fn live_conn_count(&self) -> usize {
        self.lifecycle.live_count()
    }

    /// **Anti-resurrection primitive.** Thin wrapper around
    /// [`SessionLifecycle::try_bump_live`] — see the type doc on
    /// [`super::lifecycle::SessionLifecycle`] for the CAS-loop
    /// rationale (multi-attacker hole) and the typed
    /// lifecycle that makes `Dying`/`Dead` unobservable as a transient
    /// "still Live" state from any other observer.
    pub(crate) fn try_bump_live_conns(&self) -> bool {
        self.lifecycle.try_bump_live()
    }
}

/// One `RpcSessionInner` per logical session, owning a
/// **pool** of `ConnSlot`s (AOSP `RpcSession`'s `mOutgoing`/
/// `mIncoming` connections collapsed into one duplex Vec — the
/// reentrant `DRIVING` `(session, slot)` pin keeps this
/// wire-equivalent to a split pool). `find_conn`
/// selects an available slot for outgoing calls (distribution
/// plus nested-pin for re-entrant callbacks); server workers serve a
/// specific slot via [`serve_blocking_on`](RpcSession::serve_blocking_on)
/// (their inbound connection). Default single-connection sessions own
/// one slot, so `find_conn` is a no-wait single-slot pick and the
/// `enter_connection` semantics are byte-identical.
pub struct RpcSessionInner {
    /// AOSP `RpcSession::mMutex` — the session's *single* connection
    /// pool lock, paired with [`slot_cv`](RpcSessionInner::slot_cv).
    /// Held briefly to pick a slot ([`find_conn`]); released for the
    /// duration of the chosen slot's send/recv so concurrent
    /// `find_conn`s on **other** slots run unblocked. **Not
    /// N independent mutexes**: a single per-session mutex +
    /// condvar is the AOSP-faithful selection primitive.
    conn_state: Mutex<ConnState>,
    /// AOSP `RpcSession::mAvailableConnectionCv`: woken on slot release
    /// and on slot addition. `find_conn` `wait`s here when the pool is
    /// exhausted — **block-and-wait, never busy try-loop**.
    slot_cv: Condvar,
    /// Wire profile: R34 (default, byte-unchanged) or the opt-in
    /// android-13+ versioned wire. Fixed for the session — all
    /// slots in one session speak the same profile (AOSP requires the
    /// negotiated version match across a session; attach paths reject
    /// a profile-mismatch — see [`add_incoming_slot`]).
    profile: WireProfile,
    self_weak: Mutex<Weak<RpcSessionInner>>,
    /// Non-blocking `DEC_STRONG` hand-off.
    /// `RpcProxy::drop` enqueues here and returns immediately; a
    /// dedicated reaper thread (spawned in [`with_shared`]) drains the
    /// queue and runs the blocking `find_conn` + `send_msg` off the
    /// user thread. Inner drop closes the channel ⇒ reaper exits via
    /// `recv` error; in-flight enqueues drain naturally (mpsc holds
    /// queued items until the receiver consumes them).
    dec_strong_tx: mpsc::Sender<RpcAddress>,
    /// Session-wide state shared across this session's slots. The
    /// attach path adds slots onto the founding inner directly, so a
    /// single inner owns the whole slot pool;
    /// `local_node_count`/`rpc_session_id`/lifecycle live in
    /// `SharedSession` so the leak/teardown invariants are anchored in
    /// one place.
    shared: Arc<SharedSession>,
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

/// Resolve an inbound peer to the `(uid, pid)` stamped into the RPC
/// calling context (Plan 2-16 Phase B). Unix peers carry a kernel-vouched
/// uid/pid; uid-less transports (`Vsock` / `Certificate` / `Anonymous`)
/// fail-closed to the `RPC_UNKNOWN_CALLING_UID` sentinel, never `0`/root.
fn caller_ids(peer: &PeerIdentity) -> (u32, i32) {
    match peer {
        PeerIdentity::Local { uid, pid } => (*uid, *pid),
        _ => (crate::thread_state::RPC_UNKNOWN_CALLING_UID, -1),
    }
}

impl RpcSessionInner {
    /// AOSP `RpcSession::ExclusiveConnection::find`. Selects
    /// **a** connection slot for this thread to drive (outgoing
    /// `client_transact` / `send_dec_strong`) — returning a
    /// [`ConnGuard`] that owns the slot until drop. Order (AOSP-
    /// faithful):
    ///
    ///  1. **Reentrant pin** — if this thread is already driving a
    ///     slot of *this* session (the `DRIVING` marker matches), the
    ///     nested call **re-enters that slot** (a server
    ///     handler's outbound callback returns on the inbound socket;
    ///     a same-thread recursive `client_transact` reuses the outer
    ///     slot). No `conn_state` lock — the outer frame holds the
    ///     slot.
    ///  2. **Exclusive** — a slot whose `exclusive_tid == this tid`
    ///     (defensive: should be covered by 1).
    ///  3. **First available** — the first slot with `exclusive_tid ==
    ///     None`. Claim it (`exclusive_tid = this tid`), push the
    ///     `DRIVING` marker so any same-thread nested call re-enters
    ///     here, return the guard.
    ///  4. **Pool exhausted** — `wait` on `slot_cv` (released on slot
    ///     drop OR `add_*_slot`). **Block-and-wait, never busy
    ///     try-loop**.
    ///
    /// Single-slot default: step 3 always succeeds
    /// without `wait`; the `DRIVING`-keyed reentrancy bypass collapses
    /// to the `enter_connection` semantics — byte-identical (no wire
    /// effect).
    ///
    /// **Oneway distribution.** This function is slot-policy-invariant:
    /// top-level oneway sends join the same slot distribution as twoway
    /// sends. Per-object oneway FIFO ordering is carried entirely by
    /// the per-`mNodeForAddress` `asyncNumber` send-side counter +
    /// receive-side priority replay (see [`super::state`]), not by
    /// pinning oneway sends to a fixed slot.
    fn find_conn(&self) -> ConnGuard<'_> {
        let tid = current_tid();
        let sess_ptr = self as *const RpcSessionInner as usize;
        // (1) Reentrant: a slot of this session is already driven by
        //     this thread (innermost first — `rposition`).
        if let Some(slot_id) = DRIVING.with(|d| {
            d.borrow()
                .iter()
                .rev()
                .find_map(|&(sp, sid)| if sp == sess_ptr { Some(sid) } else { None })
        }) {
            let transport = {
                let st = self.conn_state.lock().expect("conn_state poisoned");
                st.slots
                    .iter()
                    .find(|s| s.id == slot_id)
                    .map(|s| Arc::clone(&s.transport))
                    .expect("DRIVING slot present")
            };
            return ConnGuard {
                inner: self,
                slot_id,
                transport,
                reentrant: true,
            };
        }
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        loop {
            // (2) Defensive exclusive match (shouldn't fire if (1) is correct).
            // (3) First available.
            if let Some(s) = st
                .slots
                .iter_mut()
                .find(|s| s.exclusive_tid == Some(tid) || s.exclusive_tid.is_none())
            {
                s.exclusive_tid = Some(tid);
                let slot_id = s.id;
                let transport = Arc::clone(&s.transport);
                DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
                return ConnGuard {
                    inner: self,
                    slot_id,
                    transport,
                    reentrant: false,
                };
            }
            // (4) Pool exhausted — wait. The `Condvar` is woken on
            //     slot release (ConnGuard drop) or slot addition
            //     (`add_*_slot`). Spurious wakes loop back to scan.
            st = self.slot_cv.wait(st).expect("slot_cv poisoned");
        }
    }

    /// Non-blocking variant of [`find_conn`]
    /// for `RpcProxy::drop`'s fast path. Returns `None` instead of
    /// waiting on `slot_cv` when the pool has no free slot. Does NOT
    /// touch the reentrant `DRIVING` short-circuit (callers in Drop
    /// context are by definition not the slot's driver).
    fn try_find_conn(&self) -> Option<ConnGuard<'_>> {
        let tid = current_tid();
        let sess_ptr = self as *const RpcSessionInner as usize;
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        let s = st
            .slots
            .iter_mut()
            .find(|s| s.exclusive_tid == Some(tid) || s.exclusive_tid.is_none())?;
        s.exclusive_tid = Some(tid);
        let slot_id = s.id;
        let transport = Arc::clone(&s.transport);
        DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
        Some(ConnGuard {
            inner: self,
            slot_id,
            transport,
            reentrant: false,
        })
    }

    /// Reaper-only slot acquisition that is torn-down-aware. Unlike
    /// [`find_conn`], it re-checks the session lifecycle (and the slot
    /// pool) on **every** `slot_cv` wakeup and bails with `None` when the
    /// session is torn down or the pool has drained.
    ///
    /// [`find_conn`]'s pool-exhausted arm waits on `slot_cv` with no
    /// torn-down re-check. The reaper holds a strong `Arc` to the inner
    /// while it waits, so if a concurrent peer-close drains the last slot
    /// (`remove_slot` → `notify_all`) between the reaper's pre-entry
    /// `is_torn_down()` check and its scan, the plain `find_conn` would
    /// re-`wait()` forever on an empty pool that `add_*_slot` can never
    /// refill on a dead session — permanently parking the reaper and
    /// leaking the entire session graph (inner + state + cached proxies +
    /// the reaper thread). Bailing here lets the strong `Arc` drop so the
    /// channel closes and the session is reclaimed.
    fn find_conn_for_reaper(&self) -> Option<ConnGuard<'_>> {
        let tid = current_tid();
        let sess_ptr = self as *const RpcSessionInner as usize;
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        loop {
            if self.shared.lifecycle.is_torn_down() || st.slots.is_empty() {
                return None;
            }
            if let Some(s) = st
                .slots
                .iter_mut()
                .find(|s| s.exclusive_tid == Some(tid) || s.exclusive_tid.is_none())
            {
                s.exclusive_tid = Some(tid);
                let slot_id = s.id;
                let transport = Arc::clone(&s.transport);
                DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
                return Some(ConnGuard {
                    inner: self,
                    slot_id,
                    transport,
                    reentrant: false,
                });
            }
            st = self.slot_cv.wait(st).expect("slot_cv poisoned");
        }
    }

    /// Same as [`find_conn`] but pins to a **specific** slot id. Used
    /// by the server worker's `serve_blocking_on(slot_id)` (each
    /// worker drives only its own inbound slot) and by the oneway
    /// path in [`find_conn`] (founding-slot FIFO).
    ///
    /// `Err(StatusCode::DeadObject)` when `want_slot_id` is not in
    /// the pool — legitimately happens on the oneway path when the
    /// founding slot's worker has exited (`remove_slot(FOUNDING_
    /// SLOT_ID)`) while attached slots are still alive; the caller
    /// falls back to any-available. Reentrant on the same slot via
    /// DRIVING, like `find_conn`.
    fn find_conn_pinned(&self, want_slot_id: u64) -> Result<ConnGuard<'_>> {
        let tid = current_tid();
        let sess_ptr = self as *const RpcSessionInner as usize;
        // Reentrant on the same slot.
        if DRIVING.with(|d| {
            d.borrow()
                .iter()
                .any(|&(sp, sid)| sp == sess_ptr && sid == want_slot_id)
        }) {
            let transport = {
                let st = self.conn_state.lock().expect("conn_state poisoned");
                st.slots
                    .iter()
                    .find(|s| s.id == want_slot_id)
                    .map(|s| Arc::clone(&s.transport))
                    .expect("pinned slot present")
            };
            return Ok(ConnGuard {
                inner: self,
                slot_id: want_slot_id,
                transport,
                reentrant: true,
            });
        }
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        loop {
            let target = st.slots.iter_mut().find(|s| s.id == want_slot_id);
            let Some(slot) = target else {
                // A slot can be removed when its own worker exits
                // (`remove_slot` from `serve_blocking_on`). After that the
                // session may still be alive (other slots), but anyone who
                // pinned to this specific id (e.g. oneway → founding-slot
                // pin) must surface a typed error so the caller can fall
                // back rather than panicking out of library code.
                return Err(StatusCode::DeadObject);
            };
            if slot.exclusive_tid.is_none() || slot.exclusive_tid == Some(tid) {
                slot.exclusive_tid = Some(tid);
                let transport = Arc::clone(&slot.transport);
                DRIVING.with(|d| d.borrow_mut().push((sess_ptr, want_slot_id)));
                return Ok(ConnGuard {
                    inner: self,
                    slot_id: want_slot_id,
                    transport,
                    reentrant: false,
                });
            }
            st = self.slot_cv.wait(st).expect("slot_cv poisoned");
        }
    }

    pub(crate) fn parcel_ops(&self) -> Arc<dyn RpcParcelOps> {
        Arc::new(SessionParcelOps(
            self.self_weak.lock().expect("self_weak").clone(),
        ))
    }

    /// Append a new connection slot. Notifies `slot_cv`
    /// (a `find_conn` waiter blocked on "any available" can wake).
    /// Does NOT touch `live_conns` — that bookkeeping is the caller's
    /// (server-incoming bumps it via
    /// [`SharedSession::try_bump_live_conns`]; client-outgoing doesn't,
    /// since outgoing slots aren't serve-driven on the client side).
    ///
    /// Uses `notify_all` (same rationale as [`ConnGuard::drop`] and
    /// [`Self::remove_slot`]): a freshly-pushed slot satisfies
    /// `find_conn`'s "any available" waiters but NOT
    /// `find_conn_pinned(other_id)` waiters, and `std::Condvar` makes
    /// no FIFO guarantee, so `notify_one` could wake a pinned-elsewhere
    /// waiter and starve the any-available ones the new slot was
    /// actually for. The thundering-herd cost is bounded by waiter
    /// count and is zero on the default single-slot path (no waiters
    /// at all), so the trade favors mixed-waiter correctness.
    fn add_slot_inner(&self, transport: Box<dyn RpcTransport>) -> u64 {
        // `Arc::from(Box<dyn T>)` is the stable std conversion that
        // re-takes the heap allocation under an `Arc` without copying
        // (impl<T: ?Sized> From<Box<T>> for Arc<T>). The slot holds
        // the canonical `Arc`; `ConnGuard`s hand out cheap clones.
        let transport: Arc<dyn RpcTransport> = Arc::from(transport);
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        let id = st.next_slot_id;
        st.next_slot_id += 1;
        st.slots.push(ConnSlot {
            transport,
            exclusive_tid: None,
            id,
        });
        drop(st);
        self.slot_cv.notify_all();
        id
    }

    /// Client multi-outgoing: append an *outgoing*
    /// connection slot (no `live_conns` bump — client outgoing slots
    /// are not serve-driven). See
    /// [`RpcSession::add_outgoing_connection_android13plus`].
    fn add_outgoing_slot(&self, transport: Box<dyn RpcTransport>) -> u64 {
        self.add_slot_inner(transport)
    }

    /// Remove a slot from the pool on its worker's
    /// `serve_blocking_on` exit. Called by the slot's *own* worker
    /// (self-remove), so `find_conn_pinned(slot_id)`'s
    /// `panic!("removed from pool")` remains structurally unreachable
    /// (the slot's exclusive holder is the dropping thread itself; no
    /// other thread can be pinned on it). `notify_all` so any
    /// `find_conn` (any-available) waiter re-evaluates against the
    /// shrunk pool — and any `find_conn_pinned(slot_id)` waiter
    /// surfaces the structural error (treated as a should-never panic).
    fn remove_slot(&self, slot_id: u64) {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        st.slots.retain(|s| s.id != slot_id);
        drop(st);
        self.slot_cv.notify_all();
    }

    pub(crate) fn fd_mode(&self) -> FileDescriptorTransportMode {
        *self.shared.fd_mode.lock().expect("fd_mode poisoned")
    }

    /// Whether an RPC parcel built for this session records FD object
    /// positions — i.e. the android-13+ v1+
    /// profile. The session stamps every RPC parcel with this
    /// alongside the FD mode; binder positions are recorded by
    /// [`RpcSessionInner::write_binder`] directly (it owns the
    /// profile). R34 ⇒ `false` (no object table — byte-unchanged).
    pub(crate) fn records_fd_positions(&self) -> bool {
        self.profile.records_fd_positions()
    }

    /// Send one wire frame on `transport` (the slot picked by
    /// [`find_conn`]). Only a `Unix`-mode connection routes fds via
    /// `SCM_RIGHTS`; the default (`None`) uses the unchanged framed
    /// send and never carries fds (bit-identical).
    fn send_msg(
        &self,
        transport: &dyn RpcTransport,
        frame: &[u8],
        fds: &[OwnedFd],
    ) -> RpcResult<()> {
        if self.profile.aosp_framing() {
            // android-13+: the real AOSP wire has **no** length prefix —
            // write `frame` (= the codec's `[RpcWireHeader|body]`) raw
            // over the transport's byte channel, exactly what a genuine
            // android-13/14/15/16 peer reads. On a v1+ `Unix` session
            // (header-negotiated FD mode) the fds
            // ride the message's first `sendmsg` (AOSP `RpcTransportRaw`);
            // otherwise no fds are ever produced here (no-FD scope —
            // R34/v0/`None`, byte-identical).
            if self.fd_mode() == FileDescriptorTransportMode::Unix {
                let borrowed: Vec<_> = fds.iter().map(|f| f.as_fd()).collect();
                return write_aosp_message_with_fds(transport, frame, &borrowed);
            }
            debug_assert!(
                fds.is_empty(),
                "non-Unix android-13+ session must not carry fds"
            );
            let _ = fds; // release: `debug_assert!` is compiled out, so `fds` is otherwise unused
            let mut io = RawTransportIo(transport);
            return write_aosp_message(&mut io, frame);
        }
        if self.fd_mode() == FileDescriptorTransportMode::Unix {
            let borrowed: Vec<_> = fds.iter().map(|f| f.as_fd()).collect();
            transport.send_frame_with_fds(frame, &borrowed)
        } else {
            transport.send_frame(frame)
        }
    }

    /// Receive one wire frame on `transport` (+ any `SCM_RIGHTS` fds
    /// in `Unix` mode). A connection never mixes the `Read` and
    /// `recvmsg` paths because the mode is fixed by negotiation before
    /// any RPC traffic.
    fn recv_msg(&self, transport: &dyn RpcTransport) -> RpcResult<(Vec<u8>, Vec<OwnedFd>)> {
        if self.profile.aosp_framing() {
            // android-13+: read `RpcWireHeader` then exactly `bodySize`
            // bytes (capped vs `MAX_FRAME_LEN`); a clean EOF before
            // any byte surfaces as `PeerClosed` so the `serve_blocking`
            // loop terminates exactly like the R34 path. On a v1+ `Unix`
            // session the same connection always
            // uses `recvmsg` (never mixes with `Read`), accumulating the
            // `SCM_RIGHTS` fds across the header+body reads; otherwise no
            // out-of-band fds (no-FD scope, byte-identical).
            if self.fd_mode() == FileDescriptorTransportMode::Unix {
                return read_aosp_message_with_fds(transport);
            }
            let mut io = RawTransportIo(transport);
            let frame = read_aosp_message(&mut io)?;
            return Ok((frame, Vec::new()));
        }
        if self.fd_mode() == FileDescriptorTransportMode::Unix {
            transport.recv_frame_with_fds()
        } else {
            Ok((transport.recv_frame()?, Vec::new()))
        }
    }

    fn self_weak(&self) -> Weak<RpcSessionInner> {
        self.self_weak.lock().expect("self_weak").clone()
    }

    /// Leak observability delegated to
    /// [`SharedSession::local_node_count`]. Lets [`super::RpcServer`]
    /// keep its public `live_session_node_count` API byte-unchanged
    /// while the registry holds `Weak<RpcSessionInner>` (the
    /// one-inner-per-session handle) rather than `Weak<SharedSession>`.
    pub(crate) fn local_node_count(&self) -> usize {
        self.shared.local_node_count()
    }

    /// Deterministic teardown witness delegated to
    /// [`SharedSession::live_conn_count`]. Counterpart of
    /// [`local_node_count`](RpcSessionInner::local_node_count) for
    /// `RpcServer::session_live_conns`.
    pub(crate) fn live_conn_count(&self) -> usize {
        self.shared.live_conn_count()
    }

    /// Count of slots currently in this session's
    /// pool. Server-side unification means each id-echoing attached
    /// connection adds a slot to the *founding* inner rather than
    /// building a fresh inner; a topology that built a fresh
    /// inner per attach would leave the founding inner at a single slot.
    /// Used by [`super::RpcServer::session_slot_count`].
    pub(crate) fn slot_count(&self) -> usize {
        self.conn_state
            .lock()
            .expect("conn_state poisoned")
            .slots
            .len()
    }

    /// This session's advertised + enforced max-threads
    /// value. Set by [`RpcSession::set_max_threads`] (default 1) and
    /// returned to a client on `GET_MAX_THREADS`. AOSP-faithful
    /// `setMaxIncomingThreads`: the value is *both* the advertise and
    /// the **incoming slot cap** — the server attach arm refuses an
    /// id-echoing connection when adding it would push
    /// `slot_count() > max_threads_value()`. Default 1 ⇒ founding-only
    /// (multi-conn callers must explicitly `set_max_threads(N >= 2)`).
    pub(crate) fn max_threads_value(&self) -> u32 {
        // `Relaxed` is sufficient: this atomic is a single-cell config
        // value (set by `RpcSession::set_max_threads` before any
        // accept/attach takes place; `std::thread::spawn` then provides
        // the happens-before for worker threads). No multi-atomic
        // ordering pair to maintain — the prior `SeqCst` was overkill.
        self.shared.max_threads.load(Ordering::Relaxed)
    }

    /// The negotiated wire protocol version of this
    /// session, or `None` for R34. The server attach arm uses this to
    /// reject an id-echoing 2nd+ connection whose handshake settled on
    /// a different version than the founding inner — profile is
    /// immutable across a session, so any mismatch is a malformed
    /// peer.
    pub(crate) fn wire_protocol_version(&self) -> Option<u32> {
        self.profile.wire_version()
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
    ///   the creation of"`) — real-peer-pinned Parcel-body conformance.
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
                    self.shared
                        .state
                        .lock()
                        .expect("rpc state poisoned")
                        .on_binder_leaving(b)
                };
                // AOSP `Parcel::flattenBinder`: `dataPos = mDataPos`
                // is captured **before** `writeInt32(TYPE_BINDER)` —
                // the position points at the `present`/TYPE_BINDER
                // int32 itself, and is recorded into the object table
                // only at v2 (`>= INCLUDES_BINDER_POSITIONS`). null
                // binders (`TYPE_BINDER_NULL`, the `None` arm) get no
                // position. `rpc_record_object_position` is itself
                // hard-gated on `is_for_rpc`, so the kernel wire can
                // never grow a table.
                let obj_pos = parcel.data_position();
                parcel.write(&1i32)?;
                self.wire_write_binder_addr(parcel, &addr);
                if self.profile.records_binder_positions() {
                    parcel.rpc_record_object_position(obj_pos);
                }
                if matches!(self.profile, WireProfile::Android13Plus(_)) {
                    // AOSP `Parcel::finishFlattenBinder` →
                    // `writeInt32(Stability::getRepr(binder))`. r34's
                    // rsbinder↔rsbinder path is symmetric and omits it;
                    // the real libbinder peer's `finishUnflattenBinder`
                    // *requires* it (else a short read ⇒ null root —
                    // real-peer-pinned). We send the binder's *actual*
                    // declared stability (`getRepr`-faithful), not a
                    // hardcoded 0: rsbinder's default is
                    // `Stability::System` (= `0b001100`; +`0x0c000000`
                    // on android sdk 31/32), which libbinder accepts as
                    // a declared level for an RPC binder.
                    let rep: i32 = b.stability().into();
                    parcel.write(&rep)?;
                }
                // Freeze runtime stability mutation once the binder has
                // crossed the IPC boundary, on both wire profiles. RPC
                // sessions can carry handwritten native binders too, so
                // the parcel-emit hook applies here just like in the
                // kernel path (see `parcelable.rs`).
                b.set_parceled();
                Ok(())
            }
        }
    }

    /// android `unflattenBinder` (RPC branch).
    fn read_binder(&self, parcel: &mut Parcel) -> Result<Option<SIBinder>> {
        // AOSP `Parcel::unflattenBinder`: `objectPos = mDataPos`
        // captured **before** reading the present/type int32.
        let obj_pos = parcel.data_position();
        let present: i32 = parcel.read()?;
        if present == 0 {
            return Ok(None);
        }
        // v2 strict receive validation: at v2 a
        // binder may only be read from a position recorded in the
        // object table (`std::binary_search(mObjectPositions,
        // objectPos)` ⇒ `BAD_VALUE` otherwise). v0/v1/R34 never
        // record binder positions (binder is inline-lazy), so the
        // check is correctly v2-only — exactly AOSP's
        // `bindersInObjectPositions` gate. Interop does not require
        // this (a lenient decoder still round-trips); it hardens v2
        // *conformance*.
        if self.profile.records_binder_positions() && !parcel.rpc_object_position_present(obj_pos) {
            return Err(StatusCode::BadValue);
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
            .shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .lookup_local(&addr)
        {
            return Ok(Some(local));
        }
        let weak = self.self_weak();
        // Explicit inner block: the `MutexGuard` is bound to `st` and
        // dropped at the closing `}`, **before** the excess
        // `DEC_STRONG` send below. This makes the no-I/O-under-the-
        // state-lock invariant structural (rather than relying on
        // Rust's temporary-scope inference for an unbound `lock()`
        // chain, which a future refactor could silently break — e.g.,
        // re-pulling the `lock()` out into a `let g = ...` would extend
        // the guard's lifetime past the `if excess` send, violating
        // the invariant: no I/O / no callback under a `Mutex` lock the
        // recv loop also touches).
        let (sib, excess) = {
            let mut st = self.shared.state.lock().expect("rpc state poisoned");
            st.remote_proxy(addr, || {
                SIBinder::new(Arc::new(RpcProxy::new(addr, weak))).expect("SIBinder::new(RpcProxy)")
            })
        };
        if excess {
            // AOSP `flushExcessBinderRefs`: a duplicate
            // receipt of a binder we already proxy. The sender bumped
            // its `timesSent` for this send, but our deduped proxy
            // `DEC_STRONG`s only once (at its drop); return the owed
            // reference now so the books net to one DEC per send (no
            // leak). Best-effort, exactly like
            // `RpcProxy::drop`'s DEC: a dead session just means the
            // peer is already gone. On the server dispatch path this
            // runs while this thread already drives the connection
            // (`DRIVING`), so `send_dec_strong`'s `enter_connection`
            // is a reentrant bypass — the DEC is an ordinary
            // standalone frame the peer's recv loop applies
            // independently (the documented "interleaved DEC_STRONG").
            let _ = self.send_dec_strong(addr);
        }
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
        // Pick a connection slot via the AOSP-faithful
        // `ExclusiveConnection` selector. Same-thread nested calls
        // (server callback while a transact is in flight) re-enter the
        // slot already driven by this thread (the `DRIVING` marker);
        // otherwise we claim an available slot, or `wait` on `slot_cv`
        // if the pool is exhausted. Concurrent transacts on *other*
        // slots run unblocked.
        let oneway = (flags & FLAG_ONEWAY) != 0;
        let conn = self.find_conn();
        let transport = conn.transport();
        // AOSP `BinderNode::asyncNumber` (send side, per-remote-addr).
        let async_number = if oneway {
            self.shared
                .state
                .lock()
                .expect("rpc state poisoned")
                .next_send_async_number(addr)
        } else {
            0
        };
        let txn = WireTransaction {
            address: addr,
            code,
            flags,
            async_number,
            data: data.rpc_data_bytes().to_vec(),
            // Object table: the RPC-mode Parcel collects
            // binder (v2) / FD (v1+) positions during serialization;
            // empty on R34 / v0. Byte-identical to the pre-versioning
            // wire when empty.
            object_positions: data.rpc_object_positions().to_vec(),
        };
        let frame = self.profile.codec().encode_transact(&txn)?;
        // Out-of-band fds collected while serializing the request
        // (empty unless `Unix` fd-mode).
        self.send_msg(transport, &frame, data.rpc_out_fds())?;
        if oneway {
            return Ok(None);
        }
        // Apply the configured reply deadline for the duration
        // of the reply wait only. `ReplyDeadlineGuard` clears the sticky
        // `SO_RCVTIMEO` on every exit (return / `?` / panic) so it never
        // leaks onto the next call or a later recv on this connection.
        let deadline = *self.shared.timeout.lock().expect("timeout poisoned");
        let _deadline_guard = ReplyDeadlineGuard::arm(transport, deadline)?;
        loop {
            let (frame, in_fds) = self.recv_msg(transport)?;
            match self.profile.codec().decode_message(&frame)? {
                WireMessage::Reply(WireReply {
                    status,
                    data,
                    object_positions,
                }) => {
                    if status != 0 {
                        return Err(StatusCode::from(status));
                    }
                    let mut reply = Parcel::from_vec(data);
                    reply.configure_rpc(
                        self.parcel_ops(),
                        self.fd_mode(),
                        self.records_fd_positions(),
                    );
                    reply.rpc_set_in_fds(in_fds);
                    // Install the wire object table (after configure_rpc
                    // sets RPC mode) so binder/FD reads can validate
                    // positions.
                    reply.rpc_set_object_positions(object_positions);
                    reply.set_data_position(0);
                    return Ok(Some(reply));
                }
                WireMessage::DecStrong(a) => {
                    self.shared
                        .state
                        .lock()
                        .expect("rpc state poisoned")
                        .dec_strong_local(&a);
                }
                WireMessage::Transact(t) => {
                    // Nested / re-entrant call: the peer is calling
                    // back into one of *our* objects while we wait for
                    // our own reply. Dispatch it inline on this call
                    // stack over the same connection (single thread per
                    // connection ⇒ correct FIFO nesting, no deadlock).
                    // The reply deadline is lifted for the
                    // (unbounded) nested dispatch and restored for the
                    // continued wait *symmetrically via Drop* — a `?` /
                    // panic out of `dispatch_transact` can no longer
                    // leave the timeout desynchronized.
                    let _restore = NestedDeadlineGuard::lift(transport, deadline)?;
                    let peer = transport.peer_identity();
                    self.dispatch_transact(t, in_fds, peer)?;
                }
            }
        }
    }

    /// Two-tier `DEC_STRONG` hand-off for
    /// `RpcProxy::drop`. Drop runs on arbitrary user threads that may
    /// not be driving a slot of this session — without this guard,
    /// `send_dec_strong`'s `find_conn` would `cv.wait` on slot
    /// availability and a hung peer would block the user's `Drop`
    /// indefinitely.
    ///
    /// **Fast path** (no contention): try a non-blocking slot acquire
    /// via [`try_find_conn`]. The default single-slot session in steady
    /// state has no contention, so this succeeds immediately and the
    /// send happens *synchronously* — preserving the byte-and-timing
    /// ordering callers rely on (drop → next ordered round-trip ⇒
    /// peer has processed DEC_STRONG before the next reply).
    ///
    /// **Slow path** (every slot busy): enqueue so Drop returns
    /// immediately. The dedicated reaper thread (spawned in
    /// [`with_shared`]) drains the queue and performs the blocking
    /// send off the user thread. AOSP `RpcSession`'s outgoing
    /// `sendDecStrongToTarget` is itself synchronous — this two-tier
    /// shape matches the *user-observable* behavior (sync when
    /// possible) without paying the hang risk in the contention case.
    ///
    /// Send failures are silent — same as the original
    /// `RpcProxy::drop` semantics (a dead session ⇒ peer
    /// observationally gone, AOSP parity).
    pub(crate) fn queue_dec_strong(&self, addr: RpcAddress) {
        if self.shared.lifecycle.is_torn_down() {
            return;
        }
        // Fast path: synchronous send when a slot is immediately
        // available. Preserves the FIFO observable timing.
        if let Some(conn) = self.try_find_conn() {
            let frame = self.profile.codec().encode_dec_strong(&addr);
            let _ = self.send_msg(conn.transport(), &frame, &[]);
            return;
        }
        // Slow path: pool is fully exclusive (every slot mid-transact
        // by a different thread). Hand off to the reaper.
        let _ = self.dec_strong_tx.send(addr);
    }

    pub(crate) fn send_dec_strong(&self, addr: RpcAddress) -> Result<()> {
        // `RpcProxy::drop` (best-effort) and `read_binder`'s
        // excess-flush call this from arbitrary threads/contexts.
        // Shutdown guard: once the session leaves `Live` (Dying or
        // Dead), the peer is gone *and* the slot pool has either
        // started shrinking via `remove_slot` or already emptied —
        // `find_conn` on an empty pool would `cv.wait` forever (no
        // `add_slot` can race past `try_bump_live_conns`). Skip
        // best-effort. The typed lifecycle makes this check observe the
        // `Dying` window *before* the obituary completes, closing a
        // narrow window where this path could find an empty pool after
        // the founding worker started teardown.
        if self.shared.lifecycle.is_torn_down() {
            return Ok(());
        }
        // `find_conn` picks an available slot (or — if this thread is
        // already driving one of the session's slots — reuses it via
        // `DRIVING`, the documented "interleaved DEC_STRONG" path).
        let conn = self.find_conn();
        let frame = self.profile.codec().encode_dec_strong(&addr);
        self.send_msg(conn.transport(), &frame, &[])?;
        Ok(())
    }

    pub(crate) fn forget_remote_if(&self, addr: &RpcAddress, who: *const ()) {
        self.shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .forget_remote_if(addr, who);
    }

    /// Connection lost ⇒ every remote object on this session is dead:
    /// fire `binder_died` on each cached proxy's recipients (AOSP
    /// `RpcState::sendObituaries`). The strong snapshot is gathered
    /// under the state lock, which is released **before** the
    /// callbacks, so a recipient may re-enter `unlink_to_death`
    /// without deadlocking (AOSP unlocks before the obituary loop).
    /// Each `send_obituary` is idempotent, so calling this more than
    /// once for a session (e.g. a transact already saw the close, then
    /// the serve loop ends) is harmless.
    pub(crate) fn send_session_obituaries(&self) {
        let snapshot = self
            .shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .remote_proxy_snapshot();
        for arc in snapshot {
            let Some(proxy) = arc.as_any().downcast_ref::<RpcProxy>() else {
                continue;
            };
            // `who` = the dying proxy's weak binder (kernel
            // `send_obituary(&WIBinder)` parity).
            let sib = SIBinder::from_arc(arc.clone());
            let who = SIBinder::downgrade(&sib);
            proxy.send_obituary(&who);
        }
    }

    /// Send a `REPLY` (status + parcel bytes + object table + any
    /// out-of-band fds). `object_positions` is the reply parcel's
    /// object table; empty for error / no-payload
    /// replies and on R34 / v0 (byte-identical to the pre-versioning wire).
    fn send_reply(
        &self,
        status: i32,
        data: &[u8],
        object_positions: &[u32],
        fds: &[OwnedFd],
    ) -> Result<()> {
        let frame = self.profile.codec().encode_reply(&WireReply {
            status,
            data: data.to_vec(),
            object_positions: object_positions.to_vec(),
        })?;
        // Reuse this thread's already-driven slot (the
        // inbound dispatch slot — `DRIVING` reentrant pin via
        // `find_conn`). For an outermost server reply
        // (`serve_once_on_slot` pinned the slot before dispatch) this
        // is the same slot the request arrived on.
        let conn = self.find_conn();
        Ok(self.send_msg(conn.transport(), &frame, fds)?)
    }

    /// Dispatch one inbound `TRANSACT` (server role, or a nested
    /// callback while a client call is in flight) and send its reply.
    /// Shared by [`RpcSessionInner::serve_once`] and the nested-call
    /// arm of [`RpcSessionInner::client_transact`].
    fn dispatch_transact(
        &self,
        t: WireTransaction,
        in_fds: Vec<OwnedFd>,
        peer: PeerIdentity,
    ) -> Result<()> {
        let oneway = (t.flags & FLAG_ONEWAY) != 0;
        if t.address.is_zero() {
            // Special zero-address transactions (GET_ROOT etc.) have no
            // caller identity — they never reach a user handler.
            return self.serve_special(&t, oneway);
        }
        // Resolve the caller's (uid, pid) once (Plan 2-16 Phase B). The
        // resolved pair is `Copy`, so the oneway drain reuses it without
        // cloning the (possibly `String`-bearing) `PeerIdentity` per entry.
        let caller = caller_ids(&peer);
        if oneway {
            self.dispatch_oneway_ordered(t, in_fds, caller)
        } else {
            self.execute_dispatched(t, in_fds, false, caller)
        }
    }

    /// Gate an inbound oneway through the target node's `asyncTodo`
    /// priority queue (AOSP `RpcState::processTransactInternal` enqueue and
    /// drain). The state lock is released before dispatch so a nested
    /// callback re-entry can reacquire it.
    fn dispatch_oneway_ordered(
        &self,
        t: WireTransaction,
        in_fds: Vec<OwnedFd>,
        caller: (u32, i32),
    ) -> Result<()> {
        let addr = t.address;
        let wire_async = t.async_number;
        let mut next = {
            let mut state = self.shared.state.lock().expect("rpc state poisoned");
            match state.dispatch_async_or_enqueue(addr, wire_async, t, in_fds) {
                super::state::AsyncDecision::Dispatch(t, fds) => Some((t, fds)),
                super::state::AsyncDecision::Enqueued => {
                    log::trace!(
                        "RPC oneway parked: addr={:?} async#={} (out of order)",
                        addr,
                        wire_async
                    );
                    None
                }
                super::state::AsyncDecision::Drop(reason) => {
                    log::debug!(
                        "RPC oneway dropped: addr={:?} async#={} reason={:?}",
                        addr,
                        wire_async,
                        reason
                    );
                    None
                }
            }
        };
        while let Some((t, fds)) = next {
            // All replayed oneways belong to this session, so the caller
            // identity is the same for every drained entry (`Copy`, no clone).
            self.execute_dispatched(t, fds, true, caller)?;
            next = {
                let mut state = self.shared.state.lock().expect("rpc state poisoned");
                state.advance_and_pop_async(addr)
            };
        }
        Ok(())
    }

    /// Run the local dispatch (lookup target + INTERFACE/PING shortcut
    /// for twoway + `rpc_transact` + reply / oneway-log). Shared body
    /// of both the twoway and oneway dispatch paths — the asyncTodo
    /// gating in `dispatch_oneway_ordered` is layered *above* this.
    fn execute_dispatched(
        &self,
        t: WireTransaction,
        in_fds: Vec<OwnedFd>,
        oneway: bool,
        caller: (u32, i32),
    ) -> Result<()> {
        let target = self
            .shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .lookup_local(&t.address);
        let Some(target) = target else {
            if oneway {
                // Best-effort drop; visible only if `dec_strong_local`
                // ran between the asyncTodo gate and now.
                log::debug!(
                    "RPC oneway to unknown/released address {:?} dropped",
                    t.address
                );
            } else {
                self.send_reply(StatusCode::DeadObject.into(), &[], &[], &[])?;
            }
            return Ok(());
        };

        // Standard binder control transactions that libbinder's
        // `BBinder::transact` answers *before* `onTransact`, sent with
        // **no interface token** (so they must bypass
        // `consume_rpc_interface_token`). The kernel `Binder` handles
        // these internally; the RPC server adapter must too, or a real
        // libbinder client can't e.g. `getInterfaceDescriptor()` (which
        // `AIBinder_associateClass` needs) or `ping`.
        if !oneway {
            match t.code {
                INTERFACE_TRANSACTION => {
                    let mut reply = Parcel::new();
                    reply.attach_rpc_ops(self.parcel_ops());
                    reply.write(&target.descriptor())?;
                    return self.send_reply(
                        0,
                        reply.rpc_data_bytes(),
                        reply.rpc_object_positions(),
                        &[],
                    );
                }
                PING_TRANSACTION => {
                    return self.send_reply(0, &[], &[], &[]);
                }
                _ => {}
            }
        }

        let mut reader = Parcel::from_vec(t.data);
        // The inbound *args* parcel must know it speaks the v1+ AOSP fd
        // body too (the reply paths already set this; the args path did
        // not — a v1+ fd *argument* would otherwise be read as the R34
        // `[present|idx]` legacy shape and desync). v1+ ⇒
        // `[not-null|hasComm|TYPE|idx]` + strict position read; R34/v0 ⇒
        // legacy, byte-unchanged.
        reader.configure_rpc(
            self.parcel_ops(),
            self.fd_mode(),
            self.records_fd_positions(),
        );
        reader.rpc_set_in_fds(in_fds);
        // Install the inbound wire object table (position validation);
        // empty on R34 / v0 / no-object.
        reader.rpc_set_object_positions(t.object_positions);
        reader.set_data_position(0);
        let mut reply = Parcel::new();
        reply.configure_rpc(
            self.parcel_ops(),
            self.fd_mode(),
            self.records_fd_positions(),
        );

        // Plan 2-16 Phase B: stamp the caller's uid/pid (resolved once in
        // `dispatch_transact` via `caller_ids`) into the RPC calling context
        // for the duration of the user handler so `get_calling_uid()` /
        // `get_calling_pid()` work over Unix RPC. The guard restores on
        // drop, so a nested re-entrant callback over the same connection
        // nests correctly.
        let (caller_uid, caller_pid) = caller;
        let result = consume_rpc_interface_token(&mut reader, target.descriptor()).and_then(|()| {
            let _calling = crate::thread_state::RpcCallingGuard::install(caller_uid, caller_pid);
            target.rpc_transact(t.code, &mut reader, &mut reply)
        });

        if oneway {
            if let Err(e) = result {
                log::error!("oneway RPC transaction failed (dropped): {e:?}");
            }
            return Ok(());
        }
        match result {
            Ok(()) => self.send_reply(
                0,
                reply.rpc_data_bytes(),
                reply.rpc_object_positions(),
                reply.rpc_out_fds(),
            ),
            Err(e) => self.send_reply(e.into(), &[], &[], &[]),
        }
    }

    /// Handle one inbound message on the pinned **slot** (a server
    /// worker drives a specific accepted connection's slot;
    /// nested outbound callbacks from the handler reuse this same slot
    /// via the `DRIVING` marker).
    /// `Ok(false)` ⇒ peer closed (stop).
    fn serve_once_on_slot(&self, slot_id: u64) -> Result<bool> {
        // The worker drives its own slot — pinning to it must succeed
        // (the slot was just added by `add_incoming_slot`/etc. and
        // hasn't been removed yet because `remove_slot` only runs from
        // this very call's `serve_blocking_on` exit). A `DeadObject`
        // here would be a structural bug, propagated so the worker exits.
        let conn = self.find_conn_pinned(slot_id)?;
        let transport = conn.transport();
        let (frame, in_fds) = match self.recv_msg(transport) {
            Ok(f) => f,
            Err(RpcError::PeerClosed) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        match self.profile.codec().decode_message(&frame)? {
            WireMessage::Transact(t) => {
                // Resolve the connecting peer's identity (Plan 2-16
                // Phase B) so the dispatch can stamp the calling uid/pid.
                let peer = transport.peer_identity();
                self.dispatch_transact(t, in_fds, peer)?;
                Ok(true)
            }
            WireMessage::DecStrong(a) => {
                self.shared
                    .state
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
    /// rsbinder `GET_FD_MODE` extension).
    fn serve_special(&self, t: &WireTransaction, oneway: bool) -> Result<()> {
        if oneway {
            // Special transactions are never oneway.
            return Ok(());
        }
        match SpecialTransaction::from_code(t.code) {
            Some(SpecialTransaction::GetRoot) => {
                let root = self.shared.root.lock().expect("root poisoned").clone();
                let mut reply = Parcel::new();
                reply.attach_rpc_ops(self.parcel_ops());
                // SIBinder::serialize → RPC branch → write_binder.
                match &root {
                    Some(b) => reply.write(b)?,
                    None => reply.write(&0i32)?,
                }
                // GET_ROOT carries a binder-in-parcel: at v2 its
                // position is in the object table.
                self.send_reply(0, reply.rpc_data_bytes(), reply.rpc_object_positions(), &[])
            }
            Some(SpecialTransaction::GetMaxThreads) => {
                let n = self.shared.max_threads.load(Ordering::SeqCst) as i32;
                let mut reply = Parcel::new();
                reply.write(&n)?;
                self.send_reply(0, reply.rpc_data_bytes(), &[], &[])
            }
            Some(SpecialTransaction::GetSessionId) => {
                // AOSP `RpcState` server replies `reply.writeByteVector(
                // session->mId)` and the libbinder client reads it with
                // `Parcel::readByteVector` — a 32-byte (`kSessionIdBytes`)
                // opaque id. rsbinder's `Vec<u8>`/`&[u8]` serializer is
                // the AIDL `byte[]` path (`i32 len` + packed bytes +
                // 4-pad) == libbinder `writeByteVector` byte-for-byte.
                // (Was a bare `i32` ⇒ libbinder `BAD_VALUE` — found by
                // the real-peer round-trip.)
                let mut reply = Parcel::new();
                reply.write(&self.shared.rpc_session_id.as_bytes()[..])?;
                self.send_reply(0, reply.rpc_data_bytes(), &[], &[])
            }
            Some(SpecialTransaction::GetFdMode) => {
                // Body: i32 — does the client want `Unix`. Agree only
                // if this endpoint also supports it (else `None`, never
                // an error). The reply (0=None,1=Unix) is sent
                // in the *current* (None) mode; both sides switch only
                // after this exchange completes, so framing stays
                // consistent.
                let mut req = Parcel::from_vec(t.data.clone());
                req.set_data_position(0);
                // A malformed body safely defaults to "no FD support"
                // (never an error), but log the protocol violation
                // rather than swallow it silently.
                let want_unix = match req.read::<i32>() {
                    Ok(v) => v == 1,
                    Err(e) => {
                        log::debug!("RPC GET_FD_MODE: malformed body ({e:?}); defaulting to None");
                        false
                    }
                };
                let agreed = if want_unix && self.shared.fd_unix_supported.load(Ordering::SeqCst) {
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
                self.send_reply(0, reply.rpc_data_bytes(), &[], &[])?;
                // Switch AFTER the reply is on the wire (None-mode).
                *self.shared.fd_mode.lock().expect("fd_mode poisoned") = agreed;
                Ok(())
            }
            None => self.send_reply(StatusCode::UnknownTransaction.into(), &[], &[], &[]),
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
    /// Returns `Result` so a `getrandom` failure surfaces as
    /// `RpcError::Io` instead of panicking out of an infallible
    /// constructor. The only realistic failure path is early-boot
    /// containers without a working CSPRNG.
    pub fn new(transport: Box<dyn RpcTransport>, space: AddressSpace) -> RpcResult<RpcSession> {
        // Default = android-12 r34, byte-unchanged.
        RpcSession::with_profile(transport, space, WireProfile::R34(R34Codec))
    }

    /// Build a session over a connected transport with an explicit wire
    /// profile. The android-13+ codec is finalized by the handshake
    /// *before* this is called, so the profile is immutable for the
    /// session's lifetime (no interior mutability).
    fn with_profile(
        transport: Box<dyn RpcTransport>,
        space: AddressSpace,
        profile: WireProfile,
    ) -> RpcResult<RpcSession> {
        Ok(Self::with_shared(
            transport,
            profile,
            Self::fresh_shared(space)?,
        ))
    }

    /// A brand-new session's shared state (`lifecycle == Live(1)`, the
    /// founding connection). The default single-connection path uses
    /// exactly one of these, so its behavior is byte-identical to a
    /// single-`transport` `RpcSessionInner`.
    fn fresh_shared(space: AddressSpace) -> RpcResult<Arc<SharedSession>> {
        Ok(Arc::new(SharedSession {
            state: Mutex::new(RpcState::new(space)),
            root: Mutex::new(None),
            max_threads: AtomicU32::new(1),
            negotiated: AtomicU32::new(0),
            timeout: Mutex::new(None),
            fd_mode: Mutex::new(FileDescriptorTransportMode::None),
            fd_unix_supported: AtomicBool::new(false),
            rpc_session_id: gen_rpc_session_id()?,
            lifecycle: SessionLifecycle::new(),
        }))
    }

    /// Wrap `transport` as a connection of an **existing**
    /// [`SharedSession`] (the server's id-demux attaches a
    /// 2nd+ connection here instead of minting a fresh session, so a
    /// binder published over another connection is reachable — shared
    /// `state`/`root`/`rpc_session_id`). Bumps the session's live
    /// connection count. [`with_profile`](RpcSession::with_profile)
    /// is exactly this with a brand-new `SharedSession`
    /// (`live_conns == 1`) ⇒ the default single-connection path is
    /// byte-identical.
    fn with_shared(
        transport: Box<dyn RpcTransport>,
        profile: WireProfile,
        shared: Arc<SharedSession>,
    ) -> RpcSession {
        // The founding slot of this session's pool. id=1
        // (slot ids are monotonic from 1; 0 is reserved as a "no slot"
        // sentinel for future use). Default single-connection sessions
        // never add more slots ⇒ `find_conn` is a no-wait single-slot
        // pick ⇒ `enter_connection` byte-identical. The slot
        // owns its transport via `Arc<dyn RpcTransport>` — see
        // [`ConnSlot::transport`] for why.
        let founding = ConnSlot {
            transport: Arc::from(transport),
            exclusive_tid: None,
            id: 1,
        };
        let (dec_strong_tx, dec_strong_rx) = mpsc::channel();
        let inner = Arc::new(RpcSessionInner {
            conn_state: Mutex::new(ConnState {
                slots: vec![founding],
                next_slot_id: 2,
            }),
            slot_cv: Condvar::new(),
            profile,
            self_weak: Mutex::new(Weak::new()),
            shared,
            dec_strong_tx,
        });
        *inner.self_weak.lock().expect("self_weak") = Arc::downgrade(&inner);
        // Reaper thread for non-blocking `DEC_STRONG` from
        // `RpcProxy::drop`. Detached — `with_shared` is on the user
        // thread, so we never block its return; inner drop closes the
        // channel and the reaper exits naturally.
        let weak_for_reaper = Arc::downgrade(&inner);
        let _ = std::thread::Builder::new()
            .name("rsbinder-rpc-reaper".into())
            .spawn(move || reaper_loop(weak_for_reaper, dec_strong_rx));
        RpcSession { inner }
    }

    /// Id of the founding (first) slot. All non-attach
    /// `serve_blocking` callers (the default single-connection path)
    /// drive this slot.
    pub(crate) const FOUNDING_SLOT_ID: u64 = 1;

    /// Server-side unification: adds a freshly-
    /// accepted `transport` as a new incoming slot of this session's
    /// pool. The unified-model server attach arm calls this on the
    /// *founding* `Arc<RpcSessionInner>` (resolved from its 32-byte
    /// session id) instead of building a new `RpcSessionInner` sharing
    /// a `SharedSession` — so `state.remote_proxies`-cached `RpcProxy`s
    /// all point to the *single* session inner and a server worker's
    /// nested `proxy.transact` `find_conn`s stay within its own slot
    /// pool (no cross-slot aliasing). Bumps `live_conns`
    /// via the anti-resurrection primitive — returns
    /// `Err(StatusCode::DeadObject)` when the session is already torn
    /// down (obituary already fired) so the caller rejects the attach
    /// instead of silently resurrecting a dead session.
    pub(crate) fn add_incoming_slot(&self, transport: Box<dyn RpcTransport>) -> Result<u64> {
        if !self.inner.shared.try_bump_live_conns() {
            return Err(StatusCode::DeadObject);
        }
        Ok(self.inner.add_slot_inner(transport))
    }

    /// Append a server-side *callback* slot — the wire mirror of the
    /// peer's `mIncoming` (AOSP `RpcServer.cpp`: `addOutgoingConnection
    /// (client, init=true)` for `incoming` headers). Does NOT bump
    /// the lifecycle count (callback slots are not serve-driven; AOSP
    /// also does not gate session lifetime on `mOutgoing.size()`).
    /// `Err(DeadObject)` if the session is already torn down (the
    /// lifecycle covers both `Dying` and `Dead` in a single check).
    pub(crate) fn add_callback_slot(&self, transport: Box<dyn RpcTransport>) -> Result<u64> {
        // Snapshot gate, not CAS — a concurrent founding death after
        // this read is race-acceptable: the slot sits unused in a
        // dead session and is reclaimed via `Arc<RpcSessionInner>`.
        if self.inner.shared.lifecycle.is_torn_down() {
            return Err(StatusCode::DeadObject);
        }
        Ok(self.inner.add_slot_inner(transport))
    }

    /// This session's full inner state — including
    /// the *connection slot pool* and the wire profile, not just
    /// [`SharedSession`]. The unified-model server attach path stores
    /// `Weak` of this in `RpcServer.sessions` so an id-echoing 2nd+
    /// connection [`add_incoming_slot`](RpcSession::add_incoming_slot)s
    /// onto the founding inner — a single `RpcSessionInner` per
    /// session.
    pub(crate) fn inner_arc(&self) -> Arc<RpcSessionInner> {
        Arc::clone(&self.inner)
    }

    /// Wrap a resolved founding inner (from
    /// `RpcServer.sessions`) as an `RpcSession` so the attaching server
    /// worker can call [`serve_blocking_on`](RpcSession::serve_blocking_on)
    /// — symmetric to the founding worker's API surface.
    pub(crate) fn wrap_inner(inner: Arc<RpcSessionInner>) -> Self {
        RpcSession { inner }
    }

    /// Server: run **only** the android-13+ accept
    /// handshake on `transport`, returning the transport (unconsumed)
    /// plus the negotiated codec, the client's requested FD mode, and
    /// the client-supplied `session_id`. Splitting the handshake from
    /// the session build is what lets the server inspect the id and decide
    /// **new vs. attach** *before* committing the connection to a
    /// `SharedSession`.
    pub(crate) fn android13plus_accept_handshake(
        transport: Box<dyn RpcTransport>,
        server_max_version: u32,
    ) -> Result<Android13PlusAccept> {
        let (codec, client_fd_mode, client_id, incoming) = {
            let mut io = RawTransportIo(transport.as_ref());
            server_accept(&mut io, server_max_version).map_err(StatusCode::from)?
        };
        Ok((transport, codec, client_fd_mode, client_id, incoming))
    }

    /// Server: build the accepted connection's session from
    /// a completed [`android13plus_accept_handshake`](RpcSession::android13plus_accept_handshake).
    /// `shared = None` ⇒ a brand-new session (the default / new-session
    /// path); `shared = Some(existing)` ⇒
    /// **attach** this connection to a pre-existing session (id-demux),
    /// so a binder published over the founding connection is reachable
    /// here (shared `state`/`root`).
    ///
    /// **Anti-resurrection contract**: the caller MUST gate a
    /// `Some(shared)` attach through
    /// [`SharedSession::try_bump_live_conns`] *before* invoking this
    /// function and reject the connection on `false`. Once
    /// `live_conns` has been bumped this function takes ownership of
    /// the bump for the session lifetime (the `serve_blocking_on` exit
    /// hook does the matching `fetch_sub`). This split lets the
    /// server's `serve_connection` decide reject-vs-attach atomically
    /// against the race window between `resolve_session.upgrade()` and
    /// the founding worker's `live_conns.fetch_sub` (the resurrection
    /// race).
    pub(crate) fn from_android13plus(
        transport: Box<dyn RpcTransport>,
        codec: Android13PlusCodec,
        client_fd_mode: u8,
        server_fd_unix: bool,
        shared: Option<Arc<SharedSession>>,
    ) -> RpcResult<RpcSession> {
        let negotiated = codec.version();
        let shared = match shared {
            Some(s) => s,
            None => Self::fresh_shared(AddressSpace::Acceptor)?,
        };
        let session = Self::with_shared(transport, WireProfile::Android13Plus(codec), shared);
        if server_fd_unix && client_fd_mode == FD_MODE_UNIX && negotiated >= PROTOCOL_V1 {
            *session
                .inner
                .shared
                .fd_mode
                .lock()
                .expect("fd_mode poisoned") = FileDescriptorTransportMode::Unix;
        }
        Ok(session)
    }

    /// Client role, **opt-in android-13+ versioned wire**.
    /// Runs the AOSP connection handshake on `transport`
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
        Self::connect_android13plus_fd(transport, max_version, FileDescriptorTransportMode::None)
    }

    /// Client role, opt-in android-13+ wire **with FD-over-RPC**.
    /// Requests `fd_mode` in the
    /// `RpcConnectionHeader.fileDescriptorTransportMode` byte (byte-exact
    /// to AOSP `setFileDescriptorTransportMode`/`setupClient`, **not**
    /// the R34 `GET_FD_MODE` special-transact) and, on a successful
    /// handshake at **v1+** (android-14/15/16; v0 category-forbids fd,
    /// AOSP-faithful), switches the session to `Unix`.
    /// `FileDescriptorTransportMode::None` is exactly
    /// [`RpcSession::connect_android13plus`] (byte-identical no-FD path).
    pub fn connect_android13plus_fd(
        transport: Box<dyn RpcTransport>,
        max_version: u32,
        fd_mode: FileDescriptorTransportMode,
    ) -> Result<RpcSession> {
        // Empty id ⇒ request a new session — byte-identical to the
        // single-connection client handshake.
        Self::connect_android13plus_fd_with_id(transport, max_version, fd_mode, &[])
    }

    /// Identical to
    /// `connect_android13plus_fd` but echoes a server-minted 32-byte
    /// `session_id` in the `RpcConnectionHeader` (AOSP
    /// `RpcSession::setupClient`: the first connection sends an empty id
    /// and reads the server-minted one via
    /// [`RpcSession::get_session_id`], the remaining connections echo
    /// it). An **empty** `session_id` is byte-for-byte identical to
    /// `connect_android13plus_fd` (additive — the default path is
    /// unchanged). This wires + exercises the id round-trip and the
    /// server's accept-decision routing.
    pub fn connect_android13plus_fd_with_id(
        transport: Box<dyn RpcTransport>,
        max_version: u32,
        fd_mode: FileDescriptorTransportMode,
        session_id: &[u8],
    ) -> Result<RpcSession> {
        // AOSP `kSessionIdBytes == 32`: only empty (new-session) or
        // exactly 32 bytes are wire-legal. Validate here so a
        // misbehaving caller can't trigger the silent `as u16` length-
        // field truncation in `encode_connection_header` for a 64 KiB+
        // buffer (the server would then read the declared size's worth
        // of bytes vs the actual appended bytes ⇒ wire desync).
        if !(session_id.is_empty() || session_id.len() == 32) {
            return Err(StatusCode::BadValue);
        }
        let want_unix = fd_mode == FileDescriptorTransportMode::Unix;
        let hdr_fd_mode = if want_unix {
            FD_MODE_UNIX
        } else {
            FD_MODE_NONE
        };
        let codec = {
            let mut io = RawTransportIo(transport.as_ref());
            client_connect_with_id(&mut io, max_version, false, hdr_fd_mode, session_id)
                .map_err(StatusCode::from)?
        };
        let negotiated = codec.version();
        let session = RpcSession::with_profile(
            transport,
            AddressSpace::Initiator,
            WireProfile::Android13Plus(codec),
        )
        .map_err(StatusCode::from)?;
        // v0 (android-13) category-forbids fd-over-RPC; only commit to
        // `Unix` when the negotiated wire is v1+ (else stay `None` and
        // any fd write is the AOSP-faithful `BAD_TYPE` reject).
        if want_unix && negotiated >= PROTOCOL_V1 {
            *session
                .inner
                .shared
                .fd_mode
                .lock()
                .expect("fd_mode poisoned") = FileDescriptorTransportMode::Unix;
        }
        Ok(session)
    }

    /// Server role, **opt-in android-13+ versioned wire**. Runs
    /// the AOSP accept handshake on an already-accepted `transport`
    /// (negotiates `min(server_max_version, client_max)`), then returns
    /// an [`AddressSpace::Acceptor`] session speaking the negotiated
    /// version. Called by [`super::RpcServer`] on its worker thread (the
    /// handshake is blocking I/O on the accepted socket). Keeps the
    /// no-FD scope (the client's FD-mode byte is read for wire fidelity
    /// but not acted on — use [`RpcSession::accept_android13plus_fd`]).
    pub fn accept_android13plus(
        transport: Box<dyn RpcTransport>,
        server_max_version: u32,
    ) -> Result<RpcSession> {
        Self::accept_android13plus_fd(transport, server_max_version, false)
    }

    /// Server role, opt-in android-13+ wire **with FD-over-RPC**,
    /// accepting one connection as a
    /// **brand-new session** (no id-demux). Reads the client's
    /// requested FD mode from the `RpcConnectionHeader` and, when the
    /// client asked for `Unix`, this server opted in (`server_fd_unix`,
    /// [`super::RpcServer::set_supported_fd_modes`]), **and** the
    /// negotiated wire is v1+ (v0 forbids fd), switches the session to
    /// `Unix`. Lenient: a client/server FD-mode mismatch degrades to
    /// `None` (the fd write then `BAD_TYPE`-rejects) rather than AOSP's
    /// hard session-reject. `server_fd_unix == false` is exactly
    /// [`RpcSession::accept_android13plus`] (byte-identical no-FD path).
    ///
    /// This is a thin convenience wrapper over
    /// `android13plus_accept_handshake`
    /// then `from_android13plus` with
    /// `shared = None` (the client-supplied id is ignored). The
    /// multi-connection id-demux (new vs. attach) lives in
    /// [`super::RpcServer::serve_connection`], which calls the split
    /// handshake/build helpers directly; existing single-connection
    /// callers keep the byte-identical shape here.
    pub fn accept_android13plus_fd(
        transport: Box<dyn RpcTransport>,
        server_max_version: u32,
        server_fd_unix: bool,
    ) -> Result<RpcSession> {
        let (transport, codec, client_fd_mode, _client_id, incoming) =
            Self::android13plus_accept_handshake(transport, server_max_version)?;
        // This wrapper has no callback-slot path; incoming-direction
        // attaches go through `super::RpcServer::serve_connection`.
        if incoming {
            return Err(StatusCode::BadType);
        }
        Self::from_android13plus(transport, codec, client_fd_mode, server_fd_unix, None)
            .map_err(StatusCode::from)
    }

    /// The negotiated android-13+ wire protocol version
    /// (`0` = android-13, `1` = android-14/15), or `None` for the
    /// default android-12 r34 profile. Lets a caller assert the
    /// `min(client_max, server_max)` handshake outcome.
    pub fn wire_protocol_version(&self) -> Option<u32> {
        match &self.inner.profile {
            WireProfile::Android13Plus(c) => Some(c.version()),
            WireProfile::R34(_) => None,
        }
    }

    /// This session's opaque 32-byte id (AOSP `RpcSession::mId`,
    /// `kSessionIdBytes == 32`). On the server side this is the id
    /// minted at session build and replied by the `GET_SESSION_ID`
    /// special transact; the multi-connection path uses it as the
    /// [`super::RpcServer`] registry key. Per-session, never global.
    pub fn session_id(&self) -> [u8; 32] {
        *self.inner.shared.rpc_session_id.as_bytes()
    }

    /// Client: fetch the server-minted 32-byte
    /// session id via the `GET_SESSION_ID` special transact. AOSP
    /// `RpcSession::setupClient` reads this on the first connection and
    /// echoes it on the remaining ones
    /// ([`RpcSession::setup_unix_client_android13plus_with_id`]). The
    /// server already replies it (real-peer-validated:
    /// `writeByteVector(mId)` == the AIDL `byte[]` path); this is the
    /// missing *client* half.
    pub fn get_session_id(&self) -> Result<Vec<u8>> {
        let data = Parcel::new();
        let mut reply = self
            .inner
            .client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetSessionId.code(),
                &data,
                0,
            )?
            .ok_or(StatusCode::UnexpectedNull)?;
        // Propagate the parcel-read error as-is (BadValue/BadType from
        // a malformed wire byte vector is informationally distinct from
        // a missing reply — squashing to UnexpectedNull would lose that
        // diagnosis signal in logs).
        reply.read::<Vec<u8>>()
    }

    /// Server role: advertise that this endpoint will accept the
    /// `Unix` FD-over-RPC mode on `GET_FD_MODE`. Default
    /// is *not* advertised, so the categorical FD reject is the default
    /// everywhere. Has no effect on a non-UDS transport (the transport
    /// fd methods reject by type regardless).
    pub fn set_supported_fd_modes(&self, modes: &[FileDescriptorTransportMode]) {
        let unix = modes.contains(&FileDescriptorTransportMode::Unix);
        self.inner
            .shared
            .fd_unix_supported
            .store(unix, Ordering::SeqCst);
    }

    /// Client role: negotiate the FD-over-RPC mode.
    /// Sends exactly one `GET_FD_MODE` packet; the agreed mode is
    /// `Unix` iff *both* peers opted in, else `None` (never an error).
    /// Must be called before any FD-bearing call, like
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
        *self.inner.shared.fd_mode.lock().expect("fd_mode poisoned") = agreed;
        Ok(agreed)
    }

    /// The negotiated FD-over-RPC mode (default `None`).
    pub fn fd_transport_mode(&self) -> FileDescriptorTransportMode {
        self.inner.fd_mode()
    }

    /// Publish the server's root object (returned by `get_root`).
    pub fn set_root(&self, binder: SIBinder) {
        *self.inner.shared.root.lock().expect("root poisoned") = Some(binder);
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
    ///
    /// When the loop ends — peer closed (clean) **or** a fatal serve
    /// error — every remote object reachable over this session is dead,
    /// so registered death recipients are fired here (AOSP
    /// `RpcState::sendObituaries` when a session's incoming threads
    /// end). This is the rsbinder death-detection point: a peer that
    /// linked a `DeathRecipient` (e.g. a client wanting to learn the
    /// server died) must be running this serve loop — faithful to
    /// AOSP's `getMaxIncomingThreads() >= 1` requirement for an RPC
    /// `linkToDeath`.
    pub fn serve_blocking(&self) -> Result<()> {
        self.serve_blocking_on(Self::FOUNDING_SLOT_ID)
    }

    /// Serve a *specific* slot of the pool until peer
    /// closes (the server worker's API — each accepted connection's
    /// worker drives the slot it was added as via
    /// `add_incoming_slot`). The
    /// default single-connection
    /// [`serve_blocking`](RpcSession::serve_blocking) is exactly this
    /// on the founding slot (`FOUNDING_SLOT_ID`).
    pub fn serve_blocking_on(&self, slot_id: u64) -> Result<()> {
        let result = {
            let mut r = Ok(());
            loop {
                match self.inner.serve_once_on_slot(slot_id) {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(e) => {
                        r = Err(e);
                        break;
                    }
                }
            }
            r
        };
        // Typed lifecycle: this
        // connection is finished. Fire the session obituaries only on
        // the **last** connection's teardown (full session death) —
        // never on a *partial* connection loss while other connections
        // of the same session are still live (that would deliver a
        // spurious `binder_died` to a peer that can still reach the
        // session over another connection). `drop_connection` returns
        // `true` for exactly the one caller observing the 1→0 edge
        // (Live(1) → Dying); on `false` (Live(n>1) → Live(n-1)) other
        // workers still drive the session. After firing, transition
        // Dying → Dead via `mark_dead` so subsequent attach attempts
        // and best-effort `dec_strong` calls see a settled state.
        if self.inner.shared.lifecycle.drop_connection() {
            self.inner.send_session_obituaries();
            self.inner.shared.lifecycle.mark_dead();
        }
        // Drop this worker's slot from the pool **after**
        // the lifecycle transition + obituary so a concurrent
        // `RpcProxy::drop`'s best-effort `send_dec_strong` sees either
        // (i) the slot still present (`find_conn` picks it, send returns
        // `PeerClosed`, best-effort path Err — no deadlock) or (ii) the
        // lifecycle in Dying/Dead (the `send_dec_strong` early-out
        // short-circuits, skipping `find_conn` entirely). Removing the
        // slot *before* the lifecycle transition opened a window where a
        // stale proxy drop would find an empty pool and block forever on
        // `slot_cv` (no add_slot can race the obituary thanks to
        // `try_bump_live_conns`).
        self.inner.remove_slot(slot_id);
        result
    }

    /// Internal: set this session's advertised max-threads value
    /// (server role). Called by [`super::RpcServer::configure_session`]
    /// per accepted connection — external callers go through
    /// [`super::RpcServer::set_max_threads`], which owns the public
    /// advertise/slot-cap contract.
    ///
    /// Crate-private since the only caller is the server itself — there
    /// is no use case for a user-constructed `RpcSession` (always client
    /// side via `setup_unix_client*` or `from_preconnected_fd`) to set
    /// the server-only `GET_MAX_THREADS` advertise.
    pub(crate) fn set_max_threads(&self, n: u32) {
        self.inner
            .shared
            .max_threads
            .store(n.max(1), Ordering::SeqCst);
    }

    /// Set the client reply/handshake wait deadline. `None`
    /// (default) blocks forever.
    pub fn set_timeout(&self, timeout: Option<Duration>) {
        *self.inner.shared.timeout.lock().expect("timeout poisoned") = timeout;
    }

    /// `min(local, remote)` worker count established by
    /// [`RpcSession::negotiate`] (0 if not negotiated).
    pub fn negotiated_max_threads(&self) -> u32 {
        self.inner.shared.negotiated.load(Ordering::SeqCst)
    }

    /// Client role: exchange `GET_MAX_THREADS` with the server and
    /// record `min(local_max, remote_max)` (android
    /// `getRemoteMaxThreads`). Exactly one negotiation packet.
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
        self.inner
            .shared
            .negotiated
            .store(negotiated, Ordering::SeqCst);
        Ok(negotiated)
    }

    /// Client: connect to a Unix-domain RPC server. Thread negotiation
    /// is a separate, explicit step ([`RpcSession::negotiate`]) so a
    /// caller that negotiates does so with exactly one packet.
    pub fn setup_unix_client(path: impl AsRef<std::path::Path>) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect(path)?;
        RpcSession::new(Box::new(t), AddressSpace::Initiator).map_err(StatusCode::from)
    }

    /// Client: connect to a Unix-domain RPC server speaking the
    /// **android-13+ versioned wire**. Connects
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

    /// Client: connect to a **TCP** RPC server over **TLS**,
    /// R34 wire. Establishes the TCP connection, completes the
    /// TLS handshake to `server_name` (verified per `config` — a
    /// bad/untrusted server certificate fails **here**, before any RPC
    /// payload byte is exchanged), then builds an R34 session. The
    /// android-13+ variant is
    /// [`setup_tcp_client_tls_android13plus`](RpcSession::setup_tcp_client_tls_android13plus).
    ///
    /// `config` is the caller's `rustls::ClientConfig` (roots / client
    /// cert / verification policy) — rsbinder never invents crypto.
    /// For a non-TCP stream (a preconnected `unix`/`vsock`
    /// fd) build the transport directly with
    /// [`TlsTransport::connect_stream`](super::transport::TlsTransport::connect_stream)
    /// and pass it to [`RpcSession::new`].
    #[cfg(feature = "rpc-tls")]
    pub fn setup_tcp_client_tls(
        addr: impl std::net::ToSocketAddrs,
        server_name: &str,
        config: std::sync::Arc<rustls::ClientConfig>,
    ) -> Result<RpcSession> {
        let tcp = std::net::TcpStream::connect(addr)?;
        let t = super::transport::TlsTransport::connect(tcp, server_name, config)
            .map_err(StatusCode::from)?;
        RpcSession::new(Box::new(t), AddressSpace::Initiator).map_err(StatusCode::from)
    }

    /// Client: connect to a **TCP** RPC server over **TLS** speaking the
    /// **android-13+ versioned wire**. TCP-connects,
    /// TLS-handshakes to `server_name` per `config` (a bad cert fails
    /// before any RPC byte), then runs the AOSP android-13+ handshake via
    /// [`RpcSession::connect_android13plus`] negotiating
    /// `min(max_version, server_max)`. The R34 variant is
    /// [`setup_tcp_client_tls`](RpcSession::setup_tcp_client_tls).
    #[cfg(feature = "rpc-tls")]
    pub fn setup_tcp_client_tls_android13plus(
        addr: impl std::net::ToSocketAddrs,
        server_name: &str,
        config: std::sync::Arc<rustls::ClientConfig>,
        max_version: u32,
    ) -> Result<RpcSession> {
        let tcp = std::net::TcpStream::connect(addr)?;
        let t = super::transport::TlsTransport::connect(tcp, server_name, config)
            .map_err(StatusCode::from)?;
        RpcSession::connect_android13plus(Box::new(t), max_version)
    }

    /// Client: connect to a Unix-domain
    /// android-13+ RPC server **echoing a server-minted 32-byte
    /// `session_id`**. Flow (AOSP `RpcSession::setupClient`): connect
    /// the first session with `setup_unix_client_android13plus`
    /// (empty id ⇒ new session), read its id with
    /// [`RpcSession::get_session_id`], then open the remaining
    /// connections here echoing that id. An **empty** `session_id` is
    /// byte-identical to `setup_unix_client_android13plus`.
    pub fn setup_unix_client_android13plus_with_id(
        path: impl AsRef<std::path::Path>,
        max_version: u32,
        session_id: &[u8],
    ) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect(path)?;
        RpcSession::connect_android13plus_fd_with_id(
            Box::new(t),
            max_version,
            FileDescriptorTransportMode::None,
            session_id,
        )
    }

    /// Client multi-outgoing: open one *additional*
    /// outgoing connection to the same android-13+ server session and
    /// add it as a new slot in this `RpcSession`'s pool (AOSP
    /// `RpcSession::setupClient` opens N outgoing; `findConnection`
    /// distributes outgoing calls across them). Returns the
    /// new slot id. `session_id` MUST be this session's server-minted
    /// id (`get_session_id()` on the founding connection) — the server
    /// id-demuxes the echo onto the same `SharedSession`, so
    /// state/root/proxies are shared with the founding connection.
    /// Profile uniformity is enforced: the additional connection's
    /// negotiated wire version must equal this session's (else error).
    ///
    /// The default single-connection sessions never call this ⇒ the
    /// pool stays at one slot ⇒ `find_conn` is byte-identical to the
    /// `enter_connection` path.
    pub fn add_outgoing_connection_android13plus(
        &self,
        path: impl AsRef<std::path::Path>,
        max_version: u32,
        session_id: &[u8],
    ) -> Result<u64> {
        // Profile uniformity (AOSP requires same version across a
        // session). Refuse R34 / a higher caller-supplied `max_version`
        // **before** the handshake so a mismatch doesn't burn a server
        // attach + roundtrip; the handshake then runs with the
        // session's exact version as its ceiling so the server can't
        // negotiate it down to something incompatible. AOSP
        // session-id wire constraint: `kSessionIdBytes == 32` (empty
        // is illegal here because `add_outgoing` is by definition a
        // 2nd+ connection echoing the founding id) — validate at the
        // entry to avoid the silent `as u16` length-field truncation
        // in `encode_connection_header` if a caller passed a 64 KiB+
        // garbage buffer.
        if session_id.len() != 32 {
            return Err(StatusCode::BadValue);
        }
        let session_version = match &self.inner.profile {
            WireProfile::Android13Plus(c) => c.version(),
            WireProfile::R34(_) => return Err(StatusCode::BadType),
        };
        let effective_max = max_version.min(session_version);
        let t = super::transport::UnixTransport::connect(path)?;
        let codec = {
            let mut io = RawTransportIo(&t);
            client_connect_with_id(&mut io, effective_max, false, FD_MODE_NONE, session_id)
                .map_err(StatusCode::from)?
        };
        if codec.version() != session_version {
            // Server negotiated below us (older peer than the founding
            // connection's negotiation). A mixed-version pool would
            // silently route incompatible wire across one
            // `RpcSessionInner`; refuse instead.
            return Err(StatusCode::BadType);
        }
        Ok(self.inner.add_outgoing_slot(Box::new(t)))
    }

    /// Automatic outgoing-pool fan-out.
    /// AOSP `RpcSession::setupClient` automation for the path-based
    /// UDS client (one helper instead of three explicit steps).
    ///
    /// Establishes the founding connection (a brand-new session, empty
    /// session id), runs `GET_MAX_THREADS` and `GET_SESSION_ID` against
    /// the server, then mints additional outgoing connections to the
    /// same `path` echoing the server-minted session id, up to
    /// `N = min(remote_max_threads, local_max_outgoing) - 1` extras.
    /// The returned `RpcSession` then has a pool of `N` connections,
    /// matching the size AOSP's
    /// [`RpcSession::setupClient`](https://cs.android.com/android/platform/superproject/main/+/main:frameworks/native/libs/binder/RpcSession.cpp;l=483)
    /// would build for the same `mMaxOutgoingConnections`.
    ///
    /// **`local_max_outgoing <= 1`** is the *single-connection* path:
    /// no `GET_MAX_THREADS` exchange, no fan-out, returned session is
    /// byte-identical to
    /// [`setup_unix_client_android13plus`](RpcSession::setup_unix_client_android13plus).
    /// A `0` is treated as `1` — a session must have at least the
    /// founding connection to be useful (AOSP rejects 0 as a misuse).
    ///
    /// **Profile uniformity** is enforced by the per-connection
    /// [`add_outgoing_connection_android13plus`](RpcSession::add_outgoing_connection_android13plus)
    /// (the founding session's negotiated wire version caps every
    /// additional connection's `max_version`). A fan-out connection
    /// that the server downgrades below the founding version surfaces
    /// as `Err(BadType)` and the partially-built session is dropped.
    /// Rust ownership ≡ AOSP `scope_guard`'s implicit cleanup.
    ///
    /// **No retry / no progressive degradation**: a fan-out connect
    /// failure (e.g. the server's `set_max_threads` is tighter than
    /// `local_max_outgoing - 1` would imply, so the attach is refused
    /// past the cap) surfaces as `Err`. A caller that wants a softer
    /// fallback can use
    /// [`setup_unix_client_android13plus`](RpcSession::setup_unix_client_android13plus) +
    /// manual `add_outgoing_connection_android13plus` loop and tolerate
    /// per-extra failures.
    pub fn setup_unix_client_android13plus_fan_out(
        path: impl AsRef<std::path::Path>,
        max_version: u32,
        local_max_outgoing: u32,
    ) -> Result<RpcSession> {
        // Borrow the path once for the founding connect; clone to
        // `PathBuf` to drive the fan-out loop without forcing
        // `impl AsRef<Path> + Clone` on the caller (each
        // `add_outgoing_connection_android13plus` takes a fresh
        // `impl AsRef<Path>`).
        let path: std::path::PathBuf = path.as_ref().to_owned();
        let session = Self::setup_unix_client_android13plus(&path, max_version)?;
        let local = local_max_outgoing.max(1);
        if local == 1 {
            // Single-connection: skip GET_MAX_THREADS entirely so the
            // wire is bit-identical to the founding-only helper.
            return Ok(session);
        }
        // `negotiate(local)` exchanges GET_MAX_THREADS, records the
        // `min(local, remote)` on the session, and returns that exact
        // value — which is AOSP `setupClient`'s `outgoingConnections`
        // by construction (it computes the same `min` then mints the
        // pool against it).
        let negotiated = session.negotiate(local)?;
        if negotiated <= 1 {
            return Ok(session);
        }
        let session_id = session.get_session_id()?;
        // `1..negotiated` ⇒ exactly `negotiated - 1` extras; the
        // founding connection counts as the first slot. AOSP loop:
        // `for (i = 0; i + 1 < outgoingConnections; i++)`.
        for _ in 1..negotiated {
            session.add_outgoing_connection_android13plus(&path, max_version, &session_id)?;
        }
        Ok(session)
    }

    /// Client: connect to a Unix-domain android-13+ RPC server **with
    /// FD-over-RPC** opt-in. UDS connect + the
    /// AOSP handshake requesting `fd_mode` in the connection header
    /// (see [`RpcSession::connect_android13plus_fd`]).
    /// `FileDescriptorTransportMode::None` ==
    /// [`RpcSession::setup_unix_client_android13plus`] (byte-identical).
    pub fn setup_unix_client_android13plus_fd(
        path: impl AsRef<std::path::Path>,
        max_version: u32,
        fd_mode: FileDescriptorTransportMode,
    ) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect(path)?;
        RpcSession::connect_android13plus_fd(Box::new(t), max_version, fd_mode)
    }

    /// Adopt a **preconnected** RPC socket fd handed
    /// to us by an out-of-band channel (the AOSP `IAccessor::addConnection`
    /// path: `BackendUnifiedServiceManager` receives a `unique_fd` and
    /// hands it to `RpcSession::setupPreconnectedClient(fd, request)`).
    ///
    /// The fd's address family (`SO_DOMAIN`) selects the rsbinder
    /// transport — `AF_UNIX` → [`super::transport::UnixTransport`],
    /// `AF_VSOCK` → `VsockTransport` (feature `rpc-vsock`, Linux only),
    /// `AF_INET`/`AF_INET6` → [`super::transport::TcpDebugTransport`]
    /// (feature `rpc-tcp-debug`). Any other family is rejected as
    /// [`StatusCode::BadType`], paralleling AOSP's
    /// `IAccessor::ERROR_UNSUPPORTED_SOCKET_FAMILY`. The handshake then
    /// runs through [`RpcSession::connect_android13plus_fd`] with
    /// `FileDescriptorTransportMode::None` (the fd carries no FD-mode
    /// metadata of its own — re-using the versioned wire bytes, neither
    /// a new codec nor a new framing path). `max_version` is the highest
    /// `RPC_WIRE_PROTOCOL_VERSION` to offer (`2` for android-16, `1` for
    /// android-14/15, `0` for android-13). The peer's `RpcServer`
    /// negotiates `min(max_version, server_max)` exactly as for the
    /// path-based client.
    ///
    /// rsbinder uses a single-connection session here, so no
    /// AOSP `request` reconnect closure is needed.
    pub fn from_preconnected_fd(fd: OwnedFd, max_version: u32) -> Result<RpcSession> {
        // (a) Determine the fd's address family. Linux exposes
        //     `SO_DOMAIN` directly (`rustix::sockopt::socket_domain`),
        //     but macOS has no equivalent — `getsockname()` works on
        //     both, returning the local address whose family is the
        //     socket's. For the Accessor path the fd is always
        //     `connect()`-ed by the server before being handed over, so
        //     it always has a local name (`socketpair` halves also do —
        //     `AF_UNIX` with an empty path).
        let local = rustix::net::getsockname(fd.as_fd())
            .map_err(|e| RpcError::from(std::io::Error::from(e)))?;
        let family = local.address_family();

        // (a') Clear `O_NONBLOCK`. AOSP `singleSocketConnection`
        // (frameworks/native/libs/binder/RpcSession.cpp:614, android-
        // 16.0.0_r4) opens its preconnected socket with
        // `SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK` and the same
        // fd is what `LocalAccessor::addConnection` returns to a client
        // — so an Accessor-supplied fd arrives non-blocking. rsbinder
        // RPC I/O is structurally blocking (the codec/handshake runs
        // `read`/`write_all` as synchronous calls; an EAGAIN surfaces
        // as `Io(WouldBlock)` mid-handshake and tears the connection
        // down). Clear `O_NONBLOCK` here so `UnixStream`/`TcpStream`
        // inherit blocking semantics. Re-applying the flag from
        // userspace is harmless if it wasn't set.
        let flags = rustix::fs::fcntl_getfl(fd.as_fd())
            .map_err(|e| RpcError::from(std::io::Error::from(e)))?;
        if flags.contains(rustix::fs::OFlags::NONBLOCK) {
            rustix::fs::fcntl_setfl(fd.as_fd(), flags - rustix::fs::OFlags::NONBLOCK)
                .map_err(|e| RpcError::from(std::io::Error::from(e)))?;
        }

        // (b) Map family → backend. Each branch is feature-gated
        //     identically to the transport `mod` declarations so the OFF
        //     build is byte-identical (a missing backend rejects with
        //     `BadType`, mirroring the AOSP `UNSUPPORTED_SOCKET_FAMILY`).
        let transport: Box<dyn RpcTransport> = match family {
            rustix::net::AddressFamily::UNIX => {
                Box::new(super::transport::UnixTransport::from_owned_fd(fd)?)
            }
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            rustix::net::AddressFamily::VSOCK => {
                Box::new(super::transport::VsockTransport::from_owned_fd(fd)?)
            }
            #[cfg(feature = "rpc-tcp-debug")]
            rustix::net::AddressFamily::INET | rustix::net::AddressFamily::INET6 => {
                Box::new(super::transport::TcpDebugTransport::from_owned_fd(fd)?)
            }
            _ => {
                log::warn!(
                    "RPC preconnected fd has unsupported socket family ({:?}); \
                     rejecting (AOSP IAccessor::ERROR_UNSUPPORTED_SOCKET_FAMILY)",
                    family.as_raw()
                );
                return Err(StatusCode::BadType);
            }
        };

        // (c) Run the android-13+ versioned handshake. No FD-over-RPC —
        //     the Accessor-fd carries no fd-mode metadata of its own and
        //     the consumer (the eventual proxy returned by `get_root`)
        //     drives any later FD passing through the negotiated wire
        //     directly.
        RpcSession::connect_android13plus_fd(
            transport,
            max_version,
            FileDescriptorTransportMode::None,
        )
    }

    /// Test/diagnostic: live local-node count (leak check).
    pub fn local_node_count(&self) -> usize {
        self.inner
            .shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .local_node_count()
    }
}

/// Reaper for `RpcProxy::drop`'s deferred
/// `DEC_STRONG` sends. Owns a [`Weak<RpcSessionInner>`] so it never
/// keeps the session alive; the inner's [`Drop`] closes the channel
/// and the reaper exits via `recv`'s `Err`. Drains any queued addrs
/// before exiting (mpsc preserves buffered items past sender drop).
///
/// Sends are best-effort — the original `RpcProxy::drop` semantics
/// (a dead session ⇒ silent no-op, AOSP parity) are preserved here.
fn reaper_loop(weak: Weak<RpcSessionInner>, rx: mpsc::Receiver<RpcAddress>) {
    while let Ok(addr) = rx.recv() {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        if inner.shared.lifecycle.is_torn_down() {
            continue;
        }
        // `find_conn_for_reaper` may briefly wait on `slot_cv` if the
        // pool is exhausted, but the *user* `Drop` already returned — this
        // wait is contained to the dedicated reaper thread and bails out
        // (None) if the session tears down or the pool drains while we
        // wait, so the reaper can never park forever holding the strong
        // `Arc`. A dead session ⇒ silent no-op DEC_STRONG (AOSP parity).
        let Some(conn) = inner.find_conn_for_reaper() else {
            drop(inner);
            continue;
        };
        let frame = inner.profile.codec().encode_dec_strong(&addr);
        let _ = inner.send_msg(conn.transport(), &frame, &[]);
        drop(conn);
        drop(inner);
    }
}

#[cfg(test)]
mod tests {
    //! Unit gate for [`RpcSession::from_preconnected_fd`]'s
    //! family-dispatch + `O_NONBLOCK` clear at the unit layer, without
    //! standing up an `RpcServer` (the end-to-end handshake against a
    //! peer is the `tests/rpc_accessor.rs` integration suite's job).
    //!
    //! Cross-platform host (Linux + macOS): every test uses
    //! `rustix::net::socketpair(AF_UNIX, ...)` so it's deterministic
    //! and filesystem-free.
    use super::*;
    use std::os::fd::{AsFd, OwnedFd};

    /// Build a Unix socketpair and return one half as `OwnedFd`.
    fn unix_socketpair_fd() -> (OwnedFd, OwnedFd) {
        use rustix::net::{AddressFamily, SocketFlags, SocketType};
        rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .expect("socketpair")
    }

    /// `from_preconnected_fd` on an `AF_UNIX` socketpair half: family
    /// dispatch hits the `UnixTransport` arm. The handshake itself
    /// can't complete without a real RPC peer on the other end — so
    /// pair the call with a peer that closes immediately and assert
    /// the *outcome* is a clean `Err`, never a panic / hang. This is
    /// the negative-path proof; the positive path (live handshake +
    /// echo) is `tests/rpc_accessor.rs`.
    #[test]
    fn from_preconnected_fd_unix_dispatches_then_fails_cleanly_on_eof() {
        let (a, b) = unix_socketpair_fd();
        // Close the peer end immediately: the v2 client handshake's
        // first read of `RpcNewSessionResponse` then hits EOF.
        drop(b);
        let err = match RpcSession::from_preconnected_fd(a, 2) {
            Ok(_) => panic!("expected Err on closed peer"),
            Err(e) => e,
        };
        // Wire failure must surface as a peer/io-class status, never
        // a panic or a hang.
        assert!(
            matches!(
                err,
                StatusCode::DeadObject | StatusCode::NotEnoughData | StatusCode::Unknown
            ),
            "unexpected status for closed peer: {err}"
        );
    }

    /// Regression gate (host-side): assert that the
    /// `O_NONBLOCK` clear actually runs — `from_preconnected_fd` must
    /// drop the flag *before* returning the session, so subsequent
    /// blocking reads on the underlying fd don't trip EAGAIN.
    ///
    /// Strategy: don't try to *observe* a stuck read (any synthetic
    /// EOF setup we craft hides EAGAIN behind it). Instead, after the
    /// bridge fails the handshake (no real peer), re-check the flag
    /// directly via `fcntl_getfl` on a dup of the fd. The fd is
    /// transferred to a transport on success, but on failure the
    /// transport drops and closes it. To get observable post-state,
    /// dup the fd *before* calling the bridge — the dup shares the
    /// same open-file description ([fcntl(2): "Each duplicate file
    /// descriptor refers to the same open file description and …
    /// the same file status flags"]), so the bridge's `fcntl_setfl`
    /// is reflected through our dup.
    #[test]
    fn from_preconnected_fd_clears_o_nonblock_before_dispatch() {
        use std::os::fd::IntoRawFd;
        let (a, _b) = unix_socketpair_fd();
        // Set O_NONBLOCK on the half we hand in — mirroring AOSP
        // `singleSocketConnection` (SOCK_NONBLOCK at socket creation).
        let flags = rustix::fs::fcntl_getfl(a.as_fd()).expect("getfl");
        rustix::fs::fcntl_setfl(a.as_fd(), flags | rustix::fs::OFlags::NONBLOCK)
            .expect("setfl NONBLOCK");
        assert!(
            rustix::fs::fcntl_getfl(a.as_fd())
                .unwrap()
                .contains(rustix::fs::OFlags::NONBLOCK),
            "test setup: O_NONBLOCK must be set"
        );
        // Dup BEFORE handing `a` to the bridge: both fds share the
        // same open-file description (status flags are shared per
        // POSIX), so the bridge's `fcntl_setfl` is observable
        // through `observer`.
        let observer = rustix::io::fcntl_dupfd_cloexec(a.as_fd(), 0).expect("dup");
        // _b is still alive, so the handshake's first read blocks;
        // we don't actually want it to succeed (no real peer), so
        // close _b mid-flight is unhelpful. Just race the bridge in
        // a background thread and shut the peer down so the bridge
        // returns; then observe the flag.
        let bridge_t = std::thread::spawn(move || {
            // The peer (_b in this scope) is still alive here, so the
            // bridge blocks reading the response. We rely on _b being
            // dropped at the end of the outer scope to unblock us.
            let _ = RpcSession::from_preconnected_fd(a, 2);
        });
        // Give the bridge a moment to clear the flag + start reading.
        std::thread::sleep(std::time::Duration::from_millis(50));
        // The bridge must have cleared O_NONBLOCK by now (the clear
        // happens BEFORE the family dispatch which is BEFORE the read).
        let observed = rustix::fs::fcntl_getfl(observer.as_fd()).expect("getfl observer");
        assert!(
            !observed.contains(rustix::fs::OFlags::NONBLOCK),
            "from_preconnected_fd did NOT clear O_NONBLOCK — handshake will trip EAGAIN \
             against a non-blocking peer fd from libbinder"
        );
        // Drop _b to unblock the bridge thread, then join.
        drop(_b);
        bridge_t.join().expect("bridge thread");
        // `observer` drops here; keep it explicit so the IntoRawFd
        // import isn't 'unused' if the helper closes are reordered.
        let _ = observer.into_raw_fd();
    }

    /// `getsockname` is the cross-platform family probe (Linux's
    /// `SO_DOMAIN` is absent on macOS). For an unconnected non-socket
    /// fd it errors out — assert that path is clean.
    #[test]
    fn from_preconnected_fd_rejects_non_socket_fd() {
        // `/dev/null` is a character device, never a socket — the
        // `getsockname()` syscall returns ENOTSOCK. Use rustix to open
        // it with no allocation footprint.
        let fd = rustix::fs::open(
            "/dev/null",
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .expect("open /dev/null");
        // ENOTSOCK round-trips through `rustix::io::Errno → io::Error
        // → StatusCode` — we don't care which variant rustix maps
        // ENOTSOCK to, only that the call fails cleanly without
        // panic, allocation, or peer wire I/O.
        assert!(
            RpcSession::from_preconnected_fd(fd, 2).is_err(),
            "non-socket fd must reject before any handshake I/O"
        );
    }
}
