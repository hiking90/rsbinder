// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `Android13PlusCodec` — the android-13+ *versioned* RPC wire
//! (subplan 2-5b), an **additive** [`WireCodec`](super::wire::WireCodec)
//! behind the same trait as [`R34Codec`](super::wire::R34Codec).
//! `R34Codec` is byte-unchanged; nothing here touches the kernel path.
//!
//! One codec, **version-keyed** — exactly AOSP's own design
//! (`RpcWireReply::wireSize(protocolVersion)`):
//!
//! * **v0** = android-13 (`RPC_WIRE_PROTOCOL_VERSION = 0`)
//! * **v1** = android-14 **and** android-15
//!   (`RPC_WIRE_PROTOCOL_VERSION = 1`; their `RpcWireFormat.h` is
//!   byte-identical to each other)
//! * **v2** = android-16 (`RPC_WIRE_PROTOCOL_VERSION = 2`,
//!   `RPC_HEADER_INCLUDES_BINDER_POSITIONS`; subplan 2-8)
//!
//! The negotiated version is selected at runtime by the connection
//! handshake (`RpcConnectionHeader`/`RpcNewSessionResponse`). r34
//! (android-12, pre-versioning, 32-byte address, no handshake) stays a
//! separate codec.
//!
//! # v1 ≡ v2 framing (subplan 2-8 §0.3, verified vs `android-16.0.0_r4`)
//!
//! v2's wire-protocol delta is **not** a framing change. A full sweep
//! of `RpcState.cpp` + `RpcWireFormat.h` (android-16.0.0_r4) shows
//! **zero** `>= RPC_HEADER_INCLUDES_BINDER_POSITIONS` branches in the
//! framing layer — every wire branch is v0-vs-v1+
//! (`RpcWireReply::wireSize` = `v0?4:20`, the `>= EXPLICIT_PARCEL_SIZE`
//! object-table split). The only `INCLUDES_BINDER_POSITIONS` uses are
//! in `Parcel.cpp` (`flattenBinder`/`unflattenBinder`/
//! `rpcSetDataReference`): whether a *binder* (vs only an FD) position
//! is recorded in `mObjectPositions`. So v1 and v2 share **one
//! framing path**; the codec is version-agnostic about the object
//! table's *contents* (it just frames the trailing `u32[]`), and the
//! v1↔v2 distinction lives entirely in the Parcel position producer
//! (subplan 2-8 Phase B). A no-object parcel ⇒ empty table ⇒
//! byte-identical v1/v2 wire (AC-8.2, structural v1 no-regression).
//!
//! # Spec of record
//!
//! Extracted byte-exact from AOSP `frameworks/native`
//! (`android-13.0.0_r84` for v0, `android-15.0.0_r36` == v1 ==
//! `android-14.0.0_r75`), device-free — exactly the route 2-5a used
//! for r34. Source files: `libs/binder/RpcWireFormat.h`,
//! `include/binder/RpcSession.h`, `RpcSession.cpp`, `RpcState.cpp`,
//! `tests/binderRpcWireProtocolTest.cpp`.
//!
//! ## Unchanged across r34 / v0 / v1
//!
//! `RpcWireHeader` (16 B `{u32 command; u32 bodySize;
//! u32 reserved[2]}`), the command enum
//! (`TRANSACT=0/REPLY=1/DEC_STRONG=2`), the zero-address special
//! transacts (`GET_ROOT=0/GET_MAX_THREADS=1/GET_SESSION_ID=2`).
//!
//! ## r34 → v0 (android-13)
//!
//! * `RpcWireAddress`: `u8[32]` → `{u32 options; u32 address}` = **8 B**
//!   (`CREATED=1<<0`, `FOR_SERVER=1<<1`).
//! * `RpcWireTransaction`: 64 B fixed → **40 B**
//!   (`addr(8)|code|flags|asyncNumber|reserved[4]`).
//! * `RpcDecStrong`: address-only → **16 B**
//!   (`addr(8)|u32 amount|u32 reserved`).
//! * bare `int32 RPC_SESSION_ID_NEW=-1` preamble → versioned
//!   connection handshake.
//!
//! ## v0 → v1 (android-14 / android-15)
//!
//! * `RpcWireReply`: v0 = **4 B** (`i32 status`) → v1 = **20 B**
//!   (`i32 status; u32 parcelDataSize; u32 reserved[3]`). AOSP's
//!   `RpcWireReply::wireSize(version)` is `v0 ⇒ 4`, `v1 ⇒ 20`.
//! * `RpcWireTransaction` (still 40 B): the first `reserved` word
//!   becomes `u32 parcelDataSize` (size of the Parcel data that
//!   follows), then `reserved[3]`.
//! * `RpcConnectionHeader` (still 16 B): a reserved byte becomes
//!   `u8 fileDescriptorTransportMode` (`NONE=0/UNIX=1/TRUSTY=2`) —
//!   FD mode is negotiated *in the connection header* at v1, not via
//!   the separate `GET_FD_MODE` special transact rsbinder/2-7 uses.
//!
//! ## v1 → v2 (android-16, subplan 2-8)
//!
//! **No struct/framing change.** `RpcWireFormat.h` is byte-identical to
//! v1 (`RpcWireReply::wireSize` still `v0?4:20`; `RpcWireTransaction`
//! still 40 B with `parcelDataSize`). The only delta is that the
//! trailing object table (`mObjectPositions`, framed as a `u32[]` after
//! the parcel data — already present at v1 for FD positions) now also
//! carries **binder** positions. That is a Parcel-producer concern
//! (subplan 2-8 Phase B), not a codec/framing concern: the codec
//! frames the `u32[]` identically for v1 and v2.
//!
//! Version constants (`include/binder/RpcSession.h` @
//! **android-16.0.0_r4**): `RPC_WIRE_PROTOCOL_VERSION = 2`,
//! `RPC_WIRE_PROTOCOL_VERSION_NEXT = 3`,
//! `..._EXPERIMENTAL = 0xF000_0000`,
//! `..._RPC_HEADER_FEATURE_EXPLICIT_PARCEL_SIZE = 1`,
//! `..._RPC_HEADER_INCLUDES_BINDER_POSITIONS = 2`.
//! `setProtocolVersion` rejects `version >= _NEXT && version !=
//! _EXPERIMENTAL` — so a peer that supports up to v2 accepts
//! {0, 1, 2, EXPERIMENTAL}.
//!
//! ## Scope (2-5b, device-free)
//!
//! Delivers the **spec-conformance** half (byte-exact codec + golden
//! vectors from `RpcWireFormat.h`), the gating-equivalent route 2-5a
//! used for r34. Wiring the handshake into `RpcSession` against a
//! *live* android-13/14/15 RPC peer (G4), and matching a real peer's
//! `RpcState` node-id / `FOR_SERVER` address semantics, remain gated —
//! RPC_STATUS §2-5b. The Parcel-body layer (AOSP `kCurrentRepr`) is a
//! separate conformance tracked there.

use std::io::{Read, Write};

use super::address::RpcAddress;
use super::transport::MAX_FRAME_LEN;
use super::wire::{WireCodec, WireMessage, WireReply, WireTransaction};
use super::{RpcError, RpcResult};

/// `RpcWireHeader` size (unchanged r34 / v0 / v1).
const WIRE_HEADER_LEN: usize = 16;
/// android-13+ `RpcWireAddress` size (`u32 options; u32 address`).
pub const A13_ADDR_LEN: usize = 8;
/// `RpcWireTransaction` fixed prefix (v0 & v1 are both 40 B; only the
/// first reserved word's *meaning* differs — `parcelDataSize` at v1).
pub const A13_TXN_FIXED_LEN: usize = A13_ADDR_LEN + 4 + 4 + 8 + 16; // = 40
/// `RpcDecStrong` size (`addr(8) + amount(4) + reserved(4)`).
pub const A13_DEC_STRONG_LEN: usize = A13_ADDR_LEN + 4 + 4; // = 16
/// `RpcConnectionHeader` size (v0 & v1 both 16 B).
pub const A13_CONN_HEADER_LEN: usize = 16;
/// `RpcNewSessionResponse` size.
pub const A13_NEW_SESSION_RESP_LEN: usize = 8;
/// `RpcOutgoingConnectionInit` size.
pub const A13_CONN_INIT_LEN: usize = 8;

/// `RpcWireHeader.command` (unchanged).
const CMD_TRANSACT: u32 = 0;
const CMD_REPLY: u32 = 1;
const CMD_DEC_STRONG: u32 = 2;

/// `RPC_WIRE_ADDRESS_OPTION_*` (RpcWireFormat.h).
const ADDR_OPTION_CREATED: u32 = 1 << 0;
const ADDR_OPTION_FOR_SERVER: u32 = 1 << 1;

/// `RPC_CONNECTION_OPTION_INCOMING` (default is outgoing).
pub const CONN_OPTION_INCOMING: u8 = 0x1;

/// `RPC_CONNECTION_INIT_OKAY = "cci"` (+ NUL ⇒ 4 wire bytes).
pub const CONN_INIT_OKAY: [u8; 4] = *b"cci\0";

/// android-13 stable wire protocol version (`RPC_WIRE_PROTOCOL_VERSION`).
pub const PROTOCOL_V0: u32 = 0;
/// android-14 / android-15 stable wire protocol version
/// (`RPC_WIRE_PROTOCOL_VERSION_RPC_HEADER_FEATURE_EXPLICIT_PARCEL_SIZE`).
pub const PROTOCOL_V1: u32 = 1;
/// android-16 stable wire protocol version
/// (`RPC_WIRE_PROTOCOL_VERSION_RPC_HEADER_INCLUDES_BINDER_POSITIONS`;
/// subplan 2-8). Framing is byte-identical to [`PROTOCOL_V1`]; the
/// only delta is that binder positions also enter the object table
/// (a Parcel-producer concern — Phase B).
pub const PROTOCOL_V2: u32 = 2;
/// Highest version this codec implements (android-16.0.0_r4
/// `RPC_WIRE_PROTOCOL_VERSION`).
pub const SUPPORTED_MAX_VERSION: u32 = PROTOCOL_V2;
/// `RPC_WIRE_PROTOCOL_VERSION_NEXT` (android-16.0.0_r4) — the first
/// version rsbinder cannot speak; `setProtocolVersion` rejects
/// `>= _NEXT` (unless `_EXPERIMENTAL`).
pub const RPC_WIRE_PROTOCOL_VERSION_NEXT: u32 = 3;
/// `RPC_WIRE_PROTOCOL_VERSION_EXPERIMENTAL`.
pub const RPC_WIRE_PROTOCOL_VERSION_EXPERIMENTAL: u32 = 0xF000_0000;

/// `RpcSession::FileDescriptorTransportMode` (u8) — the v1
/// `RpcConnectionHeader.fileDescriptorTransportMode` byte. rsbinder
/// only ever advertises `NONE`/`UNIX` (subplan 2-7); `TRUSTY` is
/// out of scope but defined for wire fidelity.
pub const FD_MODE_NONE: u8 = 0;
pub const FD_MODE_UNIX: u8 = 1;
pub const FD_MODE_TRUSTY: u8 = 2;

/// AOSP `RpcFields::ObjectType::TYPE_NATIVE_FILE_DESCRIPTOR`
/// (`Parcel.h`, `int32_t`) — the first int32 of an RPC-Parcel fd
/// object body. Pinned byte-exact android-14.0.0_r75…android-16.0.0_r4
/// (`TYPE_BINDER_NULL=0, TYPE_BINDER=1, TYPE_NATIVE_FILE_DESCRIPTOR=2`).
/// Load-bearing for subplan 2-11: the AOSP-faithful FD-over-RPC v1+
/// Parcel body (`[not-null|hasComm|TYPE|fdIndex]`) that replaces
/// rsbinder's internal `[present|idx]` shape.
pub const TYPE_NATIVE_FILE_DESCRIPTOR: i32 = 2;

/// `setProtocolVersion` acceptance for a peer that supports up to
/// [`SUPPORTED_MAX_VERSION`] (AOSP rule, android-16.0.0_r4 `_NEXT = 3`):
/// accept `0..=2`, or exactly `_EXPERIMENTAL`. Equivalent to AOSP's
/// `version < _NEXT || version == _EXPERIMENTAL` since
/// `SUPPORTED_MAX_VERSION == _NEXT - 1`.
pub fn is_supported_protocol_version(version: u32) -> bool {
    version <= SUPPORTED_MAX_VERSION || version == RPC_WIRE_PROTOCOL_VERSION_EXPERIMENTAL
}

/// `RpcWireReply::wireSize(protocolVersion)` (RpcWireFormat.h): v0 is
/// just `i32 status` (4 B); v1+ (incl. v2 — byte-identical) adds
/// `parcelDataSize + reserved[3]` (20 B). The Parcel data follows
/// this fixed prefix.
fn reply_fixed_len(version: u32) -> usize {
    if version == PROTOCOL_V0 {
        4
    } else {
        20
    }
}

/// `true` once the wire carries a trailing object table — i.e. v1+
/// (`>= RPC_WIRE_PROTOCOL_VERSION_RPC_HEADER_FEATURE_EXPLICIT_PARCEL_SIZE`).
/// v0 has no `parcelDataSize` and no object table at all. (v1 and v2
/// are identical here — the v1↔v2 distinction is purely *which*
/// objects the Parcel producer records, subplan 2-8 Phase B.)
fn has_object_table(version: u32) -> bool {
    version >= PROTOCOL_V1
}

// --- bounds-checked LE readers (local — keeps wire.rs byte-unchanged) -

fn rd_u32(buf: &[u8], off: usize) -> RpcResult<u32> {
    let end = off
        .checked_add(4)
        .ok_or(RpcError::Protocol("offset overflow"))?;
    let s = buf
        .get(off..end)
        .ok_or(RpcError::Protocol("truncated u32"))?;
    Ok(u32::from_le_bytes(s.try_into().unwrap()))
}

fn rd_u64(buf: &[u8], off: usize) -> RpcResult<u64> {
    let end = off
        .checked_add(8)
        .ok_or(RpcError::Protocol("offset overflow"))?;
    let s = buf
        .get(off..end)
        .ok_or(RpcError::Protocol("truncated u64"))?;
    Ok(u64::from_le_bytes(s.try_into().unwrap()))
}

/// The android-13+ versioned RPC wire codec (subplan 2-5b, additive).
/// `version` is the negotiated `RPC_WIRE_PROTOCOL_VERSION`
/// ([`PROTOCOL_V0`] = android-13, [`PROTOCOL_V1`] = android-14/15).
#[derive(Debug, Clone, Copy)]
pub struct Android13PlusCodec {
    version: u32,
}

impl Default for Android13PlusCodec {
    /// Defaults to v0 (android-13) — the lowest, always-safe version.
    fn default() -> Self {
        Self::android13()
    }
}

impl Android13PlusCodec {
    /// v0 — android-13.
    pub fn android13() -> Self {
        Self {
            version: PROTOCOL_V0,
        }
    }

    /// v1 — android-14 and android-15 (identical wire).
    pub fn android14_15() -> Self {
        Self {
            version: PROTOCOL_V1,
        }
    }

    /// v2 — android-16 (subplan 2-8). Framing byte-identical to v1;
    /// differs only in that the Parcel producer also records binder
    /// positions in the object table (Phase B).
    pub fn android16() -> Self {
        Self {
            version: PROTOCOL_V2,
        }
    }

    /// Build for an explicit negotiated version; rejects an
    /// unsupported one (mirrors `setProtocolVersion`).
    pub fn with_version(version: u32) -> RpcResult<Self> {
        if !is_supported_protocol_version(version) {
            return Err(RpcError::Protocol("unsupported RPC wire protocol version"));
        }
        Ok(Self { version })
    }

    /// The negotiated protocol version this codec encodes/decodes.
    pub fn version(&self) -> u32 {
        self.version
    }

    fn header(command: u32, body_size: usize) -> [u8; WIRE_HEADER_LEN] {
        let mut h = [0u8; WIRE_HEADER_LEN];
        h[0..4].copy_from_slice(&command.to_le_bytes());
        h[4..8].copy_from_slice(&(body_size as u32).to_le_bytes());
        // reserved[2] stays zero.
        h
    }

    /// Project rsbinder's 32-byte [`RpcAddress`] onto the android-13+
    /// 8-byte `RpcWireAddress { options, address }` (unchanged v0↔v1).
    ///
    /// rsbinder's address is `counter:le_u64 @0..8 | role_tag @8`
    /// ([`RpcAddress::unique`]): zero → `{0, 0}` (the special all-zero
    /// address; `CREATED` unset); else `address = low 32 bits of the
    /// counter`, `options = CREATED | (FOR_SERVER if Acceptor-minted)`.
    /// Documented + internally consistent (round-trips within
    /// rsbinder); matching a live peer's `RpcState` node-id semantics
    /// is the G4 refinement (RPC_STATUS §2-5b).
    ///
    /// `pub(crate)` because the *in-parcel* binder encoding
    /// (`flattenBinder` RPC branch: `i32 present` + `writeUint64`) must
    /// use this same 8-byte `RpcWireAddress` — the session's
    /// `write_binder`/`read_binder` route through it for the
    /// android-13+ profile (G4(b): r34's 32-byte in-parcel address was
    /// rejected by the real libbinder peer).
    pub(crate) fn encode_addr(addr: &RpcAddress) -> [u8; A13_ADDR_LEN] {
        let mut out = [0u8; A13_ADDR_LEN];
        if addr.is_zero() {
            return out;
        }
        let raw = addr.as_wire_bytes();
        let address = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let mut options = ADDR_OPTION_CREATED;
        if raw[8] == 2 {
            options |= ADDR_OPTION_FOR_SERVER;
        }
        out[0..4].copy_from_slice(&options.to_le_bytes());
        out[4..8].copy_from_slice(&address.to_le_bytes());
        out
    }

    /// Inverse of [`Android13PlusCodec::encode_addr`]; `pub(crate)` for
    /// the in-parcel binder decode (`unflattenBinder` RPC branch).
    pub(crate) fn decode_addr(buf: &[u8], off: usize) -> RpcResult<RpcAddress> {
        let options = rd_u32(buf, off)?;
        let address = rd_u32(buf, off + 4)?;
        if options & ADDR_OPTION_CREATED == 0 && address == 0 {
            return Ok(RpcAddress::zero());
        }
        let mut bytes = [0u8; super::address::RPC_ADDR_LEN];
        bytes[0..4].copy_from_slice(&address.to_le_bytes());
        bytes[8] = if options & ADDR_OPTION_FOR_SERVER != 0 {
            2
        } else {
            1
        };
        Ok(RpcAddress::from_wire_bytes(bytes))
    }

    // --- connection handshake (used by the conformance test now and
    //     the live-session wiring later — G4) ----------------------

    /// `RpcConnectionHeader` (16 B) + `session_id` bytes
    /// (empty ⇒ request a new session). `fd_mode` is written into the
    /// v1 `fileDescriptorTransportMode` byte; at v0 that byte is part
    /// of `reserved` (so `fd_mode` is ignored — v0 negotiates FD mode
    /// via the separate `GET_FD_MODE` transact).
    pub fn encode_connection_header(
        &self,
        incoming: bool,
        fd_mode: u8,
        session_id: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(A13_CONN_HEADER_LEN + session_id.len());
        out.extend_from_slice(&self.version.to_le_bytes()); // u32 version
        out.push(if incoming { CONN_OPTION_INCOMING } else { 0 }); // u8 options
        if self.version == PROTOCOL_V0 {
            out.extend_from_slice(&[0u8; 9]); // u8 reserved[9]
        } else {
            out.push(fd_mode); // u8 fileDescriptorTransportMode
            out.extend_from_slice(&[0u8; 8]); // u8 reserved[8]
        }
        out.extend_from_slice(&(session_id.len() as u16).to_le_bytes()); // u16 sessionIdSize
        out.extend_from_slice(session_id);
        out
    }

    /// Parse an `RpcConnectionHeader`; returns `(version, options,
    /// fd_mode, session_id)`. `fd_mode` is `0` for a v0 header.
    pub fn decode_connection_header(&self, buf: &[u8]) -> RpcResult<(u32, u8, u8, Vec<u8>)> {
        if buf.len() < A13_CONN_HEADER_LEN {
            return Err(RpcError::Protocol("RpcConnectionHeader truncated"));
        }
        let version = rd_u32(buf, 0)?;
        let options = buf[4];
        let fd_mode = if version == PROTOCOL_V0 { 0 } else { buf[5] };
        let id_size = u16::from_le_bytes([buf[14], buf[15]]) as usize;
        let end = A13_CONN_HEADER_LEN
            .checked_add(id_size)
            .ok_or(RpcError::Protocol("sessionIdSize overflow"))?;
        let session_id = buf
            .get(A13_CONN_HEADER_LEN..end)
            .ok_or(RpcError::Protocol("session id truncated"))?
            .to_vec();
        Ok((version, options, fd_mode, session_id))
    }

    /// `RpcNewSessionResponse` (8 B).
    pub fn encode_new_session_response(&self, version: u32) -> [u8; A13_NEW_SESSION_RESP_LEN] {
        let mut out = [0u8; A13_NEW_SESSION_RESP_LEN];
        out[0..4].copy_from_slice(&version.to_le_bytes());
        out
    }

    /// Parse `RpcNewSessionResponse`; rejects an unsupported version
    /// per `setProtocolVersion`.
    pub fn decode_new_session_response(&self, buf: &[u8]) -> RpcResult<u32> {
        if buf.len() < A13_NEW_SESSION_RESP_LEN {
            return Err(RpcError::Protocol("RpcNewSessionResponse truncated"));
        }
        let version = rd_u32(buf, 0)?;
        if !is_supported_protocol_version(version) {
            return Err(RpcError::Protocol("unsupported RPC wire protocol version"));
        }
        Ok(version)
    }

    /// `RpcOutgoingConnectionInit` (8 B, `msg="cci\0"`).
    pub fn encode_connection_init(&self) -> [u8; A13_CONN_INIT_LEN] {
        let mut out = [0u8; A13_CONN_INIT_LEN];
        out[0..4].copy_from_slice(&CONN_INIT_OKAY);
        out
    }

    /// Verify an `RpcOutgoingConnectionInit` (`strncmp(msg,"cci",4)`).
    pub fn decode_connection_init(&self, buf: &[u8]) -> RpcResult<()> {
        if buf.len() < A13_CONN_INIT_LEN {
            return Err(RpcError::Protocol("RpcOutgoingConnectionInit truncated"));
        }
        if buf[0..4] != CONN_INIT_OKAY {
            return Err(RpcError::Protocol("bad RpcOutgoingConnectionInit msg"));
        }
        Ok(())
    }
}

/// AOSP `RpcState::sendTransaction`/`reply`: append the parcel data
/// then the object table as a trailing LE `u32[]` (`objectTableSpan
/// .toIovec()`). The wire body is `[fixed prefix][parcel data
/// (parcelDataSize bytes)][object table (4·N bytes)]`. v0 has no
/// object table (`validateParcel` rejects v0 + non-empty positions —
/// AC-8.4); v1 and v2 are byte-identical here (the table is just a
/// `u32[]`, version-agnostic — subplan 2-8 §0.3).
fn encode_data_and_table(
    out: &mut Vec<u8>,
    version: u32,
    data: &[u8],
    object_positions: &[u32],
) -> RpcResult<()> {
    if !has_object_table(version) {
        if !object_positions.is_empty() {
            // AOSP `RpcState::validateParcel` (RpcState.cpp:1469):
            // `protocolVersion < EXPLICIT_PARCEL_SIZE && !mObjectPositions
            // .empty()` ⇒ BAD_VALUE.
            return Err(RpcError::Protocol(
                "v0 wire has no object table (objects need protocol version >= 1)",
            ));
        }
        out.extend_from_slice(data);
        return Ok(());
    }
    out.extend_from_slice(data);
    for pos in object_positions {
        out.extend_from_slice(&pos.to_le_bytes());
    }
    Ok(())
}

/// Inverse of [`encode_data_and_table`]: AOSP's `parcelSpan.splitOff(
/// parcelDataSize)` + `objectTableBytes->reinterpret<uint32_t>()`
/// (`RpcState.cpp:840-866`/`1144-1176`). `rest` is the body after the
/// fixed prefix. Phase A does **length + %4 validation only**; v2
/// strict position-content validation (`binary_search`/range) is
/// Phase C (subplan 2-8 §3.1 — a lenient decoder still interops).
fn split_data_and_table(
    version: u32,
    rest: &[u8],
    parcel_data_size: usize,
) -> RpcResult<(Vec<u8>, Vec<u32>)> {
    if !has_object_table(version) {
        // v0: no parcelDataSize, no object table — the whole `rest`
        // is parcel data (bodySize authoritative).
        return Ok((rest.to_vec(), Vec::new()));
    }
    // `splitOff(parcelDataSize)` ⇒ nullopt (⇒ BAD_VALUE) if it runs
    // past the available bytes.
    if parcel_data_size > rest.len() {
        return Err(RpcError::Protocol(
            "parcelDataSize larger than available bytes",
        ));
    }
    let (data, table_bytes) = rest.split_at(parcel_data_size);
    // `reinterpret<uint32_t>()` ⇒ nullopt (⇒ BAD_VALUE) if the object
    // table byte length isn't a whole number of u32.
    if table_bytes.len() % 4 != 0 {
        return Err(RpcError::Protocol(
            "object table byte size not a multiple of 4",
        ));
    }
    let positions = table_bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Ok((data.to_vec(), positions))
}

impl WireCodec for Android13PlusCodec {
    fn encode_transact(&self, txn: &WireTransaction) -> RpcResult<Vec<u8>> {
        // 40 B fixed at all versions; at v1+ the first reserved word
        // carries parcelDataSize (size of the Parcel data following),
        // then the object table is appended as a trailing LE u32[]
        // (bodySize = 40 + parcelDataSize + 4·N). v0: no table.
        let table_bytes = if has_object_table(self.version) {
            4 * txn.object_positions.len()
        } else {
            0
        };
        let body_len = A13_TXN_FIXED_LEN + txn.data.len() + table_bytes;
        let mut out = Vec::with_capacity(WIRE_HEADER_LEN + body_len);
        out.extend_from_slice(&Self::header(CMD_TRANSACT, body_len));
        out.extend_from_slice(&Self::encode_addr(&txn.address)); // 8
        out.extend_from_slice(&txn.code.to_le_bytes()); // 4
        out.extend_from_slice(&txn.flags.to_le_bytes()); // 4
        out.extend_from_slice(&txn.async_number.to_le_bytes()); // 8
        let parcel_size = if self.version == PROTOCOL_V0 {
            0u32
        } else {
            txn.data.len() as u32
        };
        out.extend_from_slice(&parcel_size.to_le_bytes()); // 4 (v0: reserved)
        out.extend_from_slice(&[0u8; 12]); // reserved[3]
        encode_data_and_table(&mut out, self.version, &txn.data, &txn.object_positions)?;
        Ok(out)
    }

    fn encode_reply(&self, reply: &WireReply) -> RpcResult<Vec<u8>> {
        let fixed = reply_fixed_len(self.version);
        let table_bytes = if has_object_table(self.version) {
            4 * reply.object_positions.len()
        } else {
            0
        };
        let body_len = fixed + reply.data.len() + table_bytes;
        let mut out = Vec::with_capacity(WIRE_HEADER_LEN + body_len);
        out.extend_from_slice(&Self::header(CMD_REPLY, body_len));
        out.extend_from_slice(&reply.status.to_le_bytes()); // i32 status
        if self.version != PROTOCOL_V0 {
            out.extend_from_slice(&(reply.data.len() as u32).to_le_bytes()); // parcelDataSize
            out.extend_from_slice(&[0u8; 12]); // reserved[3]
        }
        encode_data_and_table(&mut out, self.version, &reply.data, &reply.object_positions)?;
        Ok(out)
    }

    fn encode_dec_strong(&self, addr: &RpcAddress) -> Vec<u8> {
        // RpcDecStrong { addr(8); u32 amount; u32 reserved } — 16 B,
        // unchanged v0↔v1. rsbinder sends one decrement per drop.
        let mut out = Vec::with_capacity(WIRE_HEADER_LEN + A13_DEC_STRONG_LEN);
        out.extend_from_slice(&Self::header(CMD_DEC_STRONG, A13_DEC_STRONG_LEN));
        out.extend_from_slice(&Self::encode_addr(addr)); // 8
        out.extend_from_slice(&1u32.to_le_bytes()); // amount
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved
        out
    }

    fn decode_message(&self, frame: &[u8]) -> RpcResult<WireMessage> {
        if frame.len() < WIRE_HEADER_LEN {
            return Err(RpcError::Protocol("frame shorter than RpcWireHeader"));
        }
        let command = rd_u32(frame, 0)?;
        let body_size = rd_u32(frame, 4)? as usize;
        if body_size > MAX_FRAME_LEN {
            return Err(RpcError::FrameTooLarge {
                declared: body_size,
                max: MAX_FRAME_LEN,
            });
        }
        let expected = WIRE_HEADER_LEN
            .checked_add(body_size)
            .ok_or(RpcError::Protocol("body size overflow"))?;
        if frame.len() != expected {
            return Err(RpcError::Protocol("frame length != header + bodySize"));
        }
        let body = &frame[WIRE_HEADER_LEN..];

        match command {
            CMD_TRANSACT => {
                if body.len() < A13_TXN_FIXED_LEN {
                    return Err(RpcError::Protocol("RpcWireTransaction truncated"));
                }
                let address = Self::decode_addr(body, 0)?;
                let code = rd_u32(body, A13_ADDR_LEN)?;
                let flags = rd_u32(body, A13_ADDR_LEN + 4)?;
                let async_number = rd_u64(body, A13_ADDR_LEN + 8)?;
                // body[24..28] = parcelDataSize (v1+) / reserved (v0);
                // body[28..40] = reserved[3]. At v1+ parcelDataSize is
                // **authoritative** for the data/table split (AOSP
                // `parcelSpan.splitOff(parcelDataSize)`); at v0 there is
                // no table and the whole body tail is the parcel data.
                let parcel_data_size = rd_u32(body, A13_ADDR_LEN + 16)? as usize;
                let (data, object_positions) = split_data_and_table(
                    self.version,
                    &body[A13_TXN_FIXED_LEN..],
                    parcel_data_size,
                )?;
                Ok(WireMessage::Transact(WireTransaction {
                    address,
                    code,
                    flags,
                    async_number,
                    data,
                    object_positions,
                }))
            }
            CMD_REPLY => {
                let fixed = reply_fixed_len(self.version);
                if body.len() < fixed {
                    return Err(RpcError::Protocol("RpcWireReply truncated"));
                }
                let status = rd_u32(body, 0)? as i32;
                // v1+: parcelDataSize @4 is authoritative for the
                // data/table split; reserved[3] @8..20. v0: 4 B fixed,
                // no table.
                let parcel_data_size = if has_object_table(self.version) {
                    rd_u32(body, 4)? as usize
                } else {
                    0
                };
                let (data, object_positions) =
                    split_data_and_table(self.version, &body[fixed..], parcel_data_size)?;
                Ok(WireMessage::Reply(WireReply {
                    status,
                    data,
                    object_positions,
                }))
            }
            CMD_DEC_STRONG => {
                if body.len() != A13_DEC_STRONG_LEN {
                    return Err(RpcError::Protocol("RpcDecStrong body != 16 bytes"));
                }
                let address = Self::decode_addr(body, 0)?;
                // amount @8 — rsbinder applies one decrement per
                // DEC_STRONG; honoring amount>1 from a live peer is the
                // G4 refinement (RPC_STATUS §2-5b).
                Ok(WireMessage::DecStrong(address))
            }
            _ => Err(RpcError::Protocol("unknown RpcWireHeader.command")),
        }
    }

    fn encode_session_preamble(&self, session_id: i32) -> Vec<u8> {
        // android-13+ replaced the bare int32 preamble with the
        // versioned RpcConnectionHeader. rsbinder only opens a new
        // session here (session_id == RPC_SESSION_ID_NEW) and defaults
        // to FD mode NONE; the richer handshake (RpcNewSessionResponse
        // / "cci") uses the inherent methods.
        let _ = session_id;
        self.encode_connection_header(false, FD_MODE_NONE, &[])
    }

    fn decode_session_preamble(&self, buf: &[u8]) -> RpcResult<i32> {
        // The version-bearing reply is RpcNewSessionResponse; return
        // the negotiated protocol version (the meaningful preamble
        // datum for android-13+; the trait's i32 slot is reinterpreted).
        Ok(self.decode_new_session_response(buf)? as i32)
    }
}

// ---------------------------------------------------------------------
// AOSP-faithful framing + connection handshake (G4)
//
// The real android RPC wire has **no length prefix**: a peer writes the
// 16-byte `RpcWireHeader` (whose `bodySize` field is authoritative)
// followed by the body, and the handshake structs are written as raw
// fixed-size structs (AOSP `RpcState::rpcSend`/`rpcRec` —
// `interruptableWriteFully`/`ReadFully` of iovecs, no framing). This is
// distinct from rsbinder's own `RpcTransport` framing, which prepends a
// `u32` length (`transport::write_frame`) — that extra prefix is an
// rsbinder-ism a real android peer neither writes nor expects.
//
// These helpers operate directly on a byte stream (`Read + Write`), so
// they are wire-identical to a genuine android-13/14/15 RPC peer. They
// are the reusable primitives the future opt-in `RpcSession`
// android-13+ profile wires in (the rest of G4 — see RPC_STATUS §2-5b);
// nothing here touches the existing R34 `RpcSession`/`RpcTransport`
// path (additive, R34 byte-unchanged).
// ---------------------------------------------------------------------

fn map_io(e: std::io::Error) -> RpcError {
    RpcError::from(e)
}

/// Read exactly `n` bytes. Zero bytes before any progress ⇒ a clean
/// [`RpcError::PeerClosed`]; a short read after partial progress ⇒
/// [`RpcError::Truncated`] (mirrors `transport::read_frame`).
fn read_exact_raw<R: Read>(r: &mut R, n: usize) -> RpcResult<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match r.read(&mut buf[got..]).map_err(map_io)? {
            0 => {
                return Err(if got == 0 {
                    RpcError::PeerClosed
                } else {
                    RpcError::Truncated
                })
            }
            k => got += k,
        }
    }
    Ok(buf)
}

/// Write one message **AOSP-faithfully**: `msg` is the codec output
/// (`[RpcWireHeader(16) | body]`, `bodySize` already correct), emitted
/// raw with no length prefix — exactly what a real android peer reads.
pub fn write_aosp_message<W: Write>(w: &mut W, msg: &[u8]) -> RpcResult<()> {
    if msg.len() < WIRE_HEADER_LEN {
        return Err(RpcError::Protocol("message shorter than RpcWireHeader"));
    }
    if msg.len() - WIRE_HEADER_LEN > MAX_FRAME_LEN {
        return Err(RpcError::FrameTooLarge {
            declared: msg.len() - WIRE_HEADER_LEN,
            max: MAX_FRAME_LEN,
        });
    }
    w.write_all(msg).map_err(map_io)?;
    w.flush().map_err(map_io)?;
    Ok(())
}

/// Read one message AOSP-faithfully: read the 16-byte `RpcWireHeader`,
/// take `bodySize` (LE @ offset 4, capped vs [`MAX_FRAME_LEN`] before
/// allocation — V4), then read exactly that many body bytes. Returns
/// `[header | body]`, exactly what [`Android13PlusCodec::decode_message`]
/// expects.
pub fn read_aosp_message<R: Read>(r: &mut R) -> RpcResult<Vec<u8>> {
    let header = read_exact_raw(r, WIRE_HEADER_LEN)?;
    let body_size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if body_size > MAX_FRAME_LEN {
        return Err(RpcError::FrameTooLarge {
            declared: body_size,
            max: MAX_FRAME_LEN,
        });
    }
    let mut out = Vec::with_capacity(WIRE_HEADER_LEN + body_size);
    out.extend_from_slice(&header);
    if body_size > 0 {
        out.extend_from_slice(&read_exact_raw(r, body_size)?);
    }
    Ok(out)
}

/// [`write_aosp_message`] + out-of-band `SCM_RIGHTS` fds (subplan 2-11
/// Phase A0 — the android-13+ v1+ `Unix` FD-over-RPC path). `msg` is
/// the codec output (`[RpcWireHeader(16) | body]`, `bodySize` correct),
/// emitted raw with **no length prefix**; `fds` ride the first
/// `sendmsg` (AOSP `RpcTransportRaw`). With `fds` empty this is exactly
/// [`write_aosp_message`] on the transport's raw channel (byte-identical
/// to the no-FD android-13+ path).
pub fn write_aosp_message_with_fds(
    t: &dyn super::transport::RpcTransport,
    msg: &[u8],
    fds: &[std::os::fd::BorrowedFd<'_>],
) -> RpcResult<()> {
    if msg.len() < WIRE_HEADER_LEN {
        return Err(RpcError::Protocol("message shorter than RpcWireHeader"));
    }
    if msg.len() - WIRE_HEADER_LEN > MAX_FRAME_LEN {
        return Err(RpcError::FrameTooLarge {
            declared: msg.len() - WIRE_HEADER_LEN,
            max: MAX_FRAME_LEN,
        });
    }
    t.send_raw_with_fds(msg, fds)
}

/// [`read_aosp_message`] + the `SCM_RIGHTS` fds delivered with it
/// (subplan 2-11 Phase A0). Reads the 16-byte `RpcWireHeader`, then
/// exactly `bodySize` body bytes (capped vs [`MAX_FRAME_LEN`] before
/// allocation — V4), **accumulating ancillary fds across every
/// `recvmsg`** that read the message (AOSP
/// `RpcTransportRaw::interruptableReadFully`: the kernel delivers
/// `SCM_RIGHTS` with the first byte of the sender's `sendmsg`, i.e. on
/// the header read). Clean EOF before any byte ⇒ [`RpcError::PeerClosed`];
/// a short read after partial progress ⇒ [`RpcError::Truncated`]
/// (mirrors [`read_aosp_message`]).
pub fn read_aosp_message_with_fds(
    t: &dyn super::transport::RpcTransport,
) -> RpcResult<(Vec<u8>, Vec<std::os::fd::OwnedFd>)> {
    let mut fds: Vec<std::os::fd::OwnedFd> = Vec::new();
    let mut total_read = 0usize;
    // Read exactly `want` bytes via `recvmsg`, accumulating any fds
    // into `fds`. `total_read` tracks progress across header+body so a
    // 0-byte recv distinguishes a clean pre-message close (PeerClosed)
    // from a mid-message truncation (Truncated) — same contract as
    // `read_exact_raw`.
    let mut fill = |want: usize| -> RpcResult<Vec<u8>> {
        let mut buf = vec![0u8; want];
        let mut got = 0;
        while got < want {
            let (n, mut more) = t.recv_raw_with_fds(&mut buf[got..])?;
            fds.append(&mut more);
            if n == 0 {
                return Err(if total_read == 0 {
                    RpcError::PeerClosed
                } else {
                    RpcError::Truncated
                });
            }
            got += n;
            total_read += n;
        }
        Ok(buf)
    };

    let header = fill(WIRE_HEADER_LEN)?;
    let body_size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if body_size > MAX_FRAME_LEN {
        return Err(RpcError::FrameTooLarge {
            declared: body_size,
            max: MAX_FRAME_LEN,
        });
    }
    let mut out = Vec::with_capacity(WIRE_HEADER_LEN + body_size);
    out.extend_from_slice(&header);
    if body_size > 0 {
        out.extend_from_slice(&fill(body_size)?);
    }
    Ok((out, fds))
}

/// Client side of the android-13+ connection handshake (new session).
///
/// **Wire order is byte-exact to AOSP `RpcSession::setupClient`
/// (android-13.0.0_r84), validated against a *real* compiled libbinder
/// peer on the Android 13 emulator (RPC_STATUS §"G4(b)"):**
///
/// 1. write `RpcConnectionHeader` (caller's max version, raw — no framing);
/// 2. write `RpcOutgoingConnectionInit` (`"cci"`) — the *outgoing*
///    connection **initiator sends** this (`addOutgoingConnection(init=
///    true)` → `RpcState::sendConnectionInit`); the acceptor reads it;
/// 3. read `RpcNewSessionResponse` (negotiated version, validated) —
///    AOSP reads this *after* sending `"cci"` (`setupClient` order).
///
/// Returns the [`Android13PlusCodec`] for the **negotiated** version.
///
/// (rsbinder originally had steps 2/3 inverted *and* the `"cci"`
/// direction reversed — symmetric with its own `server_accept`, so the
/// hermetic e2e could not catch it; the real-libbinder round-trip did.)
pub fn client_connect<S: Read + Write>(
    stream: &mut S,
    max_version: u32,
    incoming: bool,
    fd_mode: u8,
) -> RpcResult<Android13PlusCodec> {
    // Empty id ⇒ request a new session — byte-identical to the
    // pre-2-12 wire (the `session_id` slot already existed).
    client_connect_with_id(stream, max_version, incoming, fd_mode, &[])
}

/// Subplan 2-12 Phase A0a: like [`client_connect`] but echoes a
/// server-minted 32-byte `session_id` in the `RpcConnectionHeader`
/// (AOSP `RpcSession::setupClient`: the first connection sends an empty
/// id and reads the server-minted one; the remaining connections echo
/// it). An **empty** `session_id` is byte-for-byte identical to
/// [`client_connect`] (additive: the default path is unchanged).
pub fn client_connect_with_id<S: Read + Write>(
    stream: &mut S,
    max_version: u32,
    incoming: bool,
    fd_mode: u8,
    session_id: &[u8],
) -> RpcResult<Android13PlusCodec> {
    let hdr_codec = Android13PlusCodec::with_version(max_version)?;
    write_all_raw(
        stream,
        &hdr_codec.encode_connection_header(incoming, fd_mode, session_id),
    )?;
    write_all_raw(stream, &hdr_codec.encode_connection_init())?;
    let resp = read_exact_raw(stream, A13_NEW_SESSION_RESP_LEN)?;
    let negotiated = hdr_codec.decode_new_session_response(&resp)?;
    Android13PlusCodec::with_version(negotiated)
}

/// Server side of the android-13+ connection handshake (new session).
///
/// **Wire order is byte-exact to AOSP `RpcServer::establishConnection`
/// + `RpcSession::preJoinSetup` (android-13.0.0_r84):**
///
/// 1. read `RpcConnectionHeader` (+ variable session id);
/// 2. write `RpcNewSessionResponse` (`min(server_max, client_max)`) —
///    AOSP writes this immediately after the header for a new session
///    (`establishConnection`);
/// 3. read + validate the client's `RpcOutgoingConnectionInit`
///    (`"cci"`) — AOSP reads it on the per-connection serving setup
///    (`preJoinSetup` → `RpcState::readConnectionInit`).
///
/// Returns the negotiated [`Android13PlusCodec`] plus the client's
/// requested FD mode and session-id.
pub fn server_accept<S: Read + Write>(
    stream: &mut S,
    server_max_version: u32,
) -> RpcResult<(Android13PlusCodec, u8, Vec<u8>)> {
    // Fixed 16-byte header first, then the variable session id.
    let head = read_exact_raw(stream, A13_CONN_HEADER_LEN)?;
    let client_version = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let id_size = u16::from_le_bytes([head[14], head[15]]) as usize;
    let session_id = if id_size > 0 {
        read_exact_raw(stream, id_size)?
    } else {
        Vec::new()
    };
    let fd_mode = if client_version == PROTOCOL_V0 {
        FD_MODE_NONE
    } else {
        head[5]
    };
    // min(serverMax, callerMax) — and it must be one we implement.
    let negotiated = client_version.min(server_max_version);
    let codec = Android13PlusCodec::with_version(negotiated)?;
    write_all_raw(stream, &codec.encode_new_session_response(negotiated))?;
    let init = read_exact_raw(stream, A13_CONN_INIT_LEN)?;
    codec.decode_connection_init(&init)?;
    Ok((codec, fd_mode, session_id))
}

fn write_all_raw<W: Write>(w: &mut W, buf: &[u8]) -> RpcResult<()> {
    w.write_all(buf).map_err(map_io)?;
    w.flush().map_err(map_io)?;
    Ok(())
}

/// Bridges a [`RpcTransport`](super::transport::RpcTransport) to
/// `std::io::{Read, Write}` so the AOSP-faithful framing + handshake
/// helpers above run over any transport with raw byte access
/// (currently `unix`). EOF (`recv_raw` ⇒ `Ok(0)`) is preserved as
/// `Read` returning `Ok(0)`, so `read_exact_raw` still yields the
/// correct `PeerClosed`/`Truncated`. This is the bridge the opt-in
/// android-13+ `RpcSession` profile uses (G4); the R34 path never
/// touches it.
pub struct RawTransportIo<'a>(pub &'a dyn super::transport::RpcTransport);

impl Read for RawTransportIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0
            .recv_raw(buf)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

impl Write for RawTransportIo<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_all(buf)?;
        Ok(buf.len())
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.0
            .send_raw(buf)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(()) // `send_raw` already flushes the underlying stream.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::address::RPC_SESSION_ID_NEW;
    use crate::rpc::wire::{R34Codec, WIRE_TXN_FIXED_LEN};
    use crate::rpc::AddressSpace;

    fn txn(addr: RpcAddress, data: Vec<u8>) -> WireTransaction {
        WireTransaction {
            address: addr,
            code: 0xdead_beef,
            flags: 1,
            async_number: 0x0102_0304_0506_0708,
            data,
            ..Default::default()
        }
    }

    /// 2-5b spec-conformance golden — byte-exact against AOSP
    /// `RpcWireFormat.h` (v0 = android-13.0.0_r84, v1 =
    /// android-15.0.0_r36 == android-14.0.0_r75). Device-free.
    #[test]
    fn android13plus_spec_golden_vectors() {
        // ---- v0 (android-13) ----
        let c0 = Android13PlusCodec::android13();
        assert_eq!(c0.version(), 0);

        // TRANSACT(zero, GET_ROOT, no data): 16B header + 40B body, all
        // zero (v0: bytes 24..40 are reserved[4] = 0).
        let enc = c0
            .encode_transact(&WireTransaction {
                address: RpcAddress::zero(),
                code: 0,
                flags: 0,
                async_number: 0,
                data: vec![],
                ..Default::default()
            })
            .unwrap();
        let mut want = Vec::new();
        want.extend_from_slice(&0u32.to_le_bytes()); // TRANSACT
        want.extend_from_slice(&40u32.to_le_bytes()); // bodySize = 40
        want.extend_from_slice(&[0u8; 8]); // reserved[2]
        want.extend_from_slice(&[0u8; 40]); // addr8|code|flags|async|rsv16
        assert_eq!(enc, want, "v0 TRANSACT layout");
        assert_eq!(A13_TXN_FIXED_LEN, 40);

        // REPLY v0 = 4B fixed (i32 status) + data.
        let enc = c0
            .encode_reply(&WireReply {
                status: 0,
                data: vec![0xAA, 0xBB],
                ..Default::default()
            })
            .unwrap();
        let mut want = Vec::new();
        want.extend_from_slice(&1u32.to_le_bytes()); // REPLY
        want.extend_from_slice(&6u32.to_le_bytes()); // bodySize = 4 + 2
        want.extend_from_slice(&[0u8; 8]); // reserved[2]
        want.extend_from_slice(&0i32.to_le_bytes()); // status
        want.extend_from_slice(&[0xAA, 0xBB]);
        assert_eq!(enc, want, "v0 RpcWireReply = 4B fixed");

        // ---- v1 (android-14 / android-15) ----
        let c1 = Android13PlusCodec::android14_15();
        assert_eq!(c1.version(), 1);

        // TRANSACT v1: bytes 24..28 = parcelDataSize = data.len().
        let data = vec![1u8, 2, 3, 4, 5];
        let enc = c1
            .encode_transact(&txn(RpcAddress::zero(), data.clone()))
            .unwrap();
        // body[24..28] is parcelDataSize.
        let body = &enc[WIRE_HEADER_LEN..];
        assert_eq!(
            &body[24..28],
            &(data.len() as u32).to_le_bytes(),
            "v1 RpcWireTransaction.parcelDataSize"
        );
        assert_eq!(&body[28..40], &[0u8; 12]); // reserved[3] (12 B) zero
        assert_eq!(&body[40..], &data[..]);

        // REPLY v1 = 20B fixed (status|parcelDataSize|reserved[3]) + data.
        let enc = c1
            .encode_reply(&WireReply {
                status: 7,
                data: vec![0xCC, 0xDD, 0xEE],
                ..Default::default()
            })
            .unwrap();
        let mut want = Vec::new();
        want.extend_from_slice(&1u32.to_le_bytes()); // REPLY
        want.extend_from_slice(&23u32.to_le_bytes()); // bodySize = 20 + 3
        want.extend_from_slice(&[0u8; 8]); // reserved[2]
        want.extend_from_slice(&7i32.to_le_bytes()); // status
        want.extend_from_slice(&3u32.to_le_bytes()); // parcelDataSize
        want.extend_from_slice(&[0u8; 12]); // reserved[3]
        want.extend_from_slice(&[0xCC, 0xDD, 0xEE]);
        assert_eq!(enc, want, "v1 RpcWireReply = 20B fixed");

        // DEC_STRONG (unchanged v0↔v1): { addr8; amount=1; reserved }.
        for c in [c0, c1] {
            let mut ctr = 0u64;
            let a = RpcAddress::unique(&mut ctr, AddressSpace::Initiator);
            let enc = c.encode_dec_strong(&a);
            let mut want = Vec::new();
            want.extend_from_slice(&2u32.to_le_bytes()); // DEC_STRONG
            want.extend_from_slice(&16u32.to_le_bytes()); // bodySize
            want.extend_from_slice(&[0u8; 8]); // reserved[2]
            want.extend_from_slice(&ADDR_OPTION_CREATED.to_le_bytes()); // options
            want.extend_from_slice(&1u32.to_le_bytes()); // address = 1
            want.extend_from_slice(&1u32.to_le_bytes()); // amount = 1
            want.extend_from_slice(&0u32.to_le_bytes()); // reserved
            assert_eq!(enc, want, "DEC_STRONG layout (v{})", c.version());
        }

        // Connection header: v0 has reserved[9]; v1 has fdMode@5.
        let h0 = c0.encode_connection_header(false, FD_MODE_UNIX, &[]);
        assert_eq!(h0.len(), A13_CONN_HEADER_LEN);
        assert_eq!(&h0[0..4], &0u32.to_le_bytes()); // version 0
        assert_eq!(h0[5], 0, "v0 byte 5 is reserved, not fdMode");
        let h1 = c1.encode_connection_header(false, FD_MODE_UNIX, &[]);
        assert_eq!(&h1[0..4], &1u32.to_le_bytes()); // version 1
        assert_eq!(h1[5], FD_MODE_UNIX, "v1 fileDescriptorTransportMode");
        assert_eq!(&h1[14..16], &0u16.to_le_bytes()); // sessionIdSize = 0
        let h1i = c1.encode_connection_header(true, FD_MODE_NONE, &[]);
        assert_eq!(h1i[4], CONN_OPTION_INCOMING);
        assert_eq!(
            c1.decode_connection_header(&h1).unwrap(),
            (1, 0, FD_MODE_UNIX, Vec::new())
        );
        assert_eq!(
            c0.decode_connection_header(&h0).unwrap(),
            (0, 0, 0, Vec::new())
        );

        // New-session response + "cci" (unchanged v0↔v1).
        let r = c1.encode_new_session_response(PROTOCOL_V1);
        assert_eq!(c1.decode_new_session_response(&r).unwrap(), 1);
        let init = c1.encode_connection_init();
        assert_eq!(&init[0..4], b"cci\0");
        c1.decode_connection_init(&init).expect("\"cci\"");

        // Version acceptance (AOSP rule @ android-16.0.0_r4, _NEXT = 3):
        // accept 0,1,2,EXPERIMENTAL; reject 3 and above (AC-8.1).
        assert!(is_supported_protocol_version(0));
        assert!(is_supported_protocol_version(1));
        assert!(is_supported_protocol_version(2));
        assert!(is_supported_protocol_version(
            RPC_WIRE_PROTOCOL_VERSION_EXPERIMENTAL
        ));
        assert!(!is_supported_protocol_version(3));
        assert!(!is_supported_protocol_version(
            RPC_WIRE_PROTOCOL_VERSION_NEXT
        ));
        assert!(Android13PlusCodec::with_version(3).is_err());
        assert_eq!(Android13PlusCodec::with_version(2).unwrap().version(), 2);
        assert_eq!(SUPPORTED_MAX_VERSION, 2);
        assert_eq!(RPC_WIRE_PROTOCOL_VERSION_NEXT, 3);
    }

    /// encode∘decode == identity, all versions (incl. android-16 v2),
    /// both address spaces. v2 also round-trips a non-empty object
    /// table (synthetic positions).
    #[test]
    fn android13plus_roundtrip_all_commands() {
        for c in [
            Android13PlusCodec::android13(),
            Android13PlusCodec::android14_15(),
            Android13PlusCodec::android16(),
        ] {
            for size in [0usize, 1, 17, 4096, 1 << 20] {
                let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
                // v1+ may carry an object table; synthesize sorted
                // positions within the parcel data (v0 must stay empty
                // — exercised separately by the AC-8.4 negative test).
                let positions: Vec<u32> = if c.version() >= PROTOCOL_V1 && size >= 8 {
                    vec![0, (size / 2) as u32, (size - 4) as u32]
                } else {
                    Vec::new()
                };
                for space in [AddressSpace::Initiator, AddressSpace::Acceptor] {
                    let mut ctr = 0x4142u64;
                    let addr = RpcAddress::unique(&mut ctr, space);
                    let mut t = txn(addr, data.clone());
                    t.object_positions = positions.clone();
                    match c.decode_message(&c.encode_transact(&t).unwrap()).unwrap() {
                        WireMessage::Transact(d) => {
                            assert_eq!(d.address, addr, "addr rt (v{}, {space:?})", c.version());
                            assert_eq!(d.code, 0xdead_beef);
                            assert_eq!(d.flags, 1);
                            assert_eq!(d.async_number, 0x0102_0304_0506_0708);
                            assert_eq!(d.data, data);
                            assert_eq!(
                                d.object_positions,
                                positions,
                                "object table rt (v{})",
                                c.version()
                            );
                        }
                        other => panic!("expected Transact, got {other:?}"),
                    }
                }
                match c
                    .decode_message(
                        &c.encode_reply(&WireReply {
                            status: -42,
                            data: data.clone(),
                            object_positions: positions.clone(),
                        })
                        .unwrap(),
                    )
                    .unwrap()
                {
                    WireMessage::Reply(r) => {
                        assert_eq!(r.status, -42, "v{}", c.version());
                        assert_eq!(r.data, data);
                        assert_eq!(r.object_positions, positions, "reply table rt");
                    }
                    other => panic!("expected Reply, got {other:?}"),
                }
            }
            let mut ctr = 9u64;
            let addr = RpcAddress::unique(&mut ctr, AddressSpace::Initiator);
            match c.decode_message(&c.encode_dec_strong(&addr)).unwrap() {
                WireMessage::DecStrong(a) => assert_eq!(a, addr),
                other => panic!("expected DecStrong, got {other:?}"),
            }
        }
    }

    /// Additive invariant: distinct from `R34Codec`; r34 byte-unchanged
    /// (32B addr / 64B txn). v0 and v1 differ exactly in the reply
    /// fixed size (4 vs 20).
    #[test]
    fn android13plus_distinct_and_r34_unchanged() {
        let t = WireTransaction {
            address: RpcAddress::zero(),
            code: 0,
            flags: 0,
            async_number: 0,
            data: vec![],
            ..Default::default()
        };
        let r34 = R34Codec.encode_transact(&t).unwrap();
        assert_eq!(r34.len(), WIRE_HEADER_LEN + WIRE_TXN_FIXED_LEN); // 16 + 64
        assert_eq!(&r34[4..8], &64u32.to_le_bytes());

        let v0 = Android13PlusCodec::android13().encode_transact(&t).unwrap();
        let v1 = Android13PlusCodec::android14_15()
            .encode_transact(&t)
            .unwrap();
        let v2 = Android13PlusCodec::android16().encode_transact(&t).unwrap();
        assert_eq!(v0.len(), WIRE_HEADER_LEN + 40);
        assert_eq!(v1.len(), WIRE_HEADER_LEN + 40);
        assert_ne!(v0, r34, "android-13+ must differ from r34");
        // No-object parcel ⇒ v1 and v2 wire are byte-identical
        // (AC-8.2 — the structural v1 no-regression invariant).
        assert_eq!(v1, v2, "no-object v1 ≡ v2 (AC-8.2)");

        // Reply: the load-bearing v0/v1 divergence (4B vs 20B fixed);
        // v1 ≡ v2.
        let rep = WireReply {
            status: 0,
            data: vec![],
            ..Default::default()
        };
        assert_eq!(
            Android13PlusCodec::android13()
                .encode_reply(&rep)
                .unwrap()
                .len(),
            WIRE_HEADER_LEN + 4
        );
        assert_eq!(
            Android13PlusCodec::android14_15()
                .encode_reply(&rep)
                .unwrap()
                .len(),
            WIRE_HEADER_LEN + 20
        );
        assert_eq!(
            Android13PlusCodec::android14_15()
                .encode_reply(&rep)
                .unwrap(),
            Android13PlusCodec::android16().encode_reply(&rep).unwrap(),
            "no-object reply v1 ≡ v2 (AC-8.2)"
        );
    }

    /// 2-2.b4 / V4 — malformed input never panics/OOMs (both versions).
    #[test]
    fn android13plus_decoder_rejects_hostile_input_safely() {
        for c in [
            Android13PlusCodec::android13(),
            Android13PlusCodec::android14_15(),
            Android13PlusCodec::android16(),
        ] {
            assert!(c.decode_message(&[]).is_err());
            assert!(c.decode_message(&[0u8; 15]).is_err());

            let mut f = Vec::new();
            f.extend_from_slice(&0u32.to_le_bytes());
            f.extend_from_slice(&u32::MAX.to_le_bytes());
            f.extend_from_slice(&[0u8; 8]);
            assert!(matches!(
                c.decode_message(&f),
                Err(RpcError::FrameTooLarge { .. })
            ));

            // TRANSACT bodySize < 40 fixed prefix.
            let mut f = Vec::new();
            f.extend_from_slice(&0u32.to_le_bytes());
            f.extend_from_slice(&20u32.to_le_bytes());
            f.extend_from_slice(&[0u8; 8]);
            f.extend_from_slice(&[0u8; 20]);
            assert!(matches!(c.decode_message(&f), Err(RpcError::Protocol(_))));

            // DEC_STRONG wrong body length.
            let mut f = Vec::new();
            f.extend_from_slice(&2u32.to_le_bytes());
            f.extend_from_slice(&8u32.to_le_bytes());
            f.extend_from_slice(&[0u8; 8]);
            f.extend_from_slice(&[0u8; 8]);
            assert!(matches!(c.decode_message(&f), Err(RpcError::Protocol(_))));

            // Unknown command.
            let mut f = Vec::new();
            f.extend_from_slice(&999u32.to_le_bytes());
            f.extend_from_slice(&0u32.to_le_bytes());
            f.extend_from_slice(&[0u8; 8]);
            assert!(matches!(c.decode_message(&f), Err(RpcError::Protocol(_))));

            assert!(c.decode_connection_header(&[0u8; 8]).is_err());
            assert!(c.decode_new_session_response(&[0u8; 4]).is_err());
            assert!(c.decode_connection_init(&[0u8; 4]).is_err());

            let pre = c.encode_session_preamble(RPC_SESSION_ID_NEW);
            assert_eq!(pre.len(), A13_CONN_HEADER_LEN);
            assert_eq!(
                c.decode_session_preamble(&c.encode_new_session_response(0))
                    .unwrap(),
                0
            );
        }
        // v1 REPLY shorter than its 20B fixed prefix is rejected.
        let c1 = Android13PlusCodec::android14_15();
        let mut f = Vec::new();
        f.extend_from_slice(&1u32.to_le_bytes()); // REPLY
        f.extend_from_slice(&4u32.to_le_bytes()); // bodySize = 4 (< 20)
        f.extend_from_slice(&[0u8; 8]);
        f.extend_from_slice(&[0u8; 4]);
        assert!(matches!(c1.decode_message(&f), Err(RpcError::Protocol(_))));
    }

    /// G4 — the full android-13+ RPC protocol (versioned connection
    /// **handshake** + **AOSP-faithful framing** + `Android13PlusCodec`)
    /// driven end-to-end over a **raw `UnixStream`** (no rsbinder
    /// `RpcTransport` u32 prefix — wire-identical to a genuine
    /// android-13/14/15 RPC peer). Proves all three protocol layers
    /// interoperate, hermetically, over both v0 and v1 and across
    /// version negotiation. The remaining G4 work (wiring this as an
    /// opt-in `RpcSession` profile; running vs a compiled AOSP
    /// `binderRpcTest`) is scoped in RPC_STATUS §2-5b.
    #[test]
    fn android13plus_live_protocol_e2e_over_raw_socket() {
        use std::os::unix::net::UnixStream;
        use std::thread;

        // (client_max, server_max, expected_negotiated) — incl.
        // android-16 v2 and v2↔v1 / v2↔v0 downgrade negotiation.
        for (cmax, smax, expect) in [
            (0u32, 0u32, 0u32),
            (1, 1, 1),
            (1, 0, 0),
            (0, 1, 0),
            (2, 2, 2),
            (2, 1, 1),
            (1, 2, 1),
            (2, 0, 0),
        ] {
            let (mut c, mut s) = UnixStream::pair().expect("socketpair");

            let srv = thread::spawn(move || -> u32 {
                let (codec, fd_mode, sid) = server_accept(&mut s, smax).expect("server_accept");
                assert_eq!(fd_mode, FD_MODE_NONE);
                assert!(sid.is_empty(), "new-session ⇒ empty session id");

                // Read the client's GET_ROOT TRANSACT (AOSP framing).
                let raw = read_aosp_message(&mut s).expect("read transact");
                match codec.decode_message(&raw).expect("decode transact") {
                    WireMessage::Transact(t) => {
                        assert!(t.address.is_zero());
                        assert_eq!(t.code, 0); // GET_ROOT
                        assert!(t.data.is_empty());
                    }
                    other => panic!("expected Transact, got {other:?}"),
                }
                // Reply.
                write_aosp_message(
                    &mut s,
                    &codec
                        .encode_reply(&WireReply {
                            status: 0,
                            data: b"root!".to_vec(),
                            ..Default::default()
                        })
                        .unwrap(),
                )
                .expect("write reply");

                // Read a DEC_STRONG.
                let raw = read_aosp_message(&mut s).expect("read dec_strong");
                match codec.decode_message(&raw).expect("decode dec_strong") {
                    WireMessage::DecStrong(_) => {}
                    other => panic!("expected DecStrong, got {other:?}"),
                }
                codec.version()
            });

            let codec = client_connect(&mut c, cmax, false, FD_MODE_NONE).expect("client_connect");
            assert_eq!(codec.version(), expect, "negotiated min({cmax},{smax})");

            // GET_ROOT TRANSACT. Capture the exact wire bytes to prove
            // AOSP-faithful framing (no u32 length prefix).
            let txn = WireTransaction {
                address: RpcAddress::zero(),
                code: 0,
                flags: 0,
                async_number: 0,
                data: vec![],
                ..Default::default()
            };
            let encoded = codec.encode_transact(&txn).unwrap();
            // First 4 wire bytes are the RpcWireHeader.command
            // (TRANSACT=0), NOT an rsbinder u32 frame length.
            assert_eq!(&encoded[0..4], &0u32.to_le_bytes());
            assert_eq!(
                encoded.len(),
                WIRE_HEADER_LEN + A13_TXN_FIXED_LEN,
                "v{} GET_ROOT = 16 + 40, no length prefix",
                codec.version()
            );
            write_aosp_message(&mut c, &encoded).expect("write transact");

            let raw = read_aosp_message(&mut c).expect("read reply");
            match codec.decode_message(&raw).expect("decode reply") {
                WireMessage::Reply(r) => {
                    assert_eq!(r.status, 0);
                    assert_eq!(r.data, b"root!");
                }
                other => panic!("expected Reply, got {other:?}"),
            }

            let mut ctr = 7u64;
            let addr = RpcAddress::unique(&mut ctr, AddressSpace::Initiator);
            write_aosp_message(&mut c, &codec.encode_dec_strong(&addr)).expect("write dec_strong");

            assert_eq!(srv.join().expect("server thread"), expect);
        }
    }

    /// G4 layer-1 milestone: the same full android-13+ protocol, but
    /// driven over a real [`UnixTransport`](crate::rpc::transport::UnixTransport)
    /// through the [`RawTransportIo`] bridge (not a bare `UnixStream`)
    /// — i.e. over the actual `RpcTransport` abstraction the opt-in
    /// `RpcSession` profile will use. v0 and v1.
    #[test]
    fn android13plus_e2e_over_unix_transport_bridge() {
        use crate::rpc::transport::UnixTransport;
        use std::thread;

        for (cmax, expect) in [(0u32, 0u32), (1, 1), (2, 2)] {
            let (a, b) = UnixTransport::pair().expect("unix pair");

            let srv = thread::spawn(move || -> u32 {
                let mut io = RawTransportIo(&b);
                let (codec, fd, sid) = server_accept(&mut io, 2).expect("server_accept");
                assert_eq!(fd, FD_MODE_NONE);
                assert!(sid.is_empty());
                let raw = read_aosp_message(&mut io).expect("read transact");
                match codec.decode_message(&raw).expect("decode") {
                    WireMessage::Transact(t) => assert_eq!(t.code, 0),
                    o => panic!("expected Transact, got {o:?}"),
                }
                write_aosp_message(
                    &mut io,
                    &codec
                        .encode_reply(&WireReply {
                            status: 0,
                            data: b"via-transport".to_vec(),
                            ..Default::default()
                        })
                        .unwrap(),
                )
                .expect("write reply");
                codec.version()
            });

            let mut io = RawTransportIo(&a);
            let codec = client_connect(&mut io, cmax, false, FD_MODE_NONE).expect("client_connect");
            assert_eq!(codec.version(), expect);
            write_aosp_message(
                &mut io,
                &codec
                    .encode_transact(&WireTransaction {
                        address: RpcAddress::zero(),
                        code: 0,
                        flags: 0,
                        async_number: 0,
                        data: vec![],
                        ..Default::default()
                    })
                    .unwrap(),
            )
            .expect("write transact");
            let raw = read_aosp_message(&mut io).expect("read reply");
            match codec.decode_message(&raw).expect("decode reply") {
                WireMessage::Reply(r) => {
                    assert_eq!(r.status, 0);
                    assert_eq!(r.data, b"via-transport");
                }
                o => panic!("expected Reply, got {o:?}"),
            }
            assert_eq!(srv.join().expect("server thread"), expect);
        }

        // Default transports have no raw access (additive, by type).
        use crate::rpc::transport::RpcTransport;
        let (m, _m2) = crate::rpc::transport::MemTransport::pair();
        assert!(m.send_raw(b"x").is_err(), "mem has no raw byte access");
        assert!(m.recv_raw(&mut [0u8; 4]).is_err());
    }

    // ===== subplan 2-8 — android-16 RPC wire v2 (Phase A) ==========

    /// **AC-8.1** — android-16.0.0_r4 version constants golden.
    /// `RpcSession.h`: `RPC_WIRE_PROTOCOL_VERSION = 2`, `_NEXT = 3`,
    /// `_EXPLICIT_PARCEL_SIZE = 1`, `_INCLUDES_BINDER_POSITIONS = 2`.
    /// `setProtocolVersion` accepts {0,1,2,EXPERIMENTAL}, rejects ≥3.
    #[test]
    fn android16_v2_version_constants_golden() {
        assert_eq!(PROTOCOL_V0, 0);
        assert_eq!(PROTOCOL_V1, 1);
        assert_eq!(PROTOCOL_V2, 2);
        assert_eq!(
            SUPPORTED_MAX_VERSION, 2,
            "android-16 RPC_WIRE_PROTOCOL_VERSION"
        );
        assert_eq!(RPC_WIRE_PROTOCOL_VERSION_NEXT, 3);
        assert_eq!(RPC_WIRE_PROTOCOL_VERSION_EXPERIMENTAL, 0xF000_0000);
        for v in [0, 1, 2] {
            assert!(is_supported_protocol_version(v));
            assert_eq!(Android13PlusCodec::with_version(v).unwrap().version(), v);
        }
        assert!(is_supported_protocol_version(
            RPC_WIRE_PROTOCOL_VERSION_EXPERIMENTAL
        ));
        for v in [3u32, 4, 100, RPC_WIRE_PROTOCOL_VERSION_NEXT] {
            assert!(!is_supported_protocol_version(v), "reject v{v}");
            assert!(Android13PlusCodec::with_version(v).is_err());
        }
        assert_eq!(Android13PlusCodec::android16().version(), 2);
        // `RpcWireReply::wireSize` is byte-identical v1≡v2 (4 vs 20 is
        // strictly the v0-vs-v1+ split — no v2 framing change).
        assert_eq!(reply_fixed_len(0), 4);
        assert_eq!(reply_fixed_len(1), 20);
        assert_eq!(reply_fixed_len(2), 20);
        assert!(!has_object_table(0));
        assert!(has_object_table(1));
        assert!(has_object_table(2));
    }

    /// **AC-8.2** — a no-object parcel encodes **byte-identically** at
    /// v1 and v2 (TRANSACT *and* REPLY, several payload sizes). This is
    /// the structural v1-no-regression guarantee: an empty object table
    /// is 0 wire bytes ⇒ `bodySize` unchanged ⇒ a v2-capable rsbinder's
    /// no-object traffic is wire-identical to its v1 traffic (and to
    /// the pre-2-8 wire).
    #[test]
    fn android16_no_object_v1_eq_v2_byte_identical() {
        let v1 = Android13PlusCodec::android14_15();
        let v2 = Android13PlusCodec::android16();
        for size in [0usize, 1, 4, 5, 64, 4096] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut ctr = 0xABCDu64;
            let addr = RpcAddress::unique(&mut ctr, AddressSpace::Initiator);
            let t = txn(addr, data.clone()); // object_positions empty
            assert_eq!(
                v1.encode_transact(&t).unwrap(),
                v2.encode_transact(&t).unwrap(),
                "no-object TRANSACT v1≡v2 (size {size})"
            );
            let r = WireReply {
                status: -7,
                data: data.clone(),
                object_positions: Vec::new(),
            };
            assert_eq!(
                v1.encode_reply(&r).unwrap(),
                v2.encode_reply(&r).unwrap(),
                "no-object REPLY v1≡v2 (size {size})"
            );
        }
    }

    /// **AC-8.3** — object-table framing golden vs AOSP
    /// `RpcState.cpp`: `bodySize = fixed + parcelDataSize + 4·N`; the
    /// table is the trailing LE `u32[]` after the parcel data;
    /// `parcelDataSize` is the data length (unchanged); encode→decode
    /// round-trips the positions; and the android-16 `splitOff` /
    /// `reinterpret<uint32_t>` receive rules (parcelDataSize past the
    /// body, or a table byte-size not a multiple of 4) are rejected.
    #[test]
    fn android16_v2_object_table_framing_golden() {
        let c = Android13PlusCodec::android16();
        let data = vec![0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let positions = vec![0u32, 4];

        // ---- TRANSACT ----
        let t = WireTransaction {
            address: RpcAddress::zero(),
            code: 0xCAFE,
            flags: 0,
            async_number: 0,
            data: data.clone(),
            object_positions: positions.clone(),
        };
        let enc = c.encode_transact(&t).unwrap();
        // bodySize = 40 + 8 (data) + 4*2 (table) = 56.
        assert_eq!(&enc[4..8], &56u32.to_le_bytes(), "bodySize = 40+pds+4N");
        let body = &enc[WIRE_HEADER_LEN..];
        // parcelDataSize @24 = data.len() (UNCHANGED by the table).
        assert_eq!(&body[24..28], &(data.len() as u32).to_le_bytes());
        // [40 .. 40+8] = parcel data, then the LE u32[] table.
        assert_eq!(&body[40..48], &data[..]);
        assert_eq!(&body[48..52], &0u32.to_le_bytes());
        assert_eq!(&body[52..56], &4u32.to_le_bytes());
        assert_eq!(enc.len(), WIRE_HEADER_LEN + 56);
        match c.decode_message(&enc).unwrap() {
            WireMessage::Transact(d) => {
                assert_eq!(d.data, data);
                assert_eq!(d.object_positions, positions);
            }
            o => panic!("expected Transact, got {o:?}"),
        }

        // ---- REPLY ----
        let r = WireReply {
            status: 0,
            data: data.clone(),
            object_positions: positions.clone(),
        };
        let enc = c.encode_reply(&r).unwrap();
        // bodySize = 20 + 8 + 8 = 36.
        assert_eq!(&enc[4..8], &36u32.to_le_bytes());
        let body = &enc[WIRE_HEADER_LEN..];
        assert_eq!(&body[4..8], &(data.len() as u32).to_le_bytes()); // parcelDataSize
        assert_eq!(&body[20..28], &data[..]);
        assert_eq!(&body[28..32], &0u32.to_le_bytes());
        assert_eq!(&body[32..36], &4u32.to_le_bytes());
        match c.decode_message(&enc).unwrap() {
            WireMessage::Reply(d) => {
                assert_eq!(d.data, data);
                assert_eq!(d.object_positions, positions);
            }
            o => panic!("expected Reply, got {o:?}"),
        }

        // android-16 `parcelSpan.splitOff(parcelDataSize)` ⇒ nullopt
        // ⇒ BAD_VALUE: forge a TRANSACT whose parcelDataSize exceeds
        // the available body tail.
        let mut bad = c
            .encode_transact(&WireTransaction {
                address: RpcAddress::zero(),
                code: 1,
                flags: 0,
                async_number: 0,
                data: vec![1, 2, 3, 4],
                object_positions: vec![0],
            })
            .unwrap();
        // body[24..28] = parcelDataSize; bump it past the tail.
        let pds_off = WIRE_HEADER_LEN + 24;
        bad[pds_off..pds_off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert!(matches!(c.decode_message(&bad), Err(RpcError::Protocol(_))));

        // `objectTableBytes->reinterpret<uint32_t>()` ⇒ nullopt if the
        // table byte-size isn't a multiple of 4: parcelDataSize that
        // leaves a 2-byte tail.
        let mut bad = c
            .encode_transact(&WireTransaction {
                address: RpcAddress::zero(),
                code: 1,
                flags: 0,
                async_number: 0,
                data: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01],
                object_positions: Vec::new(),
            })
            .unwrap();
        // data.len()=6, no table ⇒ tail after pds is 0 (ok). Now claim
        // parcelDataSize=4 ⇒ 2-byte trailing "table" ⇒ %4 != 0.
        bad[pds_off..pds_off + 4].copy_from_slice(&4u32.to_le_bytes());
        assert!(matches!(c.decode_message(&bad), Err(RpcError::Protocol(_))));
    }

    /// **AC-8.4** — `validateParcel` analogue: a v0 (android-13) or r34
    /// wire can carry **no** object table; a non-empty `object_positions`
    /// on those is a protocol error, not a silently-dropped table
    /// (AOSP `RpcState::validateParcel` ⇒ `BAD_VALUE`).
    #[test]
    fn android16_v0_and_r34_reject_object_positions() {
        let t = WireTransaction {
            address: RpcAddress::zero(),
            code: 1,
            flags: 0,
            async_number: 0,
            data: vec![1, 2, 3, 4],
            object_positions: vec![0],
        };
        let r = WireReply {
            status: 0,
            data: vec![1, 2, 3, 4],
            object_positions: vec![0],
        };
        // v0 (android-13) — no parcelDataSize, no table.
        let v0 = Android13PlusCodec::android13();
        assert!(matches!(v0.encode_transact(&t), Err(RpcError::Protocol(_))));
        assert!(matches!(v0.encode_reply(&r), Err(RpcError::Protocol(_))));
        // r34 (android-12, pre-versioning) — no object table concept.
        assert!(matches!(
            R34Codec.encode_transact(&t),
            Err(RpcError::Protocol(_))
        ));
        assert!(matches!(
            R34Codec.encode_reply(&r),
            Err(RpcError::Protocol(_))
        ));
        // …and empty positions on v0/r34 still encode fine (unchanged).
        let t0 = txn(RpcAddress::zero(), vec![9, 9, 9, 9]);
        assert!(v0.encode_transact(&t0).is_ok());
        assert!(R34Codec.encode_transact(&t0).is_ok());
    }
}
