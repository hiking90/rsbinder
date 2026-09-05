// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcSession` — single-connection RPC session driver.
//!
//! Ties one [`RpcTransport`] + `R34Codec` + per-session `RpcState`
//! together and provides:
//! * client outbound transactions ([`RpcSession::get_root`], and
//!   [`super::proxy::RpcProxy::transact`]),
//! * a blocking server serve loop ([`RpcSession::serve_blocking`]),
//! * the `RpcParcelOps` bridge that lets the `SIBinder`
//!   (de)serializers marshal binders as `RpcAddress`.
//!
//! All state is owned here (no global).

use std::cell::RefCell;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

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
    client_connect_with_id, read_aosp_message, read_aosp_message_with_fds,
    server_accept_deferred_init, write_aosp_message, write_aosp_message_with_fds,
    Android13PlusCodec, RawTransportIo, A13_ADDR_LEN, FD_MODE_NONE, FD_MODE_UNIX, PROTOCOL_V1,
    PROTOCOL_V2,
};
use super::{RpcError, RpcResult};

/// Result of the android-13+ server accept handshake: the unconsumed
/// transport plus the negotiated codec, the client's requested FD
/// mode, the client-supplied `session_id`, and the
/// `RPC_CONNECTION_OPTION_INCOMING` flag.
type Android13PlusAccept = (Box<dyn RpcTransport>, Android13PlusCodec, u8, Vec<u8>, bool);

enum RpcUnixAddr<'a> {
    Path(&'a Path),
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Abstract(&'a [u8]),
}

/// Unix-domain android-13+ RPC client configuration — the builder
/// consumed by
/// [`RpcSession::setup_unix_client_android13plus_with_config`],
/// [`RpcSession::add_outgoing_connection_android13plus_with_config`] and
/// [`RpcSession::add_incoming_connection_android13plus_with_config`].
///
/// One config expresses everything the per-shape convenience helpers
/// (`setup_unix_client_android13plus{,_abstract,_with_id,_fan_out}`)
/// take positionally: the address (filesystem path or Linux/Android
/// abstract name), the highest wire version to offer, and the optional
/// session-id attach / fan-out / incoming-connection / fd-transport-mode
/// knobs. The defaults (`session_id = empty`, `outgoing_connections = 1`,
/// `incoming_connections = 0`, `fd_mode = None`) reproduce the plain
/// single-connection
/// [`RpcSession::setup_unix_client_android13plus`] byte-for-byte.
///
/// `session_id` is mutually exclusive with both `outgoing_connections > 1`
/// and `incoming_connections > 0` (attaching joins a session someone else
/// owns; growing the pool is the owner's business) — the consuming setup
/// call rejects the combination with `BadValue`.
pub struct RpcUnixClientConfig<'a> {
    addr: RpcUnixAddr<'a>,
    max_version: u32,
    session_id: &'a [u8],
    outgoing_connections: u32,
    incoming_connections: u32,
    fd_mode: Option<FileDescriptorTransportMode>,
    timeout: Option<Duration>,
    handshake_timeout: Option<Duration>,
}

impl<'a> RpcUnixClientConfig<'a> {
    fn new(addr: RpcUnixAddr<'a>, max_version: u32) -> Self {
        Self {
            addr,
            max_version,
            session_id: &[],
            outgoing_connections: 1,
            incoming_connections: 0,
            fd_mode: None,
            timeout: None,
            handshake_timeout: None,
        }
    }

    /// Config for a filesystem-path Unix socket, offering at most wire
    /// version `max_version` in the handshake.
    pub fn path(path: &'a Path, max_version: u32) -> Self {
        Self::new(RpcUnixAddr::Path(path), max_version)
    }

    /// Config for a Linux/Android abstract Unix socket, offering at
    /// most wire version `max_version` in the handshake.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn abstract_name(name: &'a [u8], max_version: u32) -> Self {
        Self::new(RpcUnixAddr::Abstract(name), max_version)
    }

    /// Attach to an existing server session by echoing its 32-byte id
    /// (AOSP `RpcSession::setupClient` follow-up connections). Empty
    /// (the default) requests a brand-new session.
    pub fn session_id(mut self, session_id: &'a [u8]) -> Self {
        self.session_id = session_id;
        self
    }

    /// Ask for an `n`-connection outgoing pool (AOSP `setupClient`
    /// fan-out): the setup call negotiates `min(n, server max)` and
    /// opens that many connections. Default 1 = founding connection
    /// only, skipping negotiation entirely.
    pub fn outgoing_connections(mut self, n: u32) -> Self {
        self.outgoing_connections = n;
        self
    }

    /// Open `n` **incoming (callback) connections** in addition to the
    /// outgoing pool — AOSP `RpcSession::setMaxIncomingThreads(n)`.
    /// Each one is attached with the `INCOMING` header bit, added to
    /// the server's session as a slot the server *sends* on, and served
    /// here by a dedicated thread. Without at least one, the server can
    /// reach this client's callbacks only from inside a handler that is
    /// answering one of this client's calls (a nested call); a call
    /// from any other server thread — a timer, a worker, a oneway
    /// notification — fails with `FailedTransaction` on the server.
    ///
    /// Side effect: a session with an incoming connection detects the
    /// server's death as soon as the connection drops (obituaries fire
    /// from the serving thread), instead of on the next failed call.
    ///
    /// That detection is **eager, not precise**: the loss of the last
    /// incoming connection *is* this session's death, whatever the
    /// cause. A server that retires only that connection — its
    /// [`RpcServer::set_reply_timeout`](super::RpcServer::set_reply_timeout)
    /// elapsing on a slow callback handler, or a callback send that
    /// fails — therefore tears this whole session down: the founding
    /// connection is closed too, every proxy gets `binder_died`, and
    /// every local object the peer held is released, even though the
    /// peer is still up. Nothing on the wire separates the two cases.
    /// Size the server's reply timeout against the slowest legitimate
    /// handler, or open no incoming connection and let a failed call
    /// declare the death instead. (AOSP has no "lost only its callback
    /// connections" state to be faithful to: its client-side
    /// `WaitForShutdownListener::onSessionAllIncomingThreadsEnded` is a
    /// no-op, and an incoming thread that exits without a session
    /// shutdown aborts the process.)
    ///
    /// Requires the android-13+ profile (a session id), so it cannot be
    /// combined with [`session_id`](Self::session_id) (attaching to a
    /// session you do not own). Bounded on the server by twice its
    /// `RpcServer::set_max_threads` value. Default 0. The threads end
    /// when the server closes the session or on
    /// [`RpcSession::shutdown`]; dropping the `RpcSession` handle alone
    /// does not stop them.
    pub fn incoming_connections(mut self, n: u32) -> Self {
        self.incoming_connections = n;
        self
    }

    /// Request an fd transport mode in the connection header (AOSP
    /// `setFileDescriptorTransportMode`). Default is no fd support.
    pub fn fd_mode(mut self, mode: FileDescriptorTransportMode) -> Self {
        self.fd_mode = Some(mode);
        self
    }

    /// Apply [`RpcSession::set_timeout`] to the founding session **as
    /// soon as it exists**, so the deadline also bounds the round trips
    /// this setup itself performs (`GET_MAX_THREADS` for a fan-out,
    /// `GET_SESSION_ID` for any additional connection). Setting the
    /// timeout on the returned session instead leaves those unbounded: a
    /// peer that completes the handshake and then answers neither would
    /// block the caller forever. Default `None` (no deadline).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Deadline for each **connection handshake** this config performs —
    /// the founding connect and every fan-out / incoming attach.
    /// Unset (the default) blocks forever, so a peer that accepts the
    /// socket and then writes nothing hangs the setup call.
    ///
    /// Distinct from [`timeout`](Self::timeout), which is the session's
    /// *reply* deadline and cannot cover this phase: it is applied to the
    /// session, and the handshake runs before the session exists. This is
    /// the client-side counterpart of
    /// [`RpcServer::set_handshake_timeout`](super::RpcServer::set_handshake_timeout).
    ///
    /// `Duration::ZERO` is not a deadline: the setup call this config is
    /// passed to refuses it with [`StatusCode::BadValue`] rather than
    /// silently dropping the bound the caller asked for. Leave the option
    /// unset to wait indefinitely on purpose.
    pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = Some(timeout);
        self
    }

    fn connect(&self) -> Result<super::transport::UnixTransport> {
        match &self.addr {
            RpcUnixAddr::Path(path) => super::transport::UnixTransport::connect(path),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            RpcUnixAddr::Abstract(name) => super::transport::UnixTransport::connect_abstract(name),
        }
        .map_err(StatusCode::from)
    }
}

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
/// this is wire-correct against a real libbinder RPC peer for
/// **every** profile (r34 / android-13 v0 / v1 / android-16 v2). A
/// 3-int header here would be an rsbinder-ism that only round-trips
/// hermetically (rsbinder↔rsbinder, symmetric). RPC never touches
/// `thread_state`.
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

fn write_addr(p: &mut Parcel, addr: &RpcAddress) -> Result<()> {
    // 32 bytes, already 4-aligned (no padding) — matches the r34
    // Parcel RPC binder encoding (i32 present flag handled by caller).
    p.write_aligned_data(addr.as_wire_bytes().as_slice())
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

/// Normalize a caller-supplied wait deadline: `Some(Duration::ZERO)` is
/// rejected (logged as `msg`) and becomes `None`.
///
/// A zero deadline cannot be honored — `set_read_timeout` documents an
/// error for it — and an armed-then-failed deadline is a *post-send*
/// failure that has to retire the connection (see
/// [`ReplyDeadlineGuard`]). Refusing it at the setter keeps a mistaken
/// zero from turning every transaction on the session into a hard error.
pub(super) fn reject_zero_deadline(timeout: Option<Duration>, msg: &str) -> Option<Duration> {
    match timeout {
        Some(d) if d.is_zero() => {
            log::error!("{msg}");
            None
        }
        other => other,
    }
}

/// RAII save-and-restore for the `client_transact` reply read-deadline.
///
/// `set_read_timeout(Some(d))` sets a sticky `SO_RCVTIMEO` on the
/// shared connection. This guard restores the slot's **baseline** read
/// deadline on **every** exit from the reply wait — normal return,
/// `?`-propagation, or panic — so the reply deadline can never leak onto
/// the next `client_transact`, a nested inbound dispatch, or a
/// subsequent server-side `recv` on the same connection.
///
/// Restoring the baseline rather than clearing to `None` is what keeps a
/// callback issued from inside a handler — which reuses the serve
/// connection via the `DRIVING` pin — from permanently disabling that
/// connection's idle read deadline (the serve loop arms it once, not per
/// frame). See [`SharedSession::serve_read_deadline`].
struct ReplyDeadlineGuard<'a> {
    transport: &'a dyn RpcTransport,
    armed: bool,
    restore: Option<Duration>,
}

impl<'a> ReplyDeadlineGuard<'a> {
    fn arm(
        transport: &'a dyn RpcTransport,
        deadline: Option<Duration>,
        restore: Option<Duration>,
    ) -> RpcResult<Self> {
        let armed = deadline.is_some();
        if let Some(d) = deadline {
            transport.set_read_timeout(Some(d))?;
        }
        Ok(Self {
            transport,
            armed,
            restore,
        })
    }
}

impl Drop for ReplyDeadlineGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort: a failure to restore cannot be surfaced from
            // Drop, and the next caller re-arms/clears explicitly anyway.
            let _ = self.transport.set_read_timeout(self.restore);
        }
    }
}

/// Read deadline for a connect/attach **handshake**, cleared when the
/// handshake ends.
///
/// The handshake runs before any session exists, so
/// [`RpcSession::set_timeout`] cannot bound it — without this a peer that
/// accepts the socket and then writes nothing blocks the caller forever.
///
/// Clearing on drop is not optional. [`ReplyDeadlineGuard`] restores only a
/// deadline it armed itself, and it arms nothing when the session deadline
/// is `None` (the default), so a handshake deadline left on the socket
/// would silently bound every later `recv` on that slot — and on a client's
/// incoming (callback) slot it would break the serve loop outright.
struct HandshakeDeadline<'a> {
    transport: &'a dyn RpcTransport,
    armed: bool,
}

impl<'a> HandshakeDeadline<'a> {
    fn arm(transport: &'a dyn RpcTransport, deadline: Option<Duration>) -> RpcResult<Self> {
        // Every handshake deadline funnels through here, so this is the
        // one place that can promise a zero duration never reaches a
        // transport. `set_read_timeout` is documented to reject it, but
        // that is the *socket*'s promise, not the `RpcTransport` trait's:
        // an implementation free to accept `Some(ZERO)` would leave the
        // phase unbounded, which is worse than the deadline the caller
        // asked for. `None` is how "no deadline" is spelled.
        if deadline.is_some_and(|d| d.is_zero()) {
            log::error!(
                "rsbinder RPC: a zero handshake deadline is not a deadline — pass a positive \
                 duration, or `None` to wait indefinitely on purpose"
            );
            return Err(RpcError::Io(std::io::Error::from(
                std::io::ErrorKind::InvalidInput,
            )));
        }
        let armed = deadline.is_some();
        if armed {
            transport.set_read_timeout(deadline)?;
        }
        Ok(Self { transport, armed })
    }
}

/// A zero handshake deadline, rejected where the caller can still be
/// named. `HandshakeDeadline::arm` refuses it too, but only once the
/// connect has happened and only as a transport error; here it is the
/// `BadValue` every other invalid field of these builders yields.
pub(crate) fn reject_zero_handshake_timeout(timeout: Option<Duration>, setter: &str) -> Result<()> {
    if timeout.is_some_and(|d| d.is_zero()) {
        log::error!(
            "rsbinder RPC: {setter} was given a zero duration, which is not a deadline; pass a \
             positive duration, or leave it unset to wait indefinitely"
        );
        return Err(StatusCode::BadValue);
    }
    Ok(())
}

impl Drop for HandshakeDeadline<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort: a failure cannot be surfaced from `Drop`, and
            // the slot's next reader arms or clears explicitly.
            let _ = self.transport.set_read_timeout(None);
        }
    }
}

/// Confirm that a peer actually **admitted** an android-13+ *attach* —
/// a connection whose `RpcConnectionHeader` echoes a server-minted
/// `session_id` instead of requesting a new session.
///
/// The attach wire carries no server→client acknowledgement in the
/// outgoing direction: AOSP `RpcServer.cpp` writes an
/// `RpcNewSessionResponse` only for `requestingNewSession`, and the
/// `"cci"` of an outgoing connection flows client→server. A peer that
/// refuses the attach — unknown or stale session id, its
/// `set_max_threads` outgoing-slot cap already spent, shutdown, a
/// teardown race — can only close the socket. Without this probe the
/// refusal is invisible at attach time: the client keeps a dead
/// connection (a dead pool slot, or a whole dead session) and the
/// failure resurfaces much later, on whichever unrelated call
/// [`RpcSessionInner::find_conn`] happens to route onto that slot.
///
/// The probe is one ordinary `GET_SESSION_ID` special transact on the
/// fresh connection — the same round trip [`RpcSession::get_session_id`]
/// makes, which a real libbinder server answers on any connection — and
/// the reply must carry the very id we echoed: that is what proves the
/// peer put *this* connection into *that* session. The incoming
/// (callback) direction needs no probe: there the server writes `"cci"`
/// *after* admitting the connection, which is already the
/// acknowledgement (plan 2-20).
fn confirm_attach(
    transport: &dyn RpcTransport,
    codec: &Android13PlusCodec,
    session_id: &[u8],
) -> RpcResult<()> {
    let txn = WireTransaction {
        address: RpcAddress::zero(),
        code: SpecialTransaction::GetSessionId.code(),
        flags: 0,
        async_number: 0,
        data: Vec::new(),
        object_positions: Vec::new(),
    };
    let frame = codec.encode_transact(&txn)?;
    let mut io = RawTransportIo(transport);
    write_aosp_message(&mut io, &frame)?;
    let reply = read_aosp_message(&mut io)?;
    let peer_id = match codec.decode_message(&reply)? {
        WireMessage::Reply(WireReply {
            status: 0, data, ..
        }) => {
            let mut p = Parcel::from_vec(data);
            p.set_data_position(0);
            p.read::<Vec<u8>>()
                .map_err(|_| RpcError::Protocol("malformed GET_SESSION_ID reply on an attach"))?
        }
        _ => {
            return Err(RpcError::Protocol(
                "peer did not answer GET_SESSION_ID on an attach",
            ))
        }
    };
    if peer_id == session_id {
        Ok(())
    } else {
        Err(RpcError::Protocol(
            "peer put this attach in a different session than the id it echoed",
        ))
    }
}

/// Explain a failed attach, shared by the two attach entries so the
/// diagnosis does not drift between them.
fn log_attach_refused(e: &RpcError) {
    if matches!(e, RpcError::Timeout | RpcError::Truncated) {
        log::error!(
            "android-13+ RPC: the attach admission probe (GET_SESSION_ID) did not complete \
             ({e}) — a read deadline armed by this caller ends it this way too, so this is \
             not necessarily a refusal"
        );
        return;
    }
    log::error!(
        "android-13+ RPC: the peer refused this attach ({e}) — the session id is unknown or \
         stale, the peer's outgoing-slot cap (`set_max_threads`) is spent, or it is shutting \
         down. The id must be the server-minted one from `RpcSession::get_session_id()` (NOT \
         `session_id()`, which is a client-local value), and the connection count must stay \
         within `RpcSession::negotiate()`"
    );
}

/// Map an android-13+ **client** handshake failure to a [`StatusCode`].
/// A new-session handshake logs a hint first: an r34 (default profile)
/// peer is the likeliest cause and the returned status cannot say so.
fn client_handshake_err(e: RpcError, requesting_new_session: bool) -> StatusCode {
    if requesting_new_session {
        match &e {
            RpcError::PeerClosed => log::error!(
                "rsbinder RPC: the android-13+ handshake failed at the transport ({e}) after \
                 the peer accepted the connection — it may be speaking the r34 (default) \
                 profile. Connect without `?profile=android13plus`, or enable the android-13+ \
                 wire on the server (`RpcServer::set_android13plus`)"
            ),
            RpcError::Truncated => log::error!(
                "rsbinder RPC: the android-13+ handshake failed part-way through a response \
                 ({e}) — the peer may be speaking the r34 (default) profile, or a read \
                 deadline armed on this connection landed mid-frame. Connect without \
                 `?profile=android13plus`, or enable the android-13+ wire on the server \
                 (`RpcServer::set_android13plus`)"
            ),
            RpcError::Timeout => log::error!(
                "rsbinder RPC: the android-13+ handshake stalled and a read deadline armed on \
                 this connection elapsed — that deadline is the caller's own \
                 (`RpcUnixClientConfig::handshake_timeout`, the 10s \
                 `RpcSession::from_preconnected_fd` arms, or one set on the transport \
                 directly), so it may simply be shorter than this peer's legitimate response \
                 time. A peer that should have answered well within it may be speaking the \
                 r34 (default) profile instead"
            ),
            // The returned status drops the reason string, so this log
            // is the only description of the violation a caller gets.
            RpcError::Protocol(_) => log::error!(
                "rsbinder RPC: the android-13+ handshake failed ({e}) — either the peer's \
                 answer violated the wire or the caller offered a `max_version` this build \
                 does not implement"
            ),
            _ => {}
        }
    }
    StatusCode::from(e)
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

/// Which direction this endpoint uses a connection slot in — AOSP's
/// `RpcSession::mConnections.{mOutgoing, mIncoming}` split, kept as a
/// per-slot tag on the single pool (plan 2-20).
///
/// A non-nested outbound call (`client_transact` from a thread that is
/// not already driving a slot of this session) may only claim an
/// `Outgoing` slot: an `Incoming` slot is driven by a serve loop, and
/// its peer reads it only inside its own reply wait, so a transaction
/// written there from outside a dispatch would sit unread — and the
/// worker would be locked out of its own slot for the duration. A
/// nested call (the `DRIVING` reentrant pin) re-enters an `Outgoing`
/// slot unconditionally, but an `Incoming` one only while its dispatch
/// grants it ([`ConnSlot::allow_nested`]) — a nested call from a
/// *oneway* handler falls through to an `Outgoing` slot instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlotRole {
    /// This endpoint serves the connection (AOSP `mIncoming`): the
    /// server's founding slot and its id-echoing attaches, and a
    /// client's incoming (callback) connections.
    Incoming,
    /// This endpoint sends on the connection (AOSP `mOutgoing`): the
    /// client's founding slot and its fan-out, and the callback slots
    /// a server accepted from a client's incoming attaches.
    Outgoing,
}

/// What an outbound frame needs from a connection slot — AOSP
/// `RpcSession::ConnectionUse`. Decides whether the [`DRIVING`]
/// reentrant pin may be reused when the pinned slot is serve-driven.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConnUse {
    /// AOSP `ConnectionUse::CLIENT` — a transaction (twoway or oneway).
    /// May reuse a serve-driven slot only while that slot's
    /// [`ConnSlot::allow_nested`] holds; otherwise it needs an
    /// `Outgoing` slot, since nothing would read the frame.
    Client,
    /// AOSP `ConnectionUse::CLIENT_REFCOUNT` — a reply-less
    /// `DEC_STRONG`. Always free to ride the *pinned* slot ("we currently
    /// allow ref count calls to be nested (so that you can use this
    /// without having extra threads)") — that connection is one this
    /// thread already drives, so the peer is in its reply wait and reads
    /// it. The **scan** is `Outgoing`-only, exactly as for
    /// [`Client`](Self::Client): AOSP looks `mIncoming` up with
    /// `available = nullptr`, so a free serve-driven connection is never
    /// picked there, and writing to one can block rather than merely
    /// delay the frame.
    ClientRefcount,
    /// The reply to a transaction this thread is dispatching. It owes
    /// its answer to the slot the request arrived on and to no other,
    /// so it always takes the pin — AOSP replies on the
    /// `RpcConnection` object it is serving, never through
    /// `ExclusiveConnection::find` at all.
    Reply,
}

impl ConnUse {
    /// Whether this use opens a *new* exchange with the peer, and so is
    /// bound by the `Outgoing` / [`ConnSlot::allow_nested`] rules. A
    /// reply continues an exchange the peer is already waiting on; a
    /// `DEC_STRONG` opens none.
    fn is_new_transaction(self) -> bool {
        matches!(self, ConnUse::Client)
    }
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
    /// Direction this endpoint uses the slot in — see [`SlotRole`].
    /// `setMaxIncomingThreads` caps only `Incoming` slots; the
    /// callback-slot cap counts only `Outgoing` ones.
    role: SlotRole,
    /// AOSP `RpcConnection::allowNested`. `true` only while a **twoway**
    /// inbound transaction dispatched on this slot runs its handler:
    /// the peer is then parked in its own reply wait on this socket, so
    /// it will read whatever the handler writes back. `false` during a
    /// oneway dispatch (the peer sent and moved on — nothing reads this
    /// slot) and whenever no dispatch is running. See
    /// [`AllowNestedGuard`] and [`ConnUse`].
    allow_nested: bool,
    /// Replies still owed to this slot by nested client calls that gave up
    /// waiting (`set_timeout`); the next that many `REPLY`s are skipped
    /// instead of being taken for unsolicited ones (see
    /// [`RpcSessionInner::note_stale_reply`]).
    stale_replies: u32,
    /// Set by a nested (reentrant) frame that lost track of what the peer
    /// sends next ([`Desync::ProtocolStateLost`]): whoever owns the slot
    /// must not interpret another frame on it. Never cleared — the owner
    /// retires the slot instead (see
    /// [`RpcSessionInner::mark_slot_poisoned`]).
    poisoned: bool,
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

/// What a failed step in `client_transact`'s reply wait left on the
/// stream. Only a reentrant frame has to distinguish them: it cannot
/// retire the slot, because the outer frame owns it.
enum Desync {
    /// Nothing came off the stream, so this call's `REPLY` may still be
    /// on its way and the outer frame has to skip one.
    ReplyStillInbound,
    /// What the peer sends next is unknowable — the pending `REPLY` may
    /// still arrive, or it may already be gone. Either a frame arrived
    /// and did not decode, or a read failed without the guarantee that
    /// it stopped at a frame boundary: only `PeerClosed` and `Timeout`
    /// carry that guarantee, so every other read failure lands here,
    /// including one that consumed nothing at all. AOSP ends the whole
    /// session here (`RpcState::waitForReply`: "processCommand must
    /// shutdown on failure").
    ProtocolStateLost,
}

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        if self.reentrant {
            // A reentrant guard reused the outer frame's slot *and* its
            // `DRIVING` marker — it pushed neither the marker nor claimed
            // `exclusive_tid`, so it must remove neither. Removing a
            // `DRIVING` entry here would delete the OUTER frame's marker
            // (the key (session, slot) matches), so a later same-thread
            // nested call would no longer see it, re-claim the slot as
            // non-reentrant, and on *its* drop clear an `exclusive_tid`
            // the outer frame still owns — corrupting exclusivity on
            // multi-slot sessions.
            return;
        }
        let key = (self.inner as *const _ as usize, self.slot_id);
        DRIVING.with(|d| {
            let mut v = d.borrow_mut();
            if let Some(pos) = v.iter().rposition(|&k| k == key) {
                v.remove(pos);
            }
        });
        {
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

/// AOSP `RpcState::processTransactInternal`'s `connection.allowNested =
/// !oneway` window, as RAII.
///
/// While a **twoway** inbound transaction's handler runs, the slot it
/// arrived on may carry a same-thread nested call: the peer is parked in
/// its reply wait on that socket and will read what the handler writes.
/// A **oneway** dispatch grants nothing — the peer sent and moved on, so
/// a nested transaction written there would sit unread until something
/// else happens to read the slot (a twoway reply wait), i.e. forever for
/// a client's incoming (callback) connection. [`ConnUse::Client`] then
/// declines the pin and takes an `Outgoing` slot instead.
///
/// Save-and-restore (not set-and-clear) so a oneway dispatched *inside*
/// a twoway handler's nested reply wait restores the outer frame's
/// grant — AOSP's `origAllowNested`.
struct AllowNestedGuard<'a> {
    inner: &'a RpcSessionInner,
    slot_id: u64,
    prev: bool,
}

impl<'a> AllowNestedGuard<'a> {
    /// Arm on the slot this thread currently drives for `inner` (the
    /// [`DRIVING`] top-of-stack). `None` — nothing to restore — only when
    /// the pinned slot was already retired
    /// ([`remove_slot`](RpcSessionInner::remove_slot) racing a dispatch);
    /// every `dispatch_transact` caller holds a pin.
    fn arm(inner: &'a RpcSessionInner, allow: bool) -> Option<Self> {
        let sess_ptr = inner as *const RpcSessionInner as usize;
        let slot_id = DRIVING.with(|d| {
            d.borrow()
                .iter()
                .rev()
                .find_map(|&(sp, sid)| if sp == sess_ptr { Some(sid) } else { None })
        })?;
        let mut st = inner.conn_state.lock().expect("conn_state poisoned");
        let slot = st.slots.iter_mut().find(|s| s.id == slot_id)?;
        let prev = std::mem::replace(&mut slot.allow_nested, allow);
        Some(Self {
            inner,
            slot_id,
            prev,
        })
    }
}

impl Drop for AllowNestedGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.inner.conn_state.lock().expect("conn_state poisoned");
        if let Some(s) = st.slots.iter_mut().find(|s| s.id == self.slot_id) {
            s.allow_nested = self.prev;
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
    /// Baseline read deadline a **serve-driven** ([`SlotRole::Incoming`])
    /// slot of this session must be returned to once a temporarily armed
    /// reply deadline is lifted — the server's
    /// [`set_idle_timeout`](super::RpcServer::set_idle_timeout), recorded
    /// by `configure_session`. Without it, a nested callback issued from a
    /// handler would clear the serve connection's `SO_RCVTIMEO` for good
    /// and silently disable idle eviction on it. `None` (client sessions,
    /// and servers that set no idle timeout) ⇒ byte-identical to clearing.
    serve_read_deadline: Mutex<Option<Duration>>,
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
    /// empty slot pool — unlike an `obituary_sent`-style flag, which
    /// would only flip after the callback returned.
    lifecycle: SessionLifecycle,
    /// Which side of the connection this session is: decides the
    /// founding slot's [`SlotRole`] and the client-vs-server teardown
    /// rules for serve-driven slots.
    space: AddressSpace,
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

    pub(crate) fn space(&self) -> AddressSpace {
        self.space
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
pub(crate) struct RpcSessionInner {
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
    /// a profile-mismatch — see [`add_incoming_slot_capped`]).
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
    /// Threads serving this client's incoming (callback) connections,
    /// keyed by slot id. Joined by [`RpcSession::shutdown`].
    incoming_threads: Mutex<Vec<(u64, std::thread::JoinHandle<()>)>>,
    /// How many of those threads are still running. Bumped before the
    /// spawn and dropped by the thread itself as its very last act.
    /// Tells "the thread finished" apart from "we stopped tracking it" —
    /// `incoming_threads` is `mem::take`n before the first `join()`, so
    /// its length is zero either way.
    incoming_live: AtomicUsize,
    /// How many of those threads [`RpcSession::shutdown`] has actually
    /// `join()`ed. `incoming_live` cannot stand in for this: the serve
    /// threads are woken by the transport shutdown that `shutdown`
    /// performs first, so they usually finish on their own while it is
    /// still tearing the session down, and the count would read zero
    /// even if every handle were dropped instead of joined.
    incoming_joined: AtomicUsize,
}

/// The `RpcParcelOps` implementation bound to one session.
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
    ///     slot). A serve-driven ([`SlotRole::Incoming`]) pin is
    ///     re-entered only while its dispatch grants it
    ///     ([`ConnSlot::allow_nested`] — AOSP
    ///     `exclusiveIncoming->allowNested`): a oneway dispatch leaves
    ///     nobody reading that socket, so the pin is **declined** and
    ///     the scan below applies. [`ConnUse::ClientRefcount`] ignores
    ///     the grant (a `DEC_STRONG` awaits no reply) — this is the only
    ///     place the two uses differ.
    ///  2. **Exclusive** — a slot whose `exclusive_tid == this tid`
    ///     (defensive: should be covered by 1).
    ///  3. **First available `Outgoing` slot** — the first slot with
    ///     `exclusive_tid == None` **and** [`SlotRole::Outgoing`] (AOSP
    ///     scans only `mOutgoing` here). Claim it (`exclusive_tid = this
    ///     tid`), push the `DRIVING` marker so any same-thread nested
    ///     call re-enters here, return the guard. An `Incoming` slot is
    ///     never claimed by a non-nested call: it is a serve loop's
    ///     socket, read by the peer only inside its own reply wait.
    /// - **No `Outgoing` slot at all** — fail **immediately** with
    ///   `Err(StatusCode::FailedTransaction)` (AOSP `WOULD_BLOCK`:
    ///   "Session has no outgoing connections"). This is a server
    ///   calling a client proxy outside any handler while the client
    ///   opened no incoming connection; waiting would never end,
    ///   since only a peer attach can add an `Outgoing` slot. A
    ///   `DEC_STRONG` ([`ConnUse::ClientRefcount`]) reaches the same arm
    ///   and is simply skipped — see [`find_conn_lenient`].
    ///  4. **Pool exhausted** — `wait` on `slot_cv` (released on slot
    ///     drop OR `add_*_slot`). **Block-and-wait, never busy
    ///     try-loop**. Each wakeup (and the first park) rechecks the
    ///     session lifecycle and pool (mirroring
    ///     [`find_conn_for_reaper`]) and returns
    ///     `Err(StatusCode::DeadObject)` if the session is torn down or
    ///     the pool has drained — `add_*_slot` can never refill a dead
    ///     session, so re-`wait` would park forever.
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
    fn find_conn(&self) -> Result<ConnGuard<'_>> {
        self.find_conn_impl(ConnUse::Client)
    }

    /// [`find_conn`] for reply-less `DEC_STRONG` sends. It differs from
    /// [`find_conn`] only on the **reentrant pin**, which it may re-enter
    /// without the dispatch's [`ConnSlot::allow_nested`] grant (AOSP
    /// `CLIENT_REFCOUNT`: "we currently allow ref count calls to be
    /// nested"). The *scan* is identical — `Outgoing` only.
    ///
    /// So a server that drops a client proxy outside a handler, on a
    /// session where the client opened no incoming connection, does not
    /// release the peer's node until session end. That is the documented
    /// best-effort contract, and the alternative is worse: a serve-driven
    /// slot is read by its peer only inside that peer's own reply wait,
    /// so writing there is not a delayed frame but a send that blocks
    /// once the socket buffer fills — with the slot's `exclusive_tid`
    /// held, which locks its serve loop out of its own connection.
    /// A client that wants prompt release opens an incoming connection
    /// ([`RpcUnixClientConfig::incoming_connections`], plan 2-20).
    fn find_conn_lenient(&self) -> Result<ConnGuard<'_>> {
        self.find_conn_impl(ConnUse::ClientRefcount)
    }

    /// Shared body of [`find_conn`](Self::find_conn) /
    /// [`find_conn_lenient`](Self::find_conn_lenient) — see the former
    /// for the selection order.
    ///
    /// The scan's `free` predicate matches `exclusive_tid == Some(tid)`
    /// only when this thread holds **no** pin: reaching the scan with a
    /// `pinned` slot means step 1 *declined* it, so this thread still owns
    /// that slot's `exclusive_tid`. Matching it would hand the slot back
    /// as non-reentrant, and the new guard's drop would release an
    /// `exclusive_tid` the outer frame still holds.
    fn find_conn_impl(&self, use_: ConnUse) -> Result<ConnGuard<'_>> {
        // The scan below is `Outgoing`-only for **every** use, matching
        // AOSP `ExclusiveConnection::find`, which looks `mIncoming` up with
        // `available = nullptr`: only a connection this thread already
        // holds can match there, never a free one. `ClientRefcount` still
        // differs from `Client`, but only on the *reentrant* pin above.
        let refcount = use_ == ConnUse::ClientRefcount;
        let mut wait_until: Option<Instant> = None;
        let tid = current_tid();
        let sess_ptr = self as *const RpcSessionInner as usize;
        // (1) Reentrant: a slot of this session is already driven by
        //     this thread (innermost first — `rposition`). A
        //     serve-driven slot qualifies only while its dispatch
        //     grants it (`allow_nested`, AOSP's `exclusiveIncoming->
        //     allowNested`); otherwise the pin is declined and the scan
        //     below looks for an `Outgoing` slot instead.
        let pinned = DRIVING.with(|d| {
            d.borrow()
                .iter()
                .rev()
                .find_map(|&(sp, sid)| if sp == sess_ptr { Some(sid) } else { None })
        });
        if let Some(slot_id) = pinned {
            let reusable = {
                let st = self.conn_state.lock().expect("conn_state poisoned");
                match st.slots.iter().find(|s| s.id == slot_id) {
                    // The slot this thread was driving was retired
                    // (`remove_slot` on a worker exit) between the `DRIVING`
                    // push and this reentrant lookup. Surface a typed dead
                    // error rather than panicking on the driver loop.
                    None => return Err(StatusCode::DeadObject),
                    Some(s)
                        if s.role == SlotRole::Outgoing
                            || s.allow_nested
                            || !use_.is_new_transaction() =>
                    {
                        Some(Arc::clone(&s.transport))
                    }
                    Some(_) => None,
                }
            };
            if let Some(transport) = reusable {
                return Ok(ConnGuard {
                    inner: self,
                    slot_id,
                    transport,
                    reentrant: true,
                });
            }
        }
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        loop {
            // Torn-down/drained recheck — re-`wait` on a dead pool would
            // park forever (mirrors `find_conn_for_reaper`).
            if self.shared.lifecycle.is_torn_down() || st.slots.is_empty() {
                return Err(StatusCode::DeadObject);
            }
            // (2)/(3) First available — `Outgoing` first; `pinned.is_none()`
            // guards the self-match (see this fn's rustdoc).
            let free = |s: &ConnSlot| {
                s.exclusive_tid.is_none() || (pinned.is_none() && s.exclusive_tid == Some(tid))
            };
            let pick = st
                .slots
                .iter()
                .position(|s| free(s) && s.role == SlotRole::Outgoing);
            if let Some(idx) = pick {
                let s = &mut st.slots[idx];
                s.exclusive_tid = Some(tid);
                let slot_id = s.id;
                let transport = Arc::clone(&s.transport);
                DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
                return Ok(ConnGuard {
                    inner: self,
                    slot_id,
                    transport,
                    reentrant: false,
                });
            }
            // (3b) Nothing to wait for: no `Outgoing` slot exists, and only a
            //      peer attach can create one. AOSP `WOULD_BLOCK`.
            if !st.slots.iter().any(|s| s.role == SlotRole::Outgoing) {
                if refcount {
                    // Best-effort by contract, so this is a skip, not an
                    // error: the peer's node is released at session end
                    // instead. Riding a serve-driven slot would write to a
                    // socket the peer reads only inside its own reply wait,
                    // so a filled buffer would block the send and lock that
                    // slot's serve loop out of its own connection.
                    log::debug!(
                        "RPC: no outgoing connection for a DEC_STRONG; skipping (the node is \
                         released at session end, or promptly once the peer opens an incoming \
                         connection)"
                    );
                } else if self.shared.space() == AddressSpace::Initiator {
                    // Not "the peer must open incoming connections": the
                    // incoming ones may well be up — what is gone is this
                    // endpoint's own send capability.
                    log::error!(
                        "RPC: this client has lost every outgoing connection (a reply \
                         deadline or a desynced reply retires the slot it rode); the \
                         surviving callback connections are server→client only, so \
                         nothing would read a request written on them"
                    );
                } else if self.profile.wire_version().is_none() {
                    // The r34 profile has no attach mechanism at all, so no
                    // advice about incoming connections applies there.
                    log::error!(
                        "RPC: r34 session has no outgoing connection — this endpoint \
                         accepted the connection, and the r34 profile cannot open or \
                         attach one. Only a nested call (from inside a twoway handler) \
                         can transact here; use `?profile=android13plus` for callbacks \
                         outside a handler"
                    );
                } else {
                    log::error!(
                        "RPC: session has no outgoing connection — a non-nested call (from \
                         another thread, or a oneway outside a handler) needs the peer to open \
                         incoming connections (RpcUnixClientConfig::incoming_connections / \
                         ARpcSession_setMaxIncomingThreads); refusing instead of waiting forever"
                    );
                }
                return Err(StatusCode::FailedTransaction);
            }
            // (4) Pool exhausted — wait, bounded by the session deadline: a serve-pinned slot is released only by the peer.
            let deadline = *self.shared.timeout.lock().expect("timeout poisoned");
            st = match deadline {
                Some(d) => {
                    // One absolute deadline for the whole wait: `slot_cv` is
                    // also woken by releases of slots this scan cannot use,
                    // so re-arming `d` per wake would never expire on a busy
                    // pool. Expiry is checked only after a fresh scan.
                    let at = *wait_until.get_or_insert_with(|| Instant::now() + d);
                    let remaining = at.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        log::warn!(
                            "RPC: no connection slot became available within {d:?} \
                             (every slot is driven by another thread — a session \
                             served on one thread and transacted on another needs \
                             more than one connection)"
                        );
                        return Err(StatusCode::TimedOut);
                    }
                    self.slot_cv
                        .wait_timeout(st, remaining)
                        .expect("slot_cv poisoned")
                        .0
                }
                None => self.slot_cv.wait(st).expect("slot_cv poisoned"),
            };
        }
    }

    /// Non-blocking variant of [`find_conn`]
    /// for `RpcProxy::drop`'s fast path. Returns `None` instead of
    /// waiting on `slot_cv` when the pool has no free slot.
    ///
    /// Like [`find_conn`], step (1) is the reentrant `DRIVING` pin: a
    /// caller reaching here from `RpcProxy::drop` → `queue_dec_strong` can
    /// be a server handler that dropped the last cached-proxy `SIBinder`
    /// on its *own* dispatch thread — which is already driving one of this
    /// session's slots. Reusing that slot (`reentrant: true`) is the
    /// documented interleaved-DEC_STRONG path. The first-available scan
    /// below claims **only** a genuinely free slot: claiming a slot this
    /// thread already drives would, on this guard's drop, clear an
    /// `exclusive_tid` the outer frame still owns and let another thread
    /// race onto the same transport (wire interleaving / reply theft).
    fn try_find_conn(&self) -> Option<ConnGuard<'_>> {
        let tid = current_tid();
        let sess_ptr = self as *const RpcSessionInner as usize;
        // (1) Reentrant pin — mirror `find_conn` step (1).
        if let Some(slot_id) = DRIVING.with(|d| {
            d.borrow()
                .iter()
                .rev()
                .find_map(|&(sp, sid)| if sp == sess_ptr { Some(sid) } else { None })
        }) {
            let st = self.conn_state.lock().expect("conn_state poisoned");
            let transport = Arc::clone(&st.slots.iter().find(|s| s.id == slot_id)?.transport);
            drop(st);
            return Some(ConnGuard {
                inner: self,
                slot_id,
                transport,
                reentrant: true,
            });
        }
        // (3) First available — only a truly free `Outgoing` slot. A
        //     serve-driven one is never taken from the scan (AOSP looks
        //     `mIncoming` up with `available = nullptr`): the peer reads it
        //     only inside its own reply wait, so an unread frame is not
        //     merely delayed — once the socket buffer fills, the send
        //     blocks and this slot's serve loop is locked out of it.
        //     `None` here just defers to the reaper, then to session end.
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        let idx = st
            .slots
            .iter()
            .position(|s| s.exclusive_tid.is_none() && s.role == SlotRole::Outgoing)?;
        let s = &mut st.slots[idx];
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
            let free = |s: &ConnSlot| s.exclusive_tid == Some(tid) || s.exclusive_tid.is_none();
            // `Outgoing` only, like every other scan. A serve-driven slot is
            // never taken here: the peer reads it only inside its own reply
            // wait, so a filled buffer would block this send while the
            // reaper holds the slot's `exclusive_tid` — starving that
            // slot's serve loop.
            let pick = st
                .slots
                .iter()
                .position(|s| free(s) && s.role == SlotRole::Outgoing);
            if let Some(idx) = pick {
                let s = &mut st.slots[idx];
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
            // Nothing to wait for — only a peer attach could create an
            // `Outgoing` slot, and parking here would hold the session's
            // strong `Arc` (the leak this function's doc guards against).
            // Skipping is the documented best-effort contract.
            if !st.slots.iter().any(|s| s.role == SlotRole::Outgoing) {
                return None;
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
                match st.slots.iter().find(|s| s.id == want_slot_id) {
                    Some(s) => Arc::clone(&s.transport),
                    // Pinned slot retired between the `DRIVING` push and this
                    // reentrant lookup — surface a typed dead error instead of
                    // panicking on the driver loop.
                    None => return Err(StatusCode::DeadObject),
                }
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

    /// Lift the read deadline on a slot's transport (best-effort). Used by
    /// the r34 serve path to clear the handshake/admission deadline after
    /// the first frame so an established session idles unbounded.
    fn clear_slot_read_timeout(&self, slot_id: u64) {
        let transport = {
            let st = self.conn_state.lock().expect("conn_state poisoned");
            st.slots
                .iter()
                .find(|s| s.id == slot_id)
                .map(|s| Arc::clone(&s.transport))
        };
        if let Some(t) = transport {
            if let Err(e) = t.set_read_timeout(None) {
                log::debug!("RPC: failed to clear first-frame read deadline: {e:?}");
            }
        }
    }

    /// Lift any read/write deadlines on every slot transport
    /// (best-effort). Used after a bounded handshake (e.g. an
    /// Accessor-supplied preconnected fd) so the established session
    /// reverts to the normal blocking-with-per-call-deadline behavior
    /// rather than inheriting the handshake's sticky `SO_RCVTIMEO`/
    /// `SO_SNDTIMEO`.
    fn clear_handshake_timeouts(&self) {
        let transports: Vec<Arc<dyn RpcTransport>> = {
            let st = self.conn_state.lock().expect("conn_state poisoned");
            st.slots.iter().map(|s| Arc::clone(&s.transport)).collect()
        };
        for t in transports {
            let _ = t.set_read_timeout(None);
            let _ = t.set_write_timeout(None);
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
    ///
    /// `None` for a torn-down session. That gate sits in the *same*
    /// critical section as the push (like
    /// [`add_slot_inner_capped`](Self::add_slot_inner_capped)): an
    /// attach's own pre-check is a snapshot taken before a connect +
    /// handshake round trip, and `on_session_dead` empties the pool under
    /// this very lock. A slot pushed after that is never shut down by the
    /// death sequence, so a serve loop on it blocks in `recv` forever,
    /// pinning the whole session graph.
    fn add_slot_inner(&self, transport: Box<dyn RpcTransport>, role: SlotRole) -> Option<u64> {
        // `Arc::from(Box<dyn T>)` is the stable std conversion that
        // re-takes the heap allocation under an `Arc` without copying
        // (impl<T: ?Sized> From<Box<T>> for Arc<T>). The slot holds
        // the canonical `Arc`; `ConnGuard`s hand out cheap clones.
        let transport: Arc<dyn RpcTransport> = Arc::from(transport);
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        // Anti-resurrection gate — must share the push's critical section
        // (see this fn's rustdoc).
        if self.shared.lifecycle.is_torn_down() {
            return None;
        }
        let id = st.next_slot_id;
        st.next_slot_id += 1;
        st.slots.push(ConnSlot {
            transport,
            exclusive_tid: None,
            id,
            role,
            allow_nested: false,
            stale_replies: 0,
            poisoned: false,
        });
        drop(st);
        self.slot_cv.notify_all();
        Some(id)
    }

    /// Server attach: add a serve-driven slot, enforcing the
    /// `setMaxIncomingThreads` cap **atomically** with the push and — in
    /// the same critical section — the anti-resurrection gate. Counts
    /// only `incoming` slots (AOSP caps `mIncoming.size()`, not the
    /// callback connections the client opened toward us). Returns
    /// `Err(FailedTransaction)` at the cap, `Err(DeadObject)` for a
    /// torn-down session.
    fn add_incoming_slot_capped(
        &self,
        transport: Box<dyn RpcTransport>,
        cap: usize,
    ) -> Result<u64> {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        if st
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Incoming)
            .count()
            >= cap
        {
            return Err(StatusCode::FailedTransaction);
        }
        if !self.shared.try_bump_live_conns() {
            return Err(StatusCode::DeadObject);
        }
        let transport: Arc<dyn RpcTransport> = Arc::from(transport);
        let id = st.next_slot_id;
        st.next_slot_id += 1;
        st.slots.push(ConnSlot {
            transport,
            exclusive_tid: None,
            id,
            role: SlotRole::Incoming,
            allow_nested: false,
            stale_replies: 0,
            poisoned: false,
        });
        drop(st);
        self.slot_cv.notify_all();
        Ok(id)
    }

    /// Client multi-outgoing: append an *outgoing*
    /// connection slot (no `live_conns` bump — client outgoing slots
    /// are not serve-driven). See
    /// [`RpcSession::add_outgoing_connection_android13plus`].
    /// `None` for a torn-down session.
    fn add_outgoing_slot(&self, transport: Box<dyn RpcTransport>) -> Option<u64> {
        self.add_slot_inner(transport, SlotRole::Outgoing)
    }

    /// Like [`add_slot_inner`](Self::add_slot_inner) but enforces a
    /// maximum number of callback (`Outgoing`) slots **atomically** under
    /// the `conn_state` lock: returns `None` (adding nothing, closing
    /// `transport`) when the pool already holds `cap` of them. Serve-driven
    /// slots do not count — a client's outgoing fan-out must never eat
    /// into its callback budget. Used for the server callback-slot
    /// admission cap — callback slots have no serve loop and are only
    /// reclaimed at session teardown, so a separate pre-check + add would
    /// let concurrent attach workers each pass the check and overshoot the
    /// cap, letting an untrusted peer grow the pool (and its held fds)
    /// unbounded. Folding the check into the push closes that TOCTOU.
    ///
    /// `claimed`: push the slot already driven by the calling thread
    /// (`exclusive_tid = current`), so the caller can finish a wire
    /// exchange on it before anyone else may pick it.
    ///
    /// The torn-down gate shares that critical section for the same
    /// reason: a gate read outside this lock is a snapshot, and
    /// `on_session_dead` empties the pool under this very lock — but only
    /// *after* firing obituaries and running user `Drop` code, a wide
    /// window in which a lock-free pre-check would still read `Live` and
    /// push a slot onto a session that is already dying. The caller then
    /// confirms the attach to the peer — a promise nothing can keep.
    fn add_slot_inner_capped(
        &self,
        transport: Box<dyn RpcTransport>,
        cap: usize,
        claimed: bool,
    ) -> Option<u64> {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        // Anti-resurrection gate — same critical section as the cap check,
        // for the same reason (see this fn's rustdoc).
        if self.shared.lifecycle.is_torn_down() {
            return None;
        }
        if st
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Outgoing)
            .count()
            >= cap
        {
            return None;
        }
        let transport: Arc<dyn RpcTransport> = Arc::from(transport);
        let id = st.next_slot_id;
        st.next_slot_id += 1;
        st.slots.push(ConnSlot {
            transport,
            exclusive_tid: if claimed { Some(current_tid()) } else { None },
            id,
            role: SlotRole::Outgoing,
            allow_nested: false,
            stale_replies: 0,
            poisoned: false,
        });
        drop(st);
        self.slot_cv.notify_all();
        Some(id)
    }

    /// A nested (reentrant) client call on `slot_id` stopped waiting for
    /// its reply. The slot stays in the pool — the outer frame owns it —
    /// so the reply is still inbound; count it so the stream re-syncs by
    /// skipping it rather than tearing the connection down as
    /// "unsolicited". Only for a wait that ended with the stream still at
    /// a frame boundary (`PeerClosed`, `Timeout`, or a failure before or
    /// after the read itself); a read that failed without that guarantee
    /// marks the slot unreadable instead
    /// ([`mark_slot_poisoned`](Self::mark_slot_poisoned)).
    fn note_stale_reply(&self, slot_id: u64) {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        if let Some(s) = st.slots.iter_mut().find(|s| s.id == slot_id) {
            s.stale_replies += 1;
        }
    }

    /// Mark `slot_id` unreadable: a nested (reentrant) call lost track of
    /// what the peer sends next, and the slot's owner must not take the
    /// next frame for its own. The flag — not
    /// [`RpcTransport::shutdown`](crate::rpc::RpcTransport::shutdown) — is
    /// what enforces that. What a `shutdown` does to bytes already
    /// received is platform- and backend-dependent (a kernel may keep or
    /// discard its receive queue, a transport may hold a buffered
    /// leftover, the default impl does nothing), so nothing here assumes
    /// it discards anything; the flag holds on every transport.
    fn mark_slot_poisoned(&self, slot_id: u64) {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        if let Some(s) = st.slots.iter_mut().find(|s| s.id == slot_id) {
            s.poisoned = true;
        }
    }

    /// Whether a nested call poisoned `slot_id`
    /// ([`mark_slot_poisoned`](Self::mark_slot_poisoned)).
    fn slot_poisoned(&self, slot_id: u64) -> bool {
        let st = self.conn_state.lock().expect("conn_state poisoned");
        st.slots.iter().any(|s| s.id == slot_id && s.poisoned)
    }

    /// Whether a `REPLY` just read on `slot_id` is one an abandoned nested
    /// call left behind; consumes one count when so.
    fn take_stale_reply(&self, slot_id: u64) -> bool {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        match st.slots.iter_mut().find(|s| s.id == slot_id) {
            Some(s) if s.stale_replies > 0 => {
                s.stale_replies -= 1;
                true
            }
            _ => false,
        }
    }

    /// Remove a slot from the pool — *retire* it. Six legitimate callers:
    /// the slot's *own* worker on its `serve_blocking_on` exit
    /// (self-remove); `client_transact`'s three retirements (a
    /// non-reentrant slot whose send failed at the transport, whose reply
    /// read lost the stream, or whose reply wait found the slot already
    /// marked unreadable by a nested call); and the two attach rollbacks —
    /// an incoming connection whose serve thread failed to spawn, and a
    /// callback slot whose `"cci"` never reached the peer. After a
    /// retirement, the slot's worker finds its slot gone and gets
    /// `DeadObject` from `find_conn_pinned` — an expected exit signal,
    /// not a structural bug. `notify_all` so any `find_conn`
    /// (any-available) waiter re-evaluates against the shrunk pool.
    /// (Retiring is distinct from marking unreadable —
    /// [`mark_slot_poisoned`](Self::mark_slot_poisoned) — which leaves
    /// the slot in the pool for its owner to retire.)
    ///
    /// **Not a pure pool mutation:** emptying the pool — or, on an
    /// initiator, retiring its last `Outgoing` slot — runs the full
    /// death sequence ([`on_session_dead`](Self::on_session_dead)), which
    /// fires obituaries and drops the peer's local objects — i.e. it can
    /// re-enter user `Drop` code. Callers must hold no session lock.
    fn remove_slot(&self, slot_id: u64) {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        st.slots.retain(|s| s.id != slot_id);
        let empty = st.slots.is_empty();
        // An initiator that lost every `Outgoing` slot can never transact
        // again (`find_conn_impl`'s (3b) arm refuses, and only a peer
        // attach creates one), so its surviving incoming (callback) slots
        // must not swallow the death declaration by keeping the pool
        // non-empty. An acceptor legitimately has no `Outgoing` slot until
        // a client attaches one, so this applies to initiators only.
        let no_outgoing = !empty
            && self.shared.space() == AddressSpace::Initiator
            && !st.slots.iter().any(|s| s.role == SlotRole::Outgoing);
        drop(st);
        self.slot_cv.notify_all();
        // Pool empty ⇒ no connection left; a serve-less client reaches
        // death only here.
        if (empty || no_outgoing) && self.shared.lifecycle.try_drop_sole_connection() {
            self.on_session_dead();
        }
    }

    /// End this session now, whatever its connection count: declare
    /// death (`Live(n) → Dying`) and run the death sequence. Exactly one
    /// caller does the work; from `Dying`/`Dead` it is a no-op. Both
    /// [`RpcSession::shutdown`] and the server's
    /// [`terminate`](super::RpcServer::terminate) land here, so a session
    /// several workers drive ends the same way a sole-connection one does.
    pub(crate) fn close(&self) {
        if self.shared.lifecycle.force_dying() {
            self.on_session_dead();
        }
    }

    /// Full session death (the `Dying` state has just been entered):
    /// fire the obituaries, settle to `Dead`, then release every local
    /// object the peer held (AOSP `RpcState::clear`) — the step that
    /// breaks `session → local service → stored proxy → session`.
    /// Strong refs are dropped outside every lock — user `Drop` code may
    /// re-enter the session.
    ///
    /// **Unblock before user code.** Every step that can wake a thread of
    /// this session runs ahead of the obituaries, because an obituary (or
    /// a local `Drop`) may call [`RpcSession::shutdown`], which *joins*
    /// those threads — one still parked would deadlock that join. Two
    /// steps are needed, not one: `shutdown_all_transports` wakes only
    /// threads blocked in `recv`/`send`, while a thread waiting on
    /// `slot_cv` ([`find_conn_impl`](Self::find_conn_impl) with an
    /// exhausted pool, [`find_conn_pinned`](Self::find_conn_pinned)) wakes
    /// only when the pool is emptied *and* notified — a bare notify would
    /// have both re-`wait`. Emptying the pool this early is safe: callback
    /// slots have no serve loop to retire them anyway, and a dead
    /// session's pool is only consulted by paths that already answer
    /// `DeadObject` on `is_torn_down`. Nothing here sends — the death CAS
    /// already happened, so `send_dec_strong` and friends return early.
    pub(crate) fn on_session_dead(&self) {
        self.shutdown_all_transports();
        {
            let mut st = self.conn_state.lock().expect("conn_state poisoned");
            st.slots.clear();
        }
        self.slot_cv.notify_all();
        self.send_session_obituaries();
        self.shared.lifecycle.mark_dead();
        let root = self.shared.root.lock().expect("root poisoned").take();
        let locals = self
            .shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .clear_local();
        drop(locals);
        drop(root);
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

    /// A client's incoming (callback) slot: served by a thread this
    /// session owns and not counted in `live_conns`.
    fn is_client_incoming_slot(&self, slot_id: u64) -> bool {
        self.shared.space() == AddressSpace::Initiator
            && self
                .conn_state
                .lock()
                .expect("conn_state poisoned")
                .slots
                .iter()
                .any(|s| s.id == slot_id && s.role == SlotRole::Incoming)
    }

    /// Baseline read deadline `slot_id` must be returned to when a reply
    /// deadline armed on it is lifted. Serve-driven slots carry the
    /// session's [`SharedSession::serve_read_deadline`]; every other slot
    /// (a client's outgoing fan-out, a server's callback slots) has none.
    fn slot_baseline_read_deadline(&self, slot_id: u64) -> Option<Duration> {
        let serve_driven = self
            .conn_state
            .lock()
            .expect("conn_state poisoned")
            .slots
            .iter()
            .any(|s| s.id == slot_id && s.role == SlotRole::Incoming);
        if !serve_driven {
            return None;
        }
        *self
            .shared
            .serve_read_deadline
            .lock()
            .expect("serve_read_deadline poisoned")
    }

    fn incoming_slot_count(&self) -> usize {
        self.conn_state
            .lock()
            .expect("conn_state poisoned")
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Incoming)
            .count()
    }

    /// Shut every slot's transport down (best-effort) so a thread blocked
    /// in `recv` on this session — a client's incoming-connection thread,
    /// a user `serve_blocking` — returns with `PeerClosed`. Transports are
    /// cloned out first: the lock is never held across the syscalls.
    fn shutdown_all_transports(&self) {
        let transports: Vec<Arc<dyn RpcTransport>> = self
            .conn_state
            .lock()
            .expect("conn_state poisoned")
            .slots
            .iter()
            .map(|s| Arc::clone(&s.transport))
            .collect();
        for t in transports {
            if let Err(e) = t.shutdown() {
                log::debug!("RPC: transport shutdown failed (already closed?): {e:?}");
            }
        }
    }

    fn take_incoming_threads(&self) -> Vec<(u64, std::thread::JoinHandle<()>)> {
        std::mem::take(
            &mut *self
                .incoming_threads
                .lock()
                .expect("incoming_threads poisoned"),
        )
    }

    /// This session's advertised + enforced max-threads
    /// value. Set by [`RpcSession::set_max_threads`] (default 1) and
    /// returned to a client on `GET_MAX_THREADS`. AOSP-faithful
    /// `setMaxIncomingThreads`: the value is *both* the advertise and
    /// the **incoming slot cap** — the server attach arm refuses an
    /// id-echoing connection when adding it would push the session's
    /// `Incoming` slot count past it. Default 1 ⇒ founding-only
    /// (multi-conn callers must explicitly `set_max_threads(N >= 2)`).
    /// The client's *callback* connections are budgeted separately, at
    /// `2 *` this value — see `RpcServer::set_max_threads`.
    pub(crate) fn max_threads_value(&self) -> u32 {
        // `Relaxed` is sufficient: this atomic is a single-cell config
        // value (set by `RpcSession::set_max_threads` before any
        // accept/attach takes place; `std::thread::spawn` then provides
        // the happens-before for worker threads). No multi-atomic
        // ordering pair to maintain, so `SeqCst` would buy nothing here.
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
    fn wire_write_binder_addr(&self, p: &mut Parcel, addr: &RpcAddress) -> Result<()> {
        match &self.profile {
            WireProfile::R34(_) => write_addr(p, addr),
            WireProfile::Android13Plus(_) => {
                p.write_aligned_data(&Android13PlusCodec::encode_addr(addr))
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
                    // Another session's address means nothing to this peer (AOSP `onBinderLeaving`: INVALID_OPERATION).
                    if !std::ptr::eq(rp.session_ptr(), self) {
                        log::error!("RPC: cannot send a binder from an unrelated RPC session");
                        return Err(StatusCode::InvalidOperation);
                    }
                    // Pinned in the parcel past the send, so its DEC_STRONG cannot precede the reply that names it.
                    parcel.rpc_pin_binder(b.clone());
                    rp.address()
                } else {
                    // A local object leaving this process: `on_binder_leaving`
                    // bumps its `timesSent`. Record the address so a send
                    // failure can roll the bump back (`cancel_binder_leaving`)
                    // — the unreceived binder is never DEC'd by the peer.
                    let addr = self
                        .shared
                        .state
                        .lock()
                        .expect("rpc state poisoned")
                        .on_binder_leaving(b)?;
                    parcel.rpc_record_leaving_addr(addr);
                    addr
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
                self.wire_write_binder_addr(parcel, &addr)?;
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
        // `self_weak` is published in `with_shared` and `self` is reached
        // through that `Arc`, so this upgrade cannot fail; `?` is defensive.
        let strong = self.self_weak().upgrade().ok_or(StatusCode::DeadObject)?;
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
                SIBinder::new(Arc::new(RpcProxy::new(addr, strong)))
                    .expect("SIBinder::new(RpcProxy)")
            })?
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
    /// Undo the reservations an outgoing parcel took while it was serialized but
    /// which never reached the peer: the per-node oneway `async_number` (when
    /// `oneway_addr` is `Some((addr, consumed))`) and every local-binder
    /// `timesSent` bump recorded in `data`. Called on an encode/send failure and
    /// whenever a reply parcel is discarded (handler `Err`, oneway). The success
    /// path is untouched, so the wire is byte-unchanged.
    fn rollback_outgoing(&self, data: &Parcel, oneway_addr: Option<(RpcAddress, u64)>) {
        // Nothing to cancel ⇒ skip the state lock. The oneway dispatch path
        // calls this on every transaction, but a reply/args parcel that
        // flattened no local binder has no bump to roll back — avoid the lock on
        // that hot path (`rpc_leaving_addrs()` is a lock-free read).
        if oneway_addr.is_none() && data.rpc_leaving_addrs().is_empty() {
            return;
        }
        // Any node this drives to 0 is handed back and dropped only after
        // the guard: its strong ref may be a user service whose `Drop`
        // re-enters this session (`RpcProxy::drop` → `forget_remote_if`).
        let released: Vec<SIBinder> = {
            let mut state = self.shared.state.lock().expect("rpc state poisoned");
            if let Some((addr, consumed)) = oneway_addr {
                state.cancel_send_async_number(addr, consumed);
            }
            data.rpc_leaving_addrs()
                .iter()
                .filter_map(|addr| state.cancel_binder_leaving(addr))
                .collect()
        };
        drop(released);
    }

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
        let conn = self.find_conn()?;
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
        // From here the request has consumed reservations (a oneway
        // `async_number`, and each local binder's `timesSent` bump recorded
        // while `data` was serialized). If the encode or send fails, roll them
        // back: the peer never received the transaction, and — unlike AOSP —
        // rsbinder does not tear the (possibly multi-connection) session down
        // on a send failure, so without rollback the reservations would leak
        // (a stranded local node, a permanent oneway-ordering gap).
        let rollback = || self.rollback_outgoing(data, oneway.then_some((addr, async_number)));
        let frame = match self.profile.codec().encode_transact(&txn) {
            Ok(frame) => frame,
            Err(e) => {
                rollback();
                return Err(e.into());
            }
        };
        // Out-of-band fds collected while serializing the request
        // (empty unless `Unix` fd-mode).
        if let Err(e) = self.send_msg(transport, &frame, data.rpc_out_fds()) {
            rollback();
            // Transport-level send failure ⇒ peer gone on this connection;
            // retire the slot like the reply path (encode errors are not).
            if matches!(e, RpcError::PeerClosed | RpcError::Io(_)) && !conn.reentrant {
                self.remove_slot(conn.slot_id);
            }
            return Err(e.into());
        }
        if oneway {
            return Ok(None);
        }
        // Apply the configured reply deadline for the duration
        // of the reply wait only. `ReplyDeadlineGuard` restores the slot's
        // baseline `SO_RCVTIMEO` on every exit (return / `?` / panic) so
        // it never leaks onto the next call or a later recv here.
        let deadline = *self.shared.timeout.lock().expect("timeout poisoned");
        // Post-send: a failed reply wait leaves this call's `REPLY` unaccounted
        // for, and `WireReply` carries no id to match it by later.
        let poison_slot = |desync: Desync| {
            if !conn.reentrant {
                self.remove_slot(conn.slot_id);
                return;
            }
            // A reentrant frame owns nothing: the outer frame holds this
            // slot and will keep reading it.
            match desync {
                Desync::ReplyStillInbound => self.note_stale_reply(conn.slot_id),
                Desync::ProtocolStateLost => {
                    self.mark_slot_poisoned(conn.slot_id);
                    log::error!(
                        "RPC: a nested call lost track of the stream; what the peer sends next \
                         is unknowable, so this connection is poisoned and shut down rather than \
                         left for the outer call to read"
                    );
                    // Only wakes a reader blocked in `recv`; the poison flag
                    // above is what stops already-buffered frames.
                    if let Err(e) = transport.shutdown() {
                        log::debug!("RPC: shutting the desynced connection down failed: {e:?}");
                    }
                }
            }
        };
        // Arming is itself post-send, so a failure here desyncs the stream
        // exactly like a failed recv: poison before propagating.
        let restore = self.slot_baseline_read_deadline(conn.slot_id);
        let _deadline_guard = match ReplyDeadlineGuard::arm(transport, deadline, restore) {
            Ok(g) => g,
            Err(e) => {
                poison_slot(Desync::ReplyStillInbound);
                return Err(e.into());
            }
        };
        loop {
            // A nested call on this slot lost track of the stream, and
            // `shutdown` does not drop what the peer already sent.
            if self.slot_poisoned(conn.slot_id) {
                if !conn.reentrant {
                    self.remove_slot(conn.slot_id);
                }
                return Err(StatusCode::DeadObject);
            }
            let (frame, in_fds) = match self.recv_msg(transport) {
                Ok(v) => v,
                Err(e) => {
                    // Only a failure *at* a frame boundary leaves the reply
                    // inbound; the rest may have consumed part of one.
                    let desync = if matches!(e, RpcError::PeerClosed | RpcError::Timeout) {
                        Desync::ReplyStillInbound
                    } else {
                        Desync::ProtocolStateLost
                    };
                    poison_slot(desync);
                    return Err(e.into());
                }
            };
            let message = match self.profile.codec().decode_message(&frame) {
                Ok(m) => m,
                Err(e) => {
                    poison_slot(Desync::ProtocolStateLost);
                    return Err(e.into());
                }
            };
            match message {
                WireMessage::Reply(WireReply {
                    status,
                    data,
                    object_positions,
                }) => {
                    if self.take_stale_reply(conn.slot_id) {
                        log::debug!("RPC: skipped the late reply of an abandoned nested call");
                        continue;
                    }
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
                WireMessage::DecStrong(a, amount) => {
                    // Bind the removed ref so it drops after the guard
                    // (a temporary in the statement would drop under it).
                    let released = self
                        .shared
                        .state
                        .lock()
                        .expect("rpc state poisoned")
                        .dec_strong_local(&a, amount);
                    drop(released);
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
                    // panic out of `dispatch_transact` cannot leave the
                    // timeout desynchronized.
                    // Post-send like `ReplyDeadlineGuard::arm`, and worse:
                    // the nested `Transact` is already off the stream, so a
                    // bare `?` would drop it undispatched and leave our own
                    // `REPLY` inbound on a slot the pool would hand out
                    // again (`WireReply` carries no transaction id).
                    let _restore = match NestedDeadlineGuard::lift(transport, deadline) {
                        Ok(g) => g,
                        Err(e) => {
                            poison_slot(Desync::ReplyStillInbound);
                            return Err(e.into());
                        }
                    };
                    let peer = transport.peer_identity();
                    if let Err(e) = self.dispatch_transact(t, in_fds, peer) {
                        poison_slot(Desync::ReplyStillInbound);
                        return Err(e);
                    }
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
        // available. Preserves the FIFO observable timing. Inside a
        // dispatch this is the slot the thread already drives, so the
        // DEC follows the reply on the same connection (AOSP
        // `allowNested`); a proxy written into that reply is pinned by
        // the parcel until after the send (`write_binder`), so the DEC
        // cannot overtake it.
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
        // Lenient pick: an available slot of any role (or — if this
        // thread is already driving one of the session's slots — reuse
        // it via `DRIVING`, the documented "interleaved DEC_STRONG" path).
        let conn = self.find_conn_lenient()?;
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
        // inbound dispatch slot — `DRIVING` reentrant pin). For an
        // outermost server reply (`serve_once_on_slot` pinned the slot
        // before dispatch) this is the same slot the request arrived
        // on. [`ConnUse::Reply`]: the pin is taken whatever the slot's
        // role or nesting grant — the peer is waiting for this answer
        // on that socket and nowhere else.
        let conn = self.find_conn_impl(ConnUse::Reply)?;
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
        // Wrap the peer once (Plan 2-16 Phase B / Phase C): handlers can
        // read the full identity via `calling_caller()`, and the `Arc`
        // keeps the oneway drain's per-entry install a cheap refcount bump
        // (no `String`-bearing `PeerIdentity` clone).
        let peer = Arc::new(peer);
        if oneway {
            self.dispatch_oneway_ordered(t, in_fds, peer)
        } else {
            self.execute_dispatched(t, in_fds, false, peer)
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
        peer: Arc<PeerIdentity>,
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
                super::state::AsyncDecision::Terminate(num_pending) => {
                    // The out-of-order oneway backlog hit the terminate
                    // watermark; `dispatch_async_or_enqueue` already
                    // flushed the node's queue. Returning Err breaks the
                    // serve loop and tears this connection down
                    // (FAILED_TRANSACTION). This is connection-level, not
                    // AOSP's whole-session shutdownAndWait, but the flush
                    // already reclaimed the backlog.
                    log::error!(
                        "RPC: {num_pending} pending oneway transactions on {addr:?}; \
                         flushing backlog and tearing down connection"
                    );
                    return Err(StatusCode::FailedTransaction);
                }
            }
        };
        while let Some((t, fds)) = next {
            // All replayed oneways belong to this session, so the caller
            // identity is the same for every drained entry (cheap `Arc` clone).
            self.execute_dispatched(t, fds, true, Arc::clone(&peer))?;
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
        peer: Arc<PeerIdentity>,
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

        // AOSP `RpcState::processTransactInternal`: this transaction's
        // connection admits a same-thread nested call only while a
        // *twoway* handler runs on it — a oneway leaves nobody reading.
        let _nested = AllowNestedGuard::arm(self, !oneway);
        // Plan 2-16 Phase B/C: stamp the caller's peer identity into the
        // RPC calling context for the duration of the user handler, so
        // `get_calling_uid()`/`get_calling_pid()` work over Unix RPC and
        // `calling_caller()` exposes the full peer (uid / cert / vsock) for
        // authorization. The guard restores on drop, so a nested re-entrant
        // callback over the same connection nests correctly.
        let result = consume_rpc_interface_token(&mut reader, target.descriptor()).and_then(|()| {
            // Mirror the kernel server entrypoint's `dispatch_transact_caught`
            // (thread_state.rs): a panic in the user `on_transact` handler
            // must NOT unwind through the serve loop, because that would skip
            // the `serve_blocking_on_inner` cleanup (drop_connection /
            // send_session_obituaries / remove_slot) and leave a slot pinned
            // to a dead worker — deadlock + session-lifecycle corruption +
            // missed obituaries. Catch it here and turn it into a
            // deterministic error reply, symmetric with the kernel path. The
            // `RpcCallingGuard` is created inside the closure so its `Drop`
            // (which restores the calling context) still runs on unwind. On
            // the error path the reply parcel is unused (`send_reply` sends an
            // empty body with the status), so a partially-written `reply`
            // cannot leak to the peer.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _calling = crate::thread_state::RpcCallingGuard::install(Arc::clone(&peer));
                target.rpc_transact(t.code, &mut reader, &mut reply)
            }))
            .unwrap_or_else(|payload| {
                let msg = payload
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string panic payload>");
                log::error!("RPC on_transact panicked for code {}: {msg}", t.code);
                Err(crate::StatusCode::Unknown)
            })
        });

        if oneway {
            if let Err(e) = result {
                log::error!("oneway RPC transaction failed (dropped): {e:?}");
            }
            // A oneway reply is always discarded (never sent). Roll back any
            // `timesSent` bumps a handler made by writing a binder into `reply`,
            // so those local nodes do not leak (the peer never receives them and
            // so never `DEC_STRONG`s). Empty for AIDL-generated oneway stubs.
            self.rollback_outgoing(&reply, None);
            return Ok(());
        }
        match result {
            Ok(()) => {
                let sent = self.send_reply(
                    0,
                    reply.rpc_data_bytes(),
                    reply.rpc_object_positions(),
                    reply.rpc_out_fds(),
                );
                if sent.is_err() {
                    // The reply (with any binders the handler returned) never
                    // reached the peer; roll back their `timesSent` bumps so the
                    // returned local nodes do not leak. No oneway counter here —
                    // a reply is always twoway.
                    self.rollback_outgoing(&reply, None);
                }
                sent
            }
            Err(e) => {
                // The error reply carries an empty body (the partially-written
                // `reply` is discarded), so roll back any `timesSent` bumps from
                // binders a handler wrote before returning `Err` — symmetric with
                // the send-failure arm above.
                self.rollback_outgoing(&reply, None);
                self.send_reply(e.into(), &[], &[], &[])
            }
        }
    }

    /// Handle one inbound message on the pinned **slot** (a server
    /// worker drives a specific accepted connection's slot;
    /// nested outbound callbacks from the handler reuse this same slot
    /// via the `DRIVING` marker).
    /// `Ok(false)` ⇒ end of stream (stop); a slot a nested call marked
    /// unreadable ends the loop with `Err(StatusCode::DeadObject)`.
    fn serve_once_on_slot(&self, slot_id: u64) -> Result<bool> {
        // The worker drives its own slot. Pinning normally succeeds, but a
        // concurrent `client_transact` may have retired the slot after a
        // stale-reply desync — `find_conn_pinned` then returns
        // `DeadObject`, propagated so the worker exits cleanly.
        let conn = self.find_conn_pinned(slot_id)?;
        // A nested call lost track of the stream: what is buffered is not ours.
        if self.slot_poisoned(slot_id) {
            return Err(StatusCode::DeadObject);
        }
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
            WireMessage::DecStrong(a, amount) => {
                // Bind the removed ref so it drops after the guard (a
                // temporary in the statement would drop under it — and a
                // user `Drop` re-entering the session must not hold it).
                let released = self
                    .shared
                    .state
                    .lock()
                    .expect("rpc state poisoned")
                    .dec_strong_local(&a, amount);
                drop(released);
                Ok(true)
            }
            WireMessage::Reply(_) => {
                if self.take_stale_reply(slot_id) {
                    log::debug!("RPC: skipped the late reply of an abandoned nested call");
                    return Ok(true);
                }
                // AOSP `RpcState::processCommand` ends the session for an
                // unsolicited command ("misbehaving client"); ignoring it
                // would let a peer flood the log at wire speed.
                log::warn!("RPC server received an unexpected REPLY; ending the session");
                Err(StatusCode::BadType)
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
                // SIBinder::serialize → RPC branch → write_binder. A write
                // failure after `write_binder`'s `timesSent` bump must roll
                // back too, same as a send failure below.
                let write_res = match &root {
                    Some(b) => reply.write(b),
                    None => reply.write(&0i32),
                };
                if let Err(e) = write_res {
                    self.rollback_outgoing(&reply, None);
                    return Err(e);
                }
                // GET_ROOT carries a binder-in-parcel: at v2 its
                // position is in the object table.
                let sent =
                    self.send_reply(0, reply.rpc_data_bytes(), reply.rpc_object_positions(), &[]);
                if sent.is_err() {
                    // The reply carrying the root binder never reached the peer;
                    // roll back its `timesSent` bump so the root node does not
                    // leak. Symmetric with the twoway reply path.
                    self.rollback_outgoing(&reply, None);
                }
                sent
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
                // Body: i32 "client wants Unix"; reply 0=None/1=Unix goes out in the current mode, both switch after.
                // `Unix`, once set, is never renegotiated: a flip to `None` would drop in-flight fds and desync R34.
                if self.fd_mode() == FileDescriptorTransportMode::Unix {
                    let mut reply = Parcel::new();
                    reply.write(&1i32)?;
                    return self.send_reply(0, reply.rpc_data_bytes(), &[], &[]);
                }
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
                // FD transport is a v1+ feature. Gate GET_FD_MODE the same way
                // the connection handshake does (`negotiated >= PROTOCOL_V1`),
                // so a v0-negotiated android-13+ session cannot be flipped into
                // `Unix` fd mode here — that would open a v0 wire to fd
                // transport, bypassing the handshake's category-forbids-fd rule.
                // R34 (android-12, `wire_version() == None`) has no versioned
                // handshake; GET_FD_MODE *is* its native fd negotiation.
                let fd_version_ok = match self.profile.wire_version() {
                    None => true,
                    Some(v) => v >= PROTOCOL_V1,
                };
                let agreed = if want_unix
                    && fd_version_ok
                    && self.shared.fd_unix_supported.load(Ordering::SeqCst)
                {
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
///
/// # Lifetime
///
/// Proxies obtained over this session (`get_root`, binders read from
/// replies) hold the session **strongly** — AOSP `BpBinder` ↔
/// `sp<RpcSession>`. Dropping this handle does not invalidate them; the
/// connection closes when the last proxy and handle are gone. The
/// converse also holds: while the *peer* still holds one of this
/// endpoint's local objects (a callback it was handed), the session
/// stays alive until the peer releases it (`DEC_STRONG`) or the
/// connection ends — so dropping every proxy is not a guaranteed
/// disconnect. On connection loss every local object the peer held is
/// released (AOSP `RpcState::clear`); for a session without a serve
/// thread that loss is detected on the next failed transaction.
///
/// That leaves one case the runtime cannot notice: a local object handed
/// to the peer may itself hold a proxy back into this session, and if the
/// session neither serves nor transacts again, nothing runs the release.
/// [`RpcSession::shutdown`] is the explicit break for it.
///
/// A client session with incoming (callback) connections
/// ([`RpcUnixClientConfig::incoming_connections`]) owns the threads that
/// serve them, and those threads keep the session alive: dropping every
/// handle and proxy does not stop them. They end when the server closes
/// the session or on [`RpcSession::shutdown`] — call it when you are done
/// with such a session.
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
    ///
    /// # Direction
    ///
    /// `space` also fixes the direction the founding connection is used
    /// in (AOSP `RpcSession::mConnections.{mOutgoing, mIncoming}`):
    /// `Initiator` sends on it, `Acceptor` serves it. An `Acceptor`
    /// session therefore **cannot open a transaction of its own** —
    /// `get_root`, `RpcProxy::transact` and `ping_binder` called on it
    /// from outside a dispatch fail with
    /// [`StatusCode::FailedTransaction`], because writing a request into
    /// a connection the peer only reads inside its own reply wait would
    /// sit unread. Callbacks *from inside a twoway handler* are
    /// unaffected (they re-enter the dispatching connection). To call
    /// out of an acceptor otherwise, the peer must open incoming
    /// connections, which needs the android-13+ profile
    /// (`?profile=android13plus`,
    /// `RpcUnixClientConfig::incoming_connections`); the r34 profile has
    /// no such mechanism.
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
            serve_read_deadline: Mutex::new(None),
            fd_mode: Mutex::new(FileDescriptorTransportMode::None),
            fd_unix_supported: AtomicBool::new(false),
            rpc_session_id: gen_rpc_session_id()?,
            lifecycle: SessionLifecycle::new(),
            space,
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
        // The founding connection is the one this endpoint sends on
        // (client) or serves (server) — AOSP `setupClient` vs
        // `RpcServer::establishConnection`.
        let founding_role = match shared.space() {
            AddressSpace::Initiator => SlotRole::Outgoing,
            AddressSpace::Acceptor => SlotRole::Incoming,
        };
        let founding = ConnSlot {
            transport: Arc::from(transport),
            exclusive_tid: None,
            id: 1,
            role: founding_role,
            allow_nested: false,
            stale_replies: 0,
            poisoned: false,
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
            incoming_threads: Mutex::new(Vec::new()),
            incoming_live: AtomicUsize::new(0),
            incoming_joined: AtomicUsize::new(0),
        });
        *inner.self_weak.lock().expect("self_weak") = Arc::downgrade(&inner);
        // Reaper thread for non-blocking `DEC_STRONG` from
        // `RpcProxy::drop`. Detached — `with_shared` is on the user
        // thread, so we never block its return; inner drop closes the
        // channel and the reaper exits naturally.
        let weak_for_reaper = Arc::downgrade(&inner);
        if let Err(e) = std::thread::Builder::new()
            .name("rsbinder-rpc-reaper".into())
            .spawn(move || reaper_loop(weak_for_reaper, dec_strong_rx))
        {
            // The closure (and the receiver) are gone with the failed
            // spawn: every deferred DEC_STRONG on this session will be
            // dropped, leaking the peer's node until session end.
            log::error!("RPC: reaper thread spawn failed ({e}); deferred DEC_STRONG will be lost");
        }
        RpcSession { inner }
    }

    /// Test-only leak probe: a `Weak` on the session inner, so a test can
    /// assert the whole session graph was reclaimed after a disconnect.
    #[cfg(test)]
    pub(crate) fn inner_weak(&self) -> Weak<RpcSessionInner> {
        Arc::downgrade(&self.inner)
    }

    /// Id of the founding (first) slot. All non-attach
    /// `serve_blocking` callers (the default single-connection path)
    /// drive this slot.
    pub(crate) const FOUNDING_SLOT_ID: u64 = 1;

    /// Attach an accepted `transport` as a new serve-driven slot of this
    /// session's pool, subject to the `setMaxIncomingThreads` cap. The
    /// unified-model server attach arm calls this on the *founding*
    /// `Arc<RpcSessionInner>` (resolved from its 32-byte session id)
    /// instead of building a new `RpcSessionInner` sharing a
    /// `SharedSession` — so `state.remote_proxies`-cached `RpcProxy`s all
    /// point to the *single* session inner and a server worker's nested
    /// `proxy.transact` `find_conn`s stay within its own slot pool. The
    /// cap check, the anti-resurrection gate (`try_bump_live_conns`) and
    /// the push are one critical section (see
    /// `RpcSessionInner::add_incoming_slot_capped`); `Err(DeadObject)`
    /// when the session is already torn down so the caller rejects the
    /// attach instead of silently resurrecting a dead session.
    pub(crate) fn add_incoming_slot_capped(
        &self,
        transport: Box<dyn RpcTransport>,
        cap: usize,
    ) -> Result<u64> {
        self.inner.add_incoming_slot_capped(transport, cap)
    }

    /// Append a server-side *callback* slot — the wire mirror of the
    /// peer's `mIncoming` (AOSP `RpcServer.cpp`: `addOutgoingConnection
    /// (client, init=true)` for `incoming` headers). Does NOT bump
    /// the lifecycle count (callback slots are not serve-driven; AOSP
    /// also does not gate session lifetime on `mOutgoing.size()`).
    /// `Err(DeadObject)` if the session was already torn down when the
    /// call started (the lifecycle covers both `Dying` and `Dead` in a
    /// single check), or `Err(FailedTransaction)` if the session already
    /// holds `cap` callback slots — or died in between, which the
    /// atomic gate below catches under the same lock as the cap.
    ///
    /// `cap` and the teardown gate are both enforced atomically inside
    /// [`add_slot_inner_capped`] (`RpcSessionInner`) so concurrent attach
    /// workers cannot each clear an advisory pre-check and overshoot the
    /// budget, nor confirm an attach onto a dying session. `cap` counts
    /// **callback (`Outgoing`) slots only** — a client's outgoing fan-out
    /// never eats into it.
    ///
    /// Admission first, then the server's `"cci"` on the new connection:
    /// that order is what lets a refused client see an error instead of a
    /// silently dead connection (the accept handshake defers the write —
    /// see `wire_android13::server_write_connection_init`). The slot is
    /// held exclusive by this thread while the init goes out, so no
    /// callback transaction can overtake it on the wire.
    ///
    /// A failed init retires the slot immediately rather than leaving it
    /// for the first send to retire: until something sends on it, a dead
    /// slot still counts against the callback budget
    /// (`add_slot_inner_capped` counts `Outgoing` slots), and
    /// `find_conn` picks the *first* free `Outgoing` slot — so the next
    /// legitimate callback would draw this corpse and fail before a
    /// healthy slot is ever tried.
    pub(crate) fn add_callback_slot_and_init(
        &self,
        transport: Box<dyn RpcTransport>,
        cap: usize,
        codec: &Android13PlusCodec,
    ) -> Result<u64> {
        if self.inner.shared.lifecycle.is_torn_down() {
            return Err(StatusCode::DeadObject);
        }
        let slot_id = self
            .inner
            .add_slot_inner_capped(transport, cap, true)
            .ok_or(StatusCode::FailedTransaction)?;
        let transport = {
            let st = self.inner.conn_state.lock().expect("conn_state poisoned");
            st.slots
                .iter()
                .find(|s| s.id == slot_id)
                .map(|s| Arc::clone(&s.transport))
                .ok_or(StatusCode::DeadObject)?
        };
        let sent = {
            let mut io = RawTransportIo(&*transport);
            super::wire_android13::server_write_connection_init(&mut io, codec)
        };
        {
            let mut st = self.inner.conn_state.lock().expect("conn_state poisoned");
            if let Some(s) = st.slots.iter_mut().find(|s| s.id == slot_id) {
                s.exclusive_tid = None;
            }
        }
        self.inner.slot_cv.notify_all();
        match sent {
            Ok(()) => Ok(slot_id),
            Err(e) => {
                // Retire now, not on the first send: a corpse slot still
                // eats the callback budget and `find_conn` would draw it
                // first (see this fn's rustdoc).
                let _ = transport.shutdown();
                self.inner.remove_slot(slot_id);
                Err(StatusCode::from(e))
            }
        }
    }

    /// Test form of [`add_callback_slot_and_init`](Self::add_callback_slot_and_init)
    /// without the wire init (no peer on the other end).
    #[cfg(test)]
    pub(crate) fn add_callback_slot(
        &self,
        transport: Box<dyn RpcTransport>,
        cap: usize,
    ) -> Result<u64> {
        // Snapshot gate, not CAS — a concurrent founding death after
        // this read is race-acceptable: the slot sits unused in a
        // dead session and is reclaimed via `Arc<RpcSessionInner>`.
        if self.inner.shared.lifecycle.is_torn_down() {
            return Err(StatusCode::DeadObject);
        }
        self.inner
            .add_slot_inner_capped(transport, cap, false)
            .ok_or(StatusCode::FailedTransaction)
    }

    /// This session's full inner state — including
    /// the *connection slot pool* and the wire profile, not just
    /// [`SharedSession`]. The unified-model server attach path stores
    /// `Weak` of this in `RpcServer.sessions` so an id-echoing 2nd+
    /// connection [`add_incoming_slot_capped`](RpcSession::add_incoming_slot_capped)s
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
    ///
    /// The error keeps its [`RpcError`] form so the caller's log carries
    /// the wire-level reason (`StatusCode::RpcError` would flatten every
    /// cause into one opaque name — including the `"cci"` reject that
    /// names a profile mismatch).
    pub(crate) fn android13plus_accept_handshake(
        transport: Box<dyn RpcTransport>,
        server_max_version: u32,
    ) -> RpcResult<Android13PlusAccept> {
        let (codec, client_fd_mode, client_id, incoming) = {
            let mut io = RawTransportIo(transport.as_ref());
            server_accept_deferred_init(&mut io, server_max_version)?
        };
        Ok((transport, codec, client_fd_mode, client_id, incoming))
    }

    /// Server: build the accepted connection's session from a completed
    /// [`android13plus_accept_handshake`](RpcSession::android13plus_accept_handshake).
    /// Always a brand-new session: a later connection of the same session is
    /// attached through `add_incoming_slot_capped`, never by building a
    /// second `RpcSessionInner` over the same `SharedSession` — proxies minted
    /// by one inner are refused by another's `write_binder`.
    pub(crate) fn from_android13plus(
        transport: Box<dyn RpcTransport>,
        codec: Android13PlusCodec,
        client_fd_mode: u8,
        server_fd_unix: bool,
    ) -> RpcResult<RpcSession> {
        let negotiated = codec.version();
        let shared = Self::fresh_shared(AddressSpace::Acceptor)?;
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
    /// framing — reusing the existing per-session `RpcState` and
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
        Self::connect_android13plus_fd_with_id_hs(transport, max_version, fd_mode, session_id, None)
    }

    /// [`connect_android13plus_fd_with_id`](Self::connect_android13plus_fd_with_id)
    /// with a deadline on the handshake read. `None` blocks forever, which
    /// is what the public entry point passes.
    pub(crate) fn connect_android13plus_fd_with_id_hs(
        transport: Box<dyn RpcTransport>,
        max_version: u32,
        fd_mode: FileDescriptorTransportMode,
        session_id: &[u8],
        handshake_timeout: Option<Duration>,
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
            // Scoped so the deadline is cleared before `transport` moves
            // into the session below.
            let _hs = HandshakeDeadline::arm(transport.as_ref(), handshake_timeout)
                .map_err(StatusCode::from)?;
            let mut io = RawTransportIo(transport.as_ref());
            client_connect_with_id(&mut io, max_version, false, hdr_fd_mode, session_id)
                .map_err(|e| client_handshake_err(e, session_id.is_empty()))?
        };
        if !session_id.is_empty() {
            // Attach: no `RpcNewSessionResponse` and no server-written
            // `"cci"` acknowledge it, so confirm admission here rather
            // than hand the caller a session whose only connection the
            // peer already closed (see `confirm_attach`). A new session
            // (empty id) needs no probe — its `RpcNewSessionResponse`
            // *is* the acknowledgement, so that path stays byte- and
            // round-trip-identical.
            let _hs = HandshakeDeadline::arm(transport.as_ref(), handshake_timeout)
                .map_err(StatusCode::from)?;
            if let Err(e) = confirm_attach(transport.as_ref(), &codec, session_id) {
                log_attach_refused(&e);
                return Err(StatusCode::from(e));
            }
        }
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
            Self::android13plus_accept_handshake(transport, server_max_version)
                .map_err(StatusCode::from)?;
        // This wrapper has no callback-slot path; incoming-direction
        // attaches go through `super::RpcServer::serve_connection`.
        if incoming {
            return Err(StatusCode::BadType);
        }
        Self::from_android13plus(transport, codec, client_fd_mode, server_fd_unix)
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
    ///
    /// **On a client session this is NOT the peer's session id.** A
    /// client mints this value locally and never puts it on the wire —
    /// it exists so a client session that serves callbacks can answer
    /// `GET_SESSION_ID` — and the server's id is a different 32 bytes
    /// that only [`RpcSession::get_session_id`] (one round trip, AOSP
    /// `RpcSession::setupClient` → `readId()`) can tell you. Passing
    /// this accessor's value to an attach API
    /// ([`add_outgoing_connection_android13plus`](Self::add_outgoing_connection_android13plus),
    /// [`add_incoming_connection_android13plus_with_config`](Self::add_incoming_connection_android13plus_with_config),
    /// [`setup_unix_client_android13plus_with_id`](Self::setup_unix_client_android13plus_with_id),
    /// `ClientOptions::session_id`) is therefore always wrong; those
    /// entries refuse it (the peer never admits the connection, and the
    /// refusal is reported — see `confirm_attach`), but the value
    /// itself is indistinguishable from any other 32 random bytes, so
    /// read the id you echo from `get_session_id()`.
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
        // Propagate the real read error (v2 strict-position `BadValue`, a
        // stability short-read, `UnexpectedNull` for an actually-null root)
        // rather than squashing every failure into `UnexpectedNull`, which
        // hides the diagnostic — matching `get_session_id`'s no-squash policy.
        reply.read::<SIBinder>()
    }

    /// Server: process inbound messages until this connection's read
    /// reaches end of stream or the loop fails.
    ///
    /// When the loop ends — for either reason — every remote object
    /// reachable over this session is dead,
    /// so registered death recipients are fired here (AOSP
    /// `RpcState::sendObituaries` when a session's incoming threads
    /// end). This is the rsbinder death-detection point: a peer that
    /// linked a `DeathRecipient` (e.g. a client wanting to learn the
    /// server died) must be running this serve loop — faithful to
    /// AOSP's `getMaxIncomingThreads() >= 1` requirement for an RPC
    /// `linkToDeath`.
    ///
    /// `Ok(())` is the clean end only: this connection's read reached
    /// end of stream. Which side ended it is not part of that — the loop
    /// returns `Ok(())` when the peer closed the connection or went
    /// away, and equally when *this* side closed it, since
    /// [`RpcSession::shutdown`](RpcSession::shutdown) — and every other
    /// path that declares the session dead — shuts each slot's transport
    /// down, and a serve loop woken out of `recv` that way ends here.
    /// Every other end is an `Err`, including the case
    /// where this loop's own read never failed — when a nested call made
    /// from a handler on this same connection can no longer account for
    /// what the peer sends next (a frame that arrived and did not decode,
    /// or a read that failed without the guarantee that it stopped at a
    /// frame boundary), the connection is marked unreadable and the loop
    /// ends with [`StatusCode::DeadObject`] rather than interpreting
    /// bytes whose meaning is unknowable. A caller (or a log line) can
    /// therefore always tell the clean end from every other one — but not
    /// a live peer from a dead one: that `Err` is reached both when the
    /// peer is still up and only this end's view of the stream was lost,
    /// and when the peer itself went away in the middle of a frame.
    pub fn serve_blocking(&self) -> Result<()> {
        self.serve_blocking_on(Self::FOUNDING_SLOT_ID)
    }

    /// Serve a *specific* slot of the pool until its read reaches end of
    /// stream or the loop fails (the server worker's API — each accepted
    /// connection's worker drives the slot it was added as via
    /// `add_incoming_slot_capped`). The default single-connection
    /// [`serve_blocking`](RpcSession::serve_blocking) is exactly this
    /// on the founding slot (`FOUNDING_SLOT_ID`).
    ///
    /// The return contract is that of
    /// [`serve_blocking`](RpcSession::serve_blocking), which delegates
    /// here: `Ok(())` is the clean end only — end of stream on this
    /// connection, whether the peer ended it or this side did — and
    /// every other end is an `Err`.
    /// This is also where that contract's one surprising end is actually
    /// reached, since a handler runs on the slot it is served on: when a
    /// nested call made from such a handler over this same connection can
    /// no longer account for what the peer sends next (a frame that
    /// arrived and did not decode, or a read that failed without the
    /// guarantee that it stopped at a frame boundary), the slot is marked
    /// unreadable and the loop ends with [`StatusCode::DeadObject`] —
    /// which, as [`serve_blocking`](RpcSession::serve_blocking) states,
    /// implies nothing about whether the peer is still up.
    pub fn serve_blocking_on(&self, slot_id: u64) -> Result<()> {
        self.serve_blocking_on_inner(slot_id, false)
    }

    /// Like [`serve_blocking`](RpcSession::serve_blocking), but the
    /// handshake/admission read deadline armed before the call is left in
    /// place for the **first** frame only and cleared once that frame is
    /// read (or a clean EOF arrives). Used by the r34 server path, where
    /// there is no separate handshake: the first serve-loop frame *is* the
    /// first contact, so a connected-but-silent peer's worker must still be
    /// bounded by the deadline, while an established two-way session idles
    /// unbounded between requests after that first frame.
    pub fn serve_blocking_clearing_deadline_after_first(&self) -> Result<()> {
        self.serve_blocking_on_inner(Self::FOUNDING_SLOT_ID, true)
    }

    fn serve_blocking_on_inner(
        &self,
        slot_id: u64,
        clear_deadline_after_first: bool,
    ) -> Result<()> {
        // Read the slot's role now: a `RpcSession::shutdown` racing this
        // loop clears the pool, and a role read after the loop would come
        // back `false` for a slot that is simply gone.
        let client_incoming = self.inner.is_client_incoming_slot(slot_id);
        let result = {
            let mut r = Ok(());
            let mut first = clear_deadline_after_first;
            loop {
                match self.inner.serve_once_on_slot(slot_id) {
                    Ok(cont) => {
                        // First-frame-only deadline: once the first frame is
                        // read (or a clean EOF arrives), lift the admission
                        // deadline so subsequent idle waits are unbounded.
                        if first {
                            self.inner.clear_slot_read_timeout(slot_id);
                            first = false;
                        }
                        if cont {
                            continue;
                        }
                        break;
                    }
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
        //
        // A client's incoming (callback) slot never bumped `live_conns`,
        // so its exit must not decrement either; instead the session dies
        // once the **last** such slot is gone: the founding connection's
        // `Live(1)` is what the CAS consumes. Its role was read before the
        // loop. Eager death by design, *not* AOSP (whose
        // client-side `onSessionAllIncomingThreadsEnded` is a no-op) —
        // the trade-off is on
        // [`RpcUnixClientConfig::incoming_connections`].
        if !client_incoming && self.inner.shared.lifecycle.drop_connection() {
            self.inner.on_session_dead();
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
        if client_incoming
            && self.inner.incoming_slot_count() == 0
            && self.inner.shared.lifecycle.try_drop_sole_connection()
        {
            self.inner.on_session_dead();
        }
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

    /// Declare this session dead now: fire every cached proxy's
    /// `binder_died` and release every local object the peer held (AOSP
    /// `RpcState::clear`). Idempotent; subsequent transactions on proxies
    /// of this session fail with [`StatusCode::DeadObject`]. The
    /// connection count does not matter: a session several workers drive
    /// (a server session with attached connections) is ended the same
    /// way — every slot's transport is shut down and its workers exit.
    ///
    /// Normally death is detected on its own — a serve loop ending, or a
    /// transaction failing on a lost connection. This is the explicit
    /// form, and it is the **only** way to break the
    /// `session → local object → stored proxy → session` reference cycle
    /// for a session that has no serve loop and will never transact
    /// again: a service this endpoint handed to the peer may hold a proxy
    /// back into the same session, and proxies keep the session alive (see
    /// the type-level `# Lifetime` note). Call it when abandoning such a
    /// session.
    ///
    /// The threads serving this client's incoming (callback) connections
    /// (`RpcUnixClientConfig::incoming_connections`) are stopped and
    /// joined here — every slot's transport is shut down, which ends
    /// their serve loops — except a thread that calls `shutdown` from
    /// inside its own callback handler, which is left to finish on its
    /// own (joining it would deadlock). Unlike AOSP
    /// `RpcSession::shutdownAndWait`, a user-driven `serve_blocking` on
    /// the founding slot is not joined; it exits on its own once the
    /// transport is shut down.
    pub fn shutdown(&self) {
        self.inner.close();
        let me = std::thread::current().id();
        for (slot_id, handle) in self.inner.take_incoming_threads() {
            if handle.thread().id() == me {
                // `shutdown` from inside this connection's own dispatch:
                // the loop ends when the handler returns; dropping the
                // handle detaches it.
                log::debug!(
                    "RPC: shutdown from incoming connection {slot_id}'s own thread; not joined"
                );
                continue;
            }
            if handle.join().is_err() {
                log::warn!("RPC: incoming connection {slot_id} thread panicked");
            }
            self.inner.incoming_joined.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Set the client reply/handshake wait deadline. `None`
    /// (default) blocks forever.
    ///
    /// The same deadline bounds how long a call waits for a free
    /// connection slot when every slot is driven by another thread — a
    /// session served on one thread and transacted on another needs more
    /// than one connection, or its calls time out here without ever
    /// reaching the peer.
    ///
    /// `Some(Duration::ZERO)` is **not** a valid deadline — the reply wait
    /// arms it as `SO_RCVTIMEO`, which rejects a zero duration — so it is
    /// refused (logged) and treated as `None` rather than failing every
    /// transaction on this session.
    pub fn set_timeout(&self, timeout: Option<Duration>) {
        *self.inner.shared.timeout.lock().expect("timeout poisoned") = reject_zero_deadline(
            timeout,
            "RpcSession::set_timeout: a zero duration is not a valid deadline; ignoring",
        );
    }

    /// Record the read deadline the server armed on this session's
    /// serve-driven connections, so a reply deadline lifted on one of
    /// them is restored to it instead of cleared — see
    /// [`SharedSession::serve_read_deadline`].
    pub(crate) fn set_serve_read_deadline(&self, deadline: Option<Duration>) {
        *self
            .inner
            .shared
            .serve_read_deadline
            .lock()
            .expect("serve_read_deadline poisoned") = deadline;
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

    /// Client: connect to a Linux/Android abstract Unix-domain RPC server.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn setup_unix_client_abstract(name: &[u8]) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect_abstract(name)?;
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

    /// Client: connect to a Linux/Android abstract Unix-domain RPC
    /// server speaking the **android-13+ versioned wire**.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn setup_unix_client_android13plus_abstract(
        name: &[u8],
        max_version: u32,
    ) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect_abstract(name)?;
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
    ///
    /// A non-empty id makes this an *attach*, and the server's
    /// admission is confirmed before the session is returned (see
    /// `confirm_attach`) — a refused attach is an error here, not a
    /// session whose every call fails.
    pub fn setup_unix_client_android13plus_with_id(
        path: impl AsRef<std::path::Path>,
        max_version: u32,
        session_id: &[u8],
    ) -> Result<RpcSession> {
        Self::setup_unix_client_android13plus_with_config(
            RpcUnixClientConfig::path(path.as_ref(), max_version).session_id(session_id),
        )
    }

    /// Client: connect to a Unix-domain android-13+ server using a config object.
    pub fn setup_unix_client_android13plus_with_config(
        config: RpcUnixClientConfig,
    ) -> Result<RpcSession> {
        reject_zero_handshake_timeout(
            config.handshake_timeout,
            "RpcUnixClientConfig::handshake_timeout",
        )?;
        let local = config.outgoing_connections.max(1);
        let incoming = config.incoming_connections;
        // Fan-out and incoming connections are a session *owner*'s
        // business: an attach (echoed id) gets neither.
        if (local > 1 || incoming > 0) && !config.session_id.is_empty() {
            return Err(StatusCode::BadValue);
        }

        let session = RpcSession::connect_android13plus_fd_with_id_hs(
            Box::new(config.connect()?),
            config.max_version,
            config.fd_mode.unwrap_or(FileDescriptorTransportMode::None),
            config.session_id,
            config.handshake_timeout,
        )?;
        // Before `negotiate`/`get_session_id` below — those are ordinary
        // `client_transact` round trips and read this value when they run.
        if config.timeout.is_some() {
            session.set_timeout(config.timeout);
        }
        if local == 1 && incoming == 0 {
            // Single-connection path: byte-identical to
            // `setup_unix_client_android13plus`.
            return Ok(session);
        }

        // AOSP `setupClient` order: outgoing fan-out first, then the
        // incoming connections.
        let build = || -> Result<()> {
            let negotiated = if local > 1 {
                session.negotiate(local)?
            } else {
                1
            };
            let session_id = session.get_session_id()?;
            let fd_mode = session.fd_transport_mode();
            for _ in 1..negotiated {
                session.add_outgoing_connection_android13plus_transport(
                    || config.connect(),
                    config.max_version,
                    &session_id,
                    fd_mode,
                    config.handshake_timeout,
                )?;
            }
            for _ in 0..incoming {
                session.add_incoming_connection_android13plus_transport(
                    || config.connect(),
                    config.max_version,
                    &session_id,
                    fd_mode,
                    config.handshake_timeout,
                )?;
            }
            Ok(())
        };
        if let Err(e) = build() {
            // No degradation: the partial session is torn down here —
            // including any incoming threads already serving — since the
            // caller never gets a handle to do it.
            session.shutdown();
            return Err(e);
        }
        Ok(session)
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
    /// negotiated wire version must equal this session's, so
    /// `max_version` must be **at least**
    /// [`wire_protocol_version()`](Self::wire_protocol_version) —
    /// passing less can never attach ([`StatusCode::BadType`]), because
    /// an attach gets no version negotiation of its own (the founding
    /// connection already pinned it).
    ///
    /// The server's admission is confirmed before the new slot joins
    /// the pool (one `GET_SESSION_ID` round trip on the fresh
    /// connection, see `confirm_attach`): a refused attach — `session_id`
    /// unknown or stale, the server's `set_max_threads` outgoing-slot
    /// cap spent, server shutting down — is an error **here**, never a
    /// dead slot that fails some unrelated call later. Stay within
    /// [`negotiate()`](Self::negotiate) connections to avoid the cap.
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
        self.add_outgoing_connection_android13plus_with_config(
            RpcUnixClientConfig::path(path.as_ref(), max_version).session_id(session_id),
        )
    }

    /// Client multi-outgoing using a [`RpcUnixClientConfig`].
    pub fn add_outgoing_connection_android13plus_with_config(
        &self,
        config: RpcUnixClientConfig,
    ) -> Result<u64> {
        reject_zero_handshake_timeout(
            config.handshake_timeout,
            "RpcUnixClientConfig::handshake_timeout",
        )?;
        if config.outgoing_connections.max(1) != 1
            || config.incoming_connections != 0
            || config.session_id.len() != 32
        {
            return Err(StatusCode::BadValue);
        }
        let fd_mode = self.fd_transport_mode();
        if config.fd_mode.is_some_and(|mode| mode != fd_mode) {
            return Err(StatusCode::BadValue);
        }
        self.add_outgoing_connection_android13plus_transport(
            || config.connect(),
            config.max_version,
            config.session_id,
            fd_mode,
            config.handshake_timeout,
        )
    }

    fn add_outgoing_connection_android13plus_transport(
        &self,
        connect: impl FnOnce() -> Result<super::transport::UnixTransport>,
        max_version: u32,
        session_id: &[u8],
        fd_mode: FileDescriptorTransportMode,
        handshake_timeout: Option<Duration>,
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
        let hdr_fd_mode = if fd_mode == FileDescriptorTransportMode::Unix {
            FD_MODE_UNIX
        } else {
            FD_MODE_NONE
        };
        let t = connect()?;
        let codec = {
            let _hs = HandshakeDeadline::arm(&t, handshake_timeout).map_err(StatusCode::from)?;
            let mut io = RawTransportIo(&t);
            client_connect_with_id(&mut io, effective_max, false, hdr_fd_mode, session_id)
                .map_err(StatusCode::from)?
        };
        if codec.version() != session_version {
            // Server negotiated below us (older peer than the founding
            // connection's negotiation). A mixed-version pool would
            // silently route incompatible wire across one
            // `RpcSessionInner`; refuse instead.
            log::error!(
                "android-13+ RPC: this attach negotiated wire v{} but the session runs v{} — \
                 a caller-supplied `max_version` below the session's negotiated version can \
                 never attach; pass `RpcSession::wire_protocol_version()`",
                codec.version(),
                session_version
            );
            return Err(StatusCode::BadType);
        }
        {
            // Confirm the server admitted the attach before the slot
            // joins the pool: an unadmitted slot would sit there until
            // some unrelated later call drew it and failed
            // (see `confirm_attach`). The probe is a reply wait, so it
            // honors the session's `set_timeout` when no explicit
            // handshake timeout is configured.
            let probe_deadline =
                handshake_timeout.or(*self.inner.shared.timeout.lock().expect("timeout poisoned"));
            let _hs = HandshakeDeadline::arm(&t, probe_deadline).map_err(StatusCode::from)?;
            if let Err(e) = confirm_attach(&t, &codec, session_id) {
                log_attach_refused(&e);
                return Err(StatusCode::from(e));
            }
        }
        self.inner
            .add_outgoing_slot(Box::new(t))
            .ok_or(StatusCode::DeadObject)
    }

    /// Open one *additional* **incoming (callback) connection** to the
    /// android-13+ server session this client founded and serve it on a
    /// thread owned by this session — the manual, one-at-a-time form of
    /// [`RpcUnixClientConfig::incoming_connections`] (AOSP
    /// `RpcSession::addIncomingConnection`). `config` must carry this
    /// session's id (`get_session_id()`), no fan-out and no incoming
    /// count of its own; the server adds the connection as a slot it
    /// sends on. Returns the new slot id.
    ///
    /// Profile uniformity is enforced as for the outgoing attach
    /// (R34 ⇒ `BadType`; a server that negotiates the attach below the
    /// founding version ⇒ `BadType`). The server refusing the attach
    /// (its callback-slot budget, `2 * set_max_threads`, is spent)
    /// surfaces as a handshake error.
    pub fn add_incoming_connection_android13plus_with_config(
        &self,
        config: RpcUnixClientConfig,
    ) -> Result<u64> {
        reject_zero_handshake_timeout(
            config.handshake_timeout,
            "RpcUnixClientConfig::handshake_timeout",
        )?;
        if config.outgoing_connections.max(1) != 1
            || config.incoming_connections != 0
            || config.session_id.len() != 32
        {
            return Err(StatusCode::BadValue);
        }
        let fd_mode = self.fd_transport_mode();
        if config.fd_mode.is_some_and(|mode| mode != fd_mode) {
            return Err(StatusCode::BadValue);
        }
        self.add_incoming_connection_android13plus_transport(
            || config.connect(),
            config.max_version,
            config.session_id,
            fd_mode,
            config.handshake_timeout,
        )
    }

    fn add_incoming_connection_android13plus_transport(
        &self,
        connect: impl FnOnce() -> Result<super::transport::UnixTransport>,
        max_version: u32,
        session_id: &[u8],
        fd_mode: FileDescriptorTransportMode,
        handshake_timeout: Option<Duration>,
    ) -> Result<u64> {
        // Only the side that connected owns incoming threads (a server
        // session's callback slots come from the peer's attaches).
        if self.inner.shared.space() != AddressSpace::Initiator {
            return Err(StatusCode::InvalidOperation);
        }
        if session_id.len() != 32 {
            return Err(StatusCode::BadValue);
        }
        let session_version = match &self.inner.profile {
            WireProfile::Android13Plus(c) => c.version(),
            WireProfile::R34(_) => return Err(StatusCode::BadType),
        };
        if self.inner.shared.lifecycle.is_torn_down() {
            return Err(StatusCode::DeadObject);
        }
        let effective_max = max_version.min(session_version);
        let hdr_fd_mode = if fd_mode == FileDescriptorTransportMode::Unix {
            FD_MODE_UNIX
        } else {
            FD_MODE_NONE
        };
        let t = connect()?;
        let codec = {
            // Cleared before the slot is pushed: this transport gets a
            // serve loop, which a lingering read deadline would break.
            let _hs = HandshakeDeadline::arm(&t, handshake_timeout).map_err(StatusCode::from)?;
            let mut io = RawTransportIo(&t);
            // `incoming = true`: the header carries the INCOMING bit and
            // this side *reads* the server's `"cci"`.
            client_connect_with_id(&mut io, effective_max, true, hdr_fd_mode, session_id)
                .map_err(StatusCode::from)?
        };
        if codec.version() != session_version {
            return Err(StatusCode::BadType);
        }
        // Dropping `t` on refusal closes the socket, so the peer sees EOF
        // on a connection this session will never serve.
        let slot_id = self
            .inner
            .add_slot_inner(Box::new(t), SlotRole::Incoming)
            .ok_or(StatusCode::DeadObject)?;
        let inner = Arc::clone(&self.inner);
        // Bump *before* the spawn so the counter is never observed low
        // between here and the thread's first instruction.
        self.inner.incoming_live.fetch_add(1, Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name(format!("rsbinder-rpc-in-{slot_id}"))
            .spawn(move || {
                let session = RpcSession::wrap_inner(inner);
                if let Err(e) = session.serve_blocking_on(slot_id) {
                    log::debug!("RPC: incoming connection {slot_id} ended: {e:?}");
                }
                // Last act of the thread — see `incoming_live`.
                session.inner.incoming_live.fetch_sub(1, Ordering::SeqCst);
            });
        match spawned {
            Ok(handle) => {
                self.inner
                    .incoming_threads
                    .lock()
                    .expect("incoming_threads poisoned")
                    .push((slot_id, handle));
                Ok(slot_id)
            }
            Err(e) => {
                // A slot nobody reads would make every server send on it
                // hang: retire it before reporting.
                log::error!("RPC: incoming connection thread spawn failed: {e}");
                self.inner.incoming_live.fetch_sub(1, Ordering::SeqCst);
                self.inner.remove_slot(slot_id);
                Err(StatusCode::from(e))
            }
        }
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
        Self::setup_unix_client_android13plus_with_config(
            RpcUnixClientConfig::path(path.as_ref(), max_version)
                .outgoing_connections(local_max_outgoing),
        )
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
    /// `AF_INET`/`AF_INET6` → `TcpDebugTransport`
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

        // (c) Bound the handshake. The fd comes from an Accessor returned
        //     by the service manager (`resolve_accessor`), i.e. a peer we
        //     do not trust. After clearing `O_NONBLOCK` above the handshake
        //     `read`/`write_all` are fully blocking, so a peer that accepts
        //     the connection but never sends (or never reads) the handshake
        //     would otherwise hang `getService`/`get_root` forever — there
        //     is no `SO_RCVTIMEO` here (the per-call session timeout only
        //     applies inside `client_transact`). Arm a deadline on both
        //     directions for the handshake, then clear it on the
        //     established session so steady-state I/O is unbounded as
        //     before. Best-effort: a set failure just means no deadline.
        const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
        let _ = transport.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
        let _ = transport.set_write_timeout(Some(HANDSHAKE_TIMEOUT));

        // (d) Run the android-13+ versioned handshake. No FD-over-RPC —
        //     the Accessor-fd carries no fd-mode metadata of its own and
        //     the consumer (the eventual proxy returned by `get_root`)
        //     drives any later FD passing through the negotiated wire
        //     directly.
        let session = RpcSession::connect_android13plus_fd(
            transport,
            max_version,
            FileDescriptorTransportMode::None,
        )?;
        session.inner.clear_handshake_timeouts();
        Ok(session)
    }

    /// Test/diagnostic: number of connection slots in this session's pool
    /// (founding + fan-out + incoming, or founding + attaches + callback
    /// slots on a server session). Not a stable API.
    #[doc(hidden)]
    pub fn __slot_count(&self) -> usize {
        self.inner.slot_count()
    }

    /// Test/diagnostic: incoming-connection threads whose `JoinHandle`
    /// this session still holds. `shutdown` takes the whole set before
    /// joining any of it, so this drops to zero the moment `shutdown`
    /// starts — use [`__incoming_thread_live_count`](Self::__incoming_thread_live_count)
    /// to observe the join itself.
    #[doc(hidden)]
    pub fn __incoming_thread_count(&self) -> usize {
        self.inner
            .incoming_threads
            .lock()
            .expect("incoming_threads poisoned")
            .len()
    }

    /// Test/diagnostic: incoming-connection threads still running.
    #[doc(hidden)]
    pub fn __incoming_thread_live_count(&self) -> usize {
        self.inner.incoming_live.load(Ordering::SeqCst)
    }

    /// Test/diagnostic: incoming-connection threads `shutdown` has
    /// joined. Stays 0 if they are detached instead.
    #[doc(hidden)]
    pub fn __incoming_thread_joined_count(&self) -> usize {
        self.inner.incoming_joined.load(Ordering::SeqCst)
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

    /// A zero handshake deadline is refused at the one point every
    /// arming site funnels through, so no transport can be handed a
    /// deadline it is documented to reject — and no implementation that
    /// accepts one can leave the phase unbounded instead.
    ///
    /// **Mutant gate**: dropping the guard in `HandshakeDeadline::arm`
    /// makes this `Ok` for any transport whose `set_read_timeout`
    /// tolerates zero, and turns the caller's bound into no bound.
    #[test]
    fn zero_handshake_deadline_is_refused_before_it_reaches_a_transport() {
        let (a, _b) = super::super::transport::MemTransport::pair();
        assert!(
            HandshakeDeadline::arm(&a, Some(Duration::ZERO)).is_err(),
            "a zero duration is not a deadline"
        );
        // The two shapes that are deadlines still arm.
        assert!(HandshakeDeadline::arm(&a, None).is_ok());
        assert!(HandshakeDeadline::arm(&a, Some(Duration::from_millis(50))).is_ok());
    }

    /// The same value refused where the caller can still be named: the
    /// builder's setup and attach entries report it as the `BadValue`
    /// every other invalid field of that builder yields, before any
    /// connect happens.
    #[test]
    fn zero_handshake_timeout_is_bad_value_at_the_config_entries() {
        let path = std::path::Path::new("/nonexistent/rsb-zero-handshake.sock");
        assert_eq!(
            RpcSession::setup_unix_client_android13plus_with_config(
                RpcUnixClientConfig::path(path, 2).handshake_timeout(Duration::ZERO),
            )
            .err(),
            Some(StatusCode::BadValue),
            "rejected before the connect — the path is never touched"
        );
    }

    /// A **nested** call that cannot decode a frame must not leave the
    /// connection readable for the frame that owns it.
    ///
    /// A reentrant frame borrows the outer frame's slot, so it cannot
    /// retire it, and marking a stale reply does not help: once a frame
    /// has come off the wire undecoded, whether this call's `REPLY` is
    /// still coming is exactly what is unknown. If it is, the outer frame
    /// reads it and — `WireReply` carrying no transaction id — takes it as
    /// its own answer. So the slot is poisoned (and the connection shut
    /// down), which is what the non-reentrant arm achieves by retiring the
    /// slot.
    ///
    /// Two things are set up directly rather than driven through a peer:
    /// the `DRIVING` marker (what an outer frame's `find_conn` leaves
    /// behind), and the undecodable frame, which is queued in the socket
    /// before the call so the test needs no second thread and no timing
    /// assumption. Reaching this through a real nested dispatch would
    /// need a peer that both calls back into this session and violates
    /// the wire.
    ///
    /// **Mutant gate**: the flag *and* the refusal it exists for. Drop
    /// the `mark_slot_poisoned` call and the flag assertion fails; drop
    /// the poison check in `serve_once_on_slot` and the slot reads on as
    /// if nothing happened — the state in which the frame that owns the
    /// slot would go on to take the peer's next frame for its own.
    /// `shutdown` alone gates neither: what it does to already-received
    /// bytes is platform- and backend-dependent (Linux keeps the kernel
    /// queue, a transport may hold a buffered leftover, the default impl
    /// is a no-op). The reply wait's half of the refusal needs a live
    /// connection, so it is gated by
    /// `a_poisoned_slot_is_refused_by_the_reply_wait`.
    #[test]
    fn a_nested_call_that_loses_the_stream_makes_the_slot_unreadable() {
        use crate::rpc::wire_android13::write_aosp_message;
        use std::os::unix::net::UnixStream;

        let (client_fd, peer_fd) = unix_socketpair_fd();
        let mut peer = UnixStream::from(peer_fd);
        let session = RpcSession::with_profile(
            Box::new(
                super::super::transport::UnixTransport::from_stream(UnixStream::from(client_fd))
                    .expect("transport"),
            ),
            AddressSpace::Initiator,
            WireProfile::Android13Plus(Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2")),
        )
        .expect("session");

        // Queued before the call: the reply wait reads it as soon as the
        // request is out. Command 7 is not one of AOSP's three
        // (`TRANSACT`/`REPLY`/`DEC_STRONG`), so the frame is well-formed
        // to the framing reader and undecodable to the codec.
        let mut undecodable = [0u8; 16];
        undecodable[0..4].copy_from_slice(&7u32.to_le_bytes());
        write_aosp_message(&mut peer, &undecodable).expect("queue the undecodable frame");

        // Reentrant: this thread already drives the session's only slot,
        // exactly as an outer `client_transact` would have left it.
        let (slot_id, slot_transport) = {
            let st = session.inner.conn_state.lock().expect("conn_state");
            (st.slots[0].id, Arc::clone(&st.slots[0].transport))
        };
        let sess_ptr = &*session.inner as *const RpcSessionInner as usize;
        // RAII: a failing assertion below must not leave a dead session's
        // entry on this thread's `DRIVING` stack for the next test to
        // inherit (libtest runs tests inline under `--test-threads=1`).
        struct DrivingMark;
        impl Drop for DrivingMark {
            fn drop(&mut self) {
                DRIVING.with(|d| {
                    d.borrow_mut().pop();
                });
            }
        }
        DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
        let _mark = DrivingMark;

        let err = session
            .inner
            .client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetSessionId.code(),
                &Parcel::new(),
                0,
            )
            .expect_err("an undecodable frame fails the nested call");
        assert_eq!(err, StatusCode::RpcError, "protocol violation");

        // The slot is still in the pool — a reentrant frame does not
        // retire what the outer frame owns — but its connection is gone,
        // so nothing more can be read from or written to it.
        assert_eq!(
            session
                .inner
                .conn_state
                .lock()
                .expect("conn_state")
                .slots
                .len(),
            1,
            "a reentrant frame must not retire the outer frame's slot"
        );
        assert!(
            session.inner.slot_poisoned(slot_id),
            "the borrowed slot must be marked unreadable for its owner"
        );
        assert!(
            slot_transport.send_raw(b"x").is_err(),
            "the borrowed connection must be shut down, not left usable"
        );
        // The flag is only worth having if a reader honors it: the serve
        // loop must refuse the slot rather than read whatever comes next.
        assert_eq!(
            session
                .inner
                .serve_once_on_slot(slot_id)
                .expect_err("a poisoned slot must not be served"),
            StatusCode::DeadObject,
            "the serve loop must refuse the slot, not read another frame on it"
        );
    }

    /// The other reader of a poisoned slot — the reply wait inside
    /// [`RpcSessionInner::client_transact`] — must refuse it too, and the
    /// refusal must not rest on the connection being unusable.
    ///
    /// Here the connection is perfectly healthy and a well-formed `REPLY`
    /// is already waiting on it: exactly the shape of the theft the poison
    /// exists to stop, since `WireReply` carries no transaction id and the
    /// waiting frame cannot tell the peer's answer to an abandoned nested
    /// call from its own. The slot is poisoned directly rather than
    /// through a nested call, because a nested call also shuts the
    /// connection down and would hide which of the two mechanisms did the
    /// work.
    ///
    /// **Mutant gate**: drop the poison check at the top of the reply
    /// loop and this call returns `Ok(Some(_))` — the queued reply handed
    /// back as this call's answer.
    #[test]
    fn a_poisoned_slot_is_refused_by_the_reply_wait() {
        use crate::rpc::wire_android13::write_aosp_message;
        use std::os::unix::net::UnixStream;

        let (client_fd, peer_fd) = unix_socketpair_fd();
        let mut peer = UnixStream::from(peer_fd);
        let session = RpcSession::with_profile(
            Box::new(
                super::super::transport::UnixTransport::from_stream(UnixStream::from(client_fd))
                    .expect("transport"),
            ),
            AddressSpace::Initiator,
            WireProfile::Android13Plus(Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2")),
        )
        .expect("session");

        let slot_id = {
            let st = session.inner.conn_state.lock().expect("conn_state");
            st.slots[0].id
        };

        // Queued before the call, so the reply wait would find it the
        // moment the request is out — no second thread, no timing.
        let reply = session
            .inner
            .profile
            .codec()
            .encode_reply(&WireReply {
                status: 0,
                data: Vec::new(),
                object_positions: Vec::new(),
            })
            .expect("encode a REPLY frame");
        write_aosp_message(&mut peer, &reply).expect("queue the reply frame");

        session.inner.mark_slot_poisoned(slot_id);

        assert_eq!(
            session
                .inner
                .client_transact(
                    RpcAddress::zero(),
                    SpecialTransaction::GetSessionId.code(),
                    &Parcel::new(),
                    0,
                )
                .expect_err("a poisoned slot must not be read again"),
            StatusCode::DeadObject,
            "the reply wait must refuse the slot, not decode the frame waiting on it"
        );
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
        // Wait for the bridge to clear the flag (it does so BEFORE the
        // family dispatch, which is BEFORE the read) — polling with a
        // generous bound rather than a fixed sleep, which a loaded CI
        // box can outlast.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let observed = loop {
            let fl = rustix::fs::fcntl_getfl(observer.as_fd()).expect("getfl observer");
            if !fl.contains(rustix::fs::OFlags::NONBLOCK) || std::time::Instant::now() > deadline {
                break fl;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
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

    /// A `DEC_STRONG` must never take a **serve-driven** slot from the
    /// scan. AOSP looks `mIncoming` up with `available = nullptr`, so only
    /// a connection the calling thread already holds can match there; a
    /// free one is never picked. Writing to one is not a delayed frame but
    /// a send that blocks once the peer's socket buffer fills — the peer
    /// reads that connection only inside its own reply wait — while the
    /// sender holds the slot's `exclusive_tid`, locking its serve loop out.
    ///
    /// A serve loop holds its slot across `recv`, so this window is only
    /// reachable between two of the worker's messages; the unit form below
    /// makes it deterministic.
    #[test]
    fn dec_strong_never_scans_a_serve_driven_slot() {
        use crate::rpc::transport::MemTransport;
        // Server-side session: the founding slot is `Incoming` (serve
        // driven) and, with no worker running, genuinely free.
        let (t0, _p0) = MemTransport::pair();
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("build session");
        assert_eq!(session.inner.slot_count(), 1);

        assert!(
            matches!(
                session.inner.find_conn_lenient(),
                Err(StatusCode::FailedTransaction)
            ),
            "a DEC_STRONG must not claim the free serve-driven slot"
        );
        assert!(
            session.inner.try_find_conn().is_none(),
            "the non-blocking fast path must not claim it either"
        );
        assert!(
            session.inner.find_conn_for_reaper().is_none(),
            "the reaper must skip rather than take it (or park holding the session)"
        );

        // An `Outgoing` (callback) slot is what makes the send possible.
        let (t, _p) = MemTransport::pair();
        session
            .add_callback_slot(Box::new(t), 2)
            .expect("callback slot");
        assert!(
            session.inner.find_conn_lenient().is_ok(),
            "with an outgoing slot the DEC_STRONG goes out normally"
        );
    }

    /// The incoming callback-slot admission cap is enforced **atomically**
    /// inside `add_callback_slot` (check-and-push under the `conn_state`
    /// lock), so concurrent attaches cannot clear an advisory pre-check and
    /// overshoot it. Callback slots are never serve-reclaimed, so an
    /// unbounded pool would be a peer-driven fd/memory DoS. This single-
    /// threaded gate proves the cap is hard (never exceeded) and refuses
    /// past it.
    #[test]
    fn callback_slot_cap_is_enforced() {
        use crate::rpc::transport::MemTransport;
        // Server-side post-handshake session form (no wire I/O).
        let (t0, _p0) = MemTransport::pair();
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("build session");

        // The cap counts callback (`Outgoing`) slots only — the served
        // founding slot is not part of the budget.
        let base = session.inner.slot_count();
        let cap = 2;
        let mut peers = Vec::new();
        let mut admitted = 0usize;
        let mut refused = 0usize;
        for _ in 0..5 {
            let (t, p) = MemTransport::pair();
            peers.push(p); // keep peer halves alive
            match session.add_callback_slot(Box::new(t), cap) {
                Ok(_) => admitted += 1,
                Err(_) => refused += 1,
            }
        }
        assert_eq!(
            session.inner.slot_count(),
            base + cap,
            "pool never exceeds founding + cap"
        );
        assert_eq!(admitted, cap, "exactly cap callback slots admitted");
        assert!(refused >= 1, "attaches past the cap are refused");
    }

    /// A callback attach whose connection-init write fails must leave no
    /// trace in the pool. Left behind, the dead slot would (a) hold a
    /// place in the `2 * max_threads` callback budget until the whole
    /// session dies, so a client that retried the attach could exhaust it,
    /// and (b) be picked *first* by `find_conn` (lowest index among free
    /// `Outgoing` slots), failing the next legitimate callback before any
    /// healthy slot is tried.
    #[test]
    fn callback_slot_init_failure_retires_the_slot() {
        use crate::rpc::transport::MemTransport;
        let (t0, _p0) = MemTransport::pair();
        let codec = Android13PlusCodec::android14_15();
        let session = RpcSession::from_android13plus(Box::new(t0), codec, FD_MODE_NONE, false)
            .expect("build session");
        let base = session.inner.slot_count();

        // `mem` is frame-based and carries no raw handshake byte stream,
        // so the `"cci"` write fails on it — which is exactly the case
        // under test (a client that closed the socket right after its
        // attach header leaves the server's init write failing too).
        let (t, _p) = MemTransport::pair();
        assert!(
            session
                .add_callback_slot_and_init(Box::new(t), 2, &codec)
                .is_err(),
            "a failed connection-init must be reported"
        );
        assert_eq!(
            session.inner.slot_count(),
            base,
            "a failed connection-init must leave no slot behind"
        );

        // …and the callback budget is intact: two slots still fit under
        // the same cap the failed attach would otherwise have eaten into.
        let mut peers = Vec::new();
        for _ in 0..2 {
            let (t, p) = MemTransport::pair();
            peers.push(p);
            session
                .add_callback_slot(Box::new(t), 2)
                .expect("callback budget still has room");
        }
        assert_eq!(session.inner.slot_count(), base + 2);
    }

    /// The teardown gate lives in the same critical section as the cap:
    /// `on_session_dead` empties the pool under the `conn_state` lock, but
    /// only after firing obituaries and running user `Drop` code, so a
    /// gate read outside the lock would still see `Live` and push a slot
    /// onto a session that is already dying — which the caller would then
    /// confirm to the peer.
    #[test]
    fn callback_slot_refused_on_torn_down_session() {
        use crate::rpc::transport::MemTransport;
        let (t0, _p0) = MemTransport::pair();
        let codec = Android13PlusCodec::android14_15();
        let session = RpcSession::from_android13plus(Box::new(t0), codec, FD_MODE_NONE, false)
            .expect("build session");
        session.shutdown();

        let (t, _p) = MemTransport::pair();
        assert!(
            session
                .inner
                .add_slot_inner_capped(Box::new(t), 2, false)
                .is_none(),
            "no slot may be pushed onto a torn-down session"
        );
        assert_eq!(session.inner.slot_count(), 0, "the dead pool stays empty");
    }

    /// A proxy minted by one session names a node in *that* peer's address
    /// space; written into another session's parcel it would be resolved by
    /// an unrelated peer against its own nodes. AOSP `onBinderLeaving`
    /// refuses with `INVALID_OPERATION`; so does `write_binder`.
    #[test]
    fn proxy_of_another_session_is_refused() {
        use crate::rpc::proxy::RpcProxy;
        use crate::rpc::transport::MemTransport;
        let make = || {
            let (t, p) = MemTransport::pair();
            let s = RpcSession::from_android13plus(
                Box::new(t),
                Android13PlusCodec::android14_15(),
                FD_MODE_NONE,
                false,
            )
            .expect("build session");
            (s, p)
        };
        let (a, _pa) = make();
        let (b, _pb) = make();
        let mut counter = 0u64;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let proxy_of_a = SIBinder::new(Arc::new(RpcProxy::new(addr, a.inner.clone())))
            .expect("SIBinder::new(RpcProxy)");

        let mut parcel = Parcel::new();
        assert_eq!(
            b.inner.write_binder(Some(&proxy_of_a), &mut parcel),
            Err(StatusCode::InvalidOperation),
            "another session's proxy must not be addressed on this wire"
        );
        let mut parcel = Parcel::new();
        a.inner
            .write_binder(Some(&proxy_of_a), &mut parcel)
            .expect("the owning session writes its own proxy back");
    }
}
