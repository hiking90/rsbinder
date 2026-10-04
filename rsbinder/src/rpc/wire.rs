// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! RPC wire codec.
//!
//! rsbinder does not invent a wire. The default codec is the
//! [`WireCodec`] trait + [`R34Codec`], a *direct implementation of the
//! real android-12 r34 RPC wire*, so the round-trip and golden tests are
//! simultaneously r34 spec-conformance — no device, no AOSP build
//! needed. The android-13+ versioned wire is the additive
//! [`Android13PlusCodec`](super::wire_android13::Android13PlusCodec)
//! behind the same trait (v0 = android-13, v1 = android-14/15, v2 =
//! android-16).
//!
//! r34 layout (LE, explicit serialization — no `#[repr]` dependency):
//! * `RpcWireHeader` (16B): `u32 command; u32 bodySize; u32 reserved[2]`
//! * commands: `TRANSACT=0`, `REPLY=1`, `DEC_STRONG=2`
//! * `RpcWireTransaction` (64B fixed + data): `u8 address[32]; u32 code;
//!   u32 flags; u64 asyncNumber; u32 reserved[4]; u8 data[]`
//! * `RpcWireReply`: `i32 status; u8 data[]`
//! * `DEC_STRONG` body: `u8 address[32]`
//! * session preamble: a bare `int32` session id (no header)
//!
//! No magic / self-sync: `bodySize` is authoritative, so the decoder is
//! strict and every length/offset is bounds-checked before use.

use super::address::{RpcAddress, RPC_ADDR_LEN};
use super::transport::MAX_FRAME_LEN;
use super::{RpcError, RpcResult};

/// `RpcWireHeader` size (android-12 r34).
pub(crate) const WIRE_HEADER_LEN: usize = 16;
/// `RpcWireTransaction` fixed-prefix size (before `data[]`).
pub(crate) const WIRE_TXN_FIXED_LEN: usize = RPC_ADDR_LEN + 4 + 4 + 8 + 16; // = 64

/// r34 `RpcWireHeader.command` values.
const CMD_TRANSACT: u32 = 0;
const CMD_REPLY: u32 = 1;
const CMD_DEC_STRONG: u32 = 2;

/// A decoded transaction (`RpcWireTransaction` + parcel payload).
#[derive(Debug, Clone)]
pub struct WireTransaction {
    /// Target object address (`zero()` for special server transactions).
    pub address: RpcAddress,
    /// AIDL transaction code (or [`super::address::SpecialTransaction`]
    /// code when `address` is zero).
    pub code: u32,
    /// Transaction flags (oneway bit etc.).
    pub flags: u32,
    /// Oneway ordering counter (android `asyncNumber`).
    pub async_number: u64,
    /// The serialized RPC-mode `Parcel` payload.
    pub data: Vec<u8>,
    /// AOSP `RpcFields::mObjectPositions` — the sorted byte offsets,
    /// **within `data`**, of every flattened RPC object (binder at
    /// android-16 v2, FD at v1+). Sent on the wire as a trailing LE
    /// `u32[]` after the parcel data (android-16 v2).
    /// **Default empty** ⇒ the wire is byte-identical to the no-object
    /// encoding (the v1≡v2 invariant). The R34 and v0 wires
    /// have no object table; a non-empty table on those is a protocol
    /// error (`validateParcel`).
    pub object_positions: Vec<u32>,
}

/// A decoded reply (`RpcWireReply` + parcel payload).
#[derive(Debug, Clone, Default)]
pub struct WireReply {
    /// AIDL/binder status (`0` == ok).
    pub status: i32,
    /// The serialized RPC-mode `Parcel` payload.
    pub data: Vec<u8>,
    /// See [`WireTransaction::object_positions`] — the reply parcel's
    /// object table (android-16 v2). Default empty = byte-identical to
    /// the no-object reply encoding.
    pub object_positions: Vec<u32>,
}

/// A transaction to encode, borrowing its payload: [`WireTransaction`] without the copies.
#[derive(Debug, Clone, Copy)]
pub struct WireTransactionRef<'a> {
    pub address: &'a RpcAddress,
    pub code: u32,
    pub flags: u32,
    pub async_number: u64,
    pub data: &'a [u8],
    pub object_positions: &'a [u32],
}

/// A reply to encode, borrowing its payload: [`WireReply`] without the copies.
#[derive(Debug, Clone, Copy)]
pub struct WireReplyRef<'a> {
    pub status: i32,
    pub data: &'a [u8],
    pub object_positions: &'a [u32],
}

impl<'a> From<&'a WireTransaction> for WireTransactionRef<'a> {
    fn from(txn: &'a WireTransaction) -> Self {
        WireTransactionRef {
            address: &txn.address,
            code: txn.code,
            flags: txn.flags,
            async_number: txn.async_number,
            data: &txn.data,
            object_positions: &txn.object_positions,
        }
    }
}

impl<'a> From<&'a WireReply> for WireReplyRef<'a> {
    fn from(reply: &'a WireReply) -> Self {
        WireReplyRef {
            status: reply.status,
            data: &reply.data,
            object_positions: &reply.object_positions,
        }
    }
}

impl Default for WireTransaction {
    fn default() -> Self {
        Self {
            address: RpcAddress::zero(),
            code: 0,
            flags: 0,
            async_number: 0,
            data: Vec::new(),
            object_positions: Vec::new(),
        }
    }
}

/// One decoded wire message.
#[derive(Debug, Clone)]
pub enum WireMessage {
    /// `TRANSACT`.
    Transact(WireTransaction),
    /// `REPLY`.
    Reply(WireReply),
    /// `DEC_STRONG` against `RpcAddress` by the given `amount` (AOSP
    /// `RpcDecStrong.amount` — a peer may batch more than one decrement into a
    /// single command; the R34 wire has no amount field and always means `1`).
    DecStrong(RpcAddress, u32),
}

/// Pluggable RPC wire format. The session owns a codec instance (no
/// global); the android-13+ versioned wire is an additional impl
/// ([`super::wire_android13::Android13PlusCodec`]) behind this trait.
pub trait WireCodec: Send + Sync {
    /// Encode a complete `TRANSACT` message (header + txn + data +
    /// optional object table). Fails (`validateParcel`) if the codec's
    /// wire version has no object table but `txn.object_positions` is
    /// non-empty.
    fn encode_transact_ref(&self, txn: WireTransactionRef<'_>) -> RpcResult<Vec<u8>>;
    /// Encode a complete `REPLY` message. Same object-table version
    /// rule as [`WireCodec::encode_transact_ref`].
    fn encode_reply_ref(&self, reply: WireReplyRef<'_>) -> RpcResult<Vec<u8>>;
    /// [`WireCodec::encode_transact_ref`] of an owned transaction.
    fn encode_transact(&self, txn: &WireTransaction) -> RpcResult<Vec<u8>> {
        self.encode_transact_ref(txn.into())
    }
    /// [`WireCodec::encode_reply_ref`] of an owned reply.
    #[cfg(test)]
    fn encode_reply(&self, reply: &WireReply) -> RpcResult<Vec<u8>> {
        self.encode_reply_ref(reply.into())
    }
    /// Encode the `DEC_STRONG` frames that release `amount` references to
    /// `addr`, in send order. A wire with an `amount` field (android-13+
    /// `RpcDecStrong`) returns one frame; r34 has none, so it returns
    /// `amount` one-reference frames, each framed on its own. `amount == 0`
    /// returns no frame.
    fn encode_dec_strong(&self, addr: &RpcAddress, amount: u32) -> Vec<Vec<u8>>;
    /// Decode one complete wire message (header + body).
    fn decode_message(&self, frame: &[u8]) -> RpcResult<WireMessage>;
    /// Encode the bare `int32` session-id preamble (no header).
    #[cfg(test)]
    fn encode_session_preamble(&self, session_id: i32) -> Vec<u8>;
    /// Decode the bare `int32` session-id preamble.
    #[cfg(any(test, feature = "fuzzing"))]
    fn decode_session_preamble(&self, buf: &[u8]) -> RpcResult<i32>;
}

/// The android-12.0.0_r34 RPC wire (the default Track-1 codec).
#[derive(Debug, Default, Clone, Copy)]
pub struct R34Codec;

impl R34Codec {
    fn header(command: u32, body_size: usize) -> RpcResult<[u8; WIRE_HEADER_LEN]> {
        // As the decoder: past the cap, `as u32` could truncate `bodySize` and misframe the peer.
        if body_size > MAX_FRAME_LEN {
            return Err(RpcError::FrameTooLarge {
                declared: body_size,
                max: MAX_FRAME_LEN,
            });
        }
        let mut h = [0u8; WIRE_HEADER_LEN];
        h[0..4].copy_from_slice(&command.to_le_bytes());
        h[4..8].copy_from_slice(&(body_size as u32).to_le_bytes());
        // reserved[2] stays zero.
        Ok(h)
    }
}

/// Read a little-endian `u32` at `off`, bounds-checked.
fn rd_u32(buf: &[u8], off: usize) -> RpcResult<u32> {
    buf.get(off..)
        .and_then(<[u8]>::first_chunk)
        .map(|b| u32::from_le_bytes(*b))
        .ok_or(RpcError::Protocol("truncated u32"))
}

fn rd_i32(buf: &[u8], off: usize) -> RpcResult<i32> {
    Ok(rd_u32(buf, off)? as i32)
}

fn rd_u64(buf: &[u8], off: usize) -> RpcResult<u64> {
    buf.get(off..)
        .and_then(<[u8]>::first_chunk)
        .map(|b| u64::from_le_bytes(*b))
        .ok_or(RpcError::Protocol("truncated u64"))
}

fn rd_addr(buf: &[u8], off: usize) -> RpcResult<RpcAddress> {
    buf.get(off..)
        .and_then(<[u8]>::first_chunk::<RPC_ADDR_LEN>)
        .map(|b| RpcAddress::from_wire_bytes(*b))
        .ok_or(RpcError::Protocol("truncated address"))
}

impl WireCodec for R34Codec {
    fn encode_transact_ref(&self, txn: WireTransactionRef<'_>) -> RpcResult<Vec<u8>> {
        // r34 has no object table: reject one rather than drop it and desync (`validateParcel`).
        if !txn.object_positions.is_empty() {
            return Err(RpcError::Protocol(
                "r34 wire has no object table (object_positions must be empty)",
            ));
        }
        let body_len = WIRE_TXN_FIXED_LEN + txn.data.len();
        let mut out = Vec::with_capacity(WIRE_HEADER_LEN + body_len);
        out.extend_from_slice(&Self::header(CMD_TRANSACT, body_len)?);
        out.extend_from_slice(txn.address.as_wire_bytes()); // 32
        out.extend_from_slice(&txn.code.to_le_bytes()); // 4
        out.extend_from_slice(&txn.flags.to_le_bytes()); // 4
        out.extend_from_slice(&txn.async_number.to_le_bytes()); // 8
        out.extend_from_slice(&[0u8; 16]); // reserved[4]
        out.extend_from_slice(txn.data); // data[]
        Ok(out)
    }

    fn encode_reply_ref(&self, reply: WireReplyRef<'_>) -> RpcResult<Vec<u8>> {
        if !reply.object_positions.is_empty() {
            return Err(RpcError::Protocol(
                "r34 wire has no object table (object_positions must be empty)",
            ));
        }
        let body_len = 4 + reply.data.len();
        let mut out = Vec::with_capacity(WIRE_HEADER_LEN + body_len);
        out.extend_from_slice(&Self::header(CMD_REPLY, body_len)?);
        out.extend_from_slice(&reply.status.to_le_bytes());
        out.extend_from_slice(reply.data);
        Ok(out)
    }

    fn encode_dec_strong(&self, addr: &RpcAddress, amount: u32) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(WIRE_HEADER_LEN + RPC_ADDR_LEN);
        // RPC_ADDR_LEN is far below MAX_FRAME_LEN, so the bound check cannot trip.
        out.extend_from_slice(
            &Self::header(CMD_DEC_STRONG, RPC_ADDR_LEN)
                .expect("dec_strong header is within the frame bound"),
        );
        out.extend_from_slice(addr.as_wire_bytes());
        // No amount field on r34: one frame per reference, as AOSP r34 sends them.
        vec![out; amount as usize]
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
        // The frame must be exactly header + bodySize (no trailing slop, no short body).
        let expected = WIRE_HEADER_LEN
            .checked_add(body_size)
            .ok_or(RpcError::Protocol("body size overflow"))?;
        if frame.len() != expected {
            return Err(RpcError::Protocol("frame length != header + bodySize"));
        }
        let body = &frame[WIRE_HEADER_LEN..];

        match command {
            CMD_TRANSACT => {
                if body.len() < WIRE_TXN_FIXED_LEN {
                    return Err(RpcError::Protocol("RpcWireTransaction truncated"));
                }
                let address = rd_addr(body, 0)?;
                let code = rd_u32(body, RPC_ADDR_LEN)?;
                let flags = rd_u32(body, RPC_ADDR_LEN + 4)?;
                let async_number = rd_u64(body, RPC_ADDR_LEN + 8)?;
                // reserved[4] at RPC_ADDR_LEN+16 .. +32 — ignored.
                let data = body[WIRE_TXN_FIXED_LEN..].to_vec();
                Ok(WireMessage::Transact(WireTransaction {
                    address,
                    code,
                    flags,
                    async_number,
                    data,
                    // r34 has no object table.
                    object_positions: Vec::new(),
                }))
            }
            CMD_REPLY => {
                if body.len() < 4 {
                    return Err(RpcError::Protocol("RpcWireReply truncated"));
                }
                let status = rd_i32(body, 0)?;
                let data = body[4..].to_vec();
                Ok(WireMessage::Reply(WireReply {
                    status,
                    data,
                    object_positions: Vec::new(),
                }))
            }
            CMD_DEC_STRONG => {
                if body.len() != RPC_ADDR_LEN {
                    return Err(RpcError::Protocol("DEC_STRONG body != 32 bytes"));
                }
                // The R34 wire carries no amount field: one decrement per command.
                Ok(WireMessage::DecStrong(rd_addr(body, 0)?, 1))
            }
            _ => Err(RpcError::Protocol("unknown RpcWireHeader.command")),
        }
    }

    #[cfg(test)]
    fn encode_session_preamble(&self, session_id: i32) -> Vec<u8> {
        session_id.to_le_bytes().to_vec()
    }

    #[cfg(any(test, feature = "fuzzing"))]
    fn decode_session_preamble(&self, buf: &[u8]) -> RpcResult<i32> {
        // Exactly a bare `int32`: any other length is malformed and would desync the next recv.
        let arr: [u8; 4] = buf
            .try_into()
            .map_err(|_| RpcError::Protocol("session preamble must be exactly 4 bytes"))?;
        Ok(i32::from_le_bytes(arr))
    }
}

/// Decode-only entrypoint for the `rpc_wire_decode` fuzz target: the r34 decoder and the
/// android-13+ v1 decoder, whose `parcelDataSize`/object-table split r34 does not have.
/// Not part of the supported API surface.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn __fuzz_decode_wire(input: &[u8]) {
    let _ = R34Codec.decode_message(input);
    let _ = super::wire_android13::Android13PlusCodec::android14_15().decode_message(input);
}

/// Decode-only entrypoint for the `rpc_session_handshake` fuzz target:
/// the first 4 bytes are fed to the R34 session preamble decoder, the
/// remainder through the R34 message decoder. No panic / OOM / hang on
/// any input; bad negotiation values are rejected, not trusted. (The
/// android-13+ connection header has its own path — `server_accept` /
/// `decode_connection_header` — and is not covered here.)
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn __fuzz_session_handshake(input: &[u8]) {
    let c = R34Codec;
    let (pre, rest) = input.split_at(input.len().min(4));
    let _ = c.decode_session_preamble(pre);
    let _ = c.decode_message(rest);
}

/// Decode-only entrypoint for the `rpc_address_decode` fuzz target: both
/// address parsers — the R34 32-byte form and the android-13+ 8-byte
/// form — are called *directly* on the input (a DEC_STRONG wrapper alone
/// would stop at the body-length check for every input that is not
/// exactly 32 bytes and never reach them), plus the framed path.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn __fuzz_decode_address(input: &[u8]) {
    let _ = rd_addr(input, 0);
    let _ = super::wire_android13::Android13PlusCodec::decode_addr(input, 0);
    let Ok(header) = R34Codec::header(CMD_DEC_STRONG, input.len()) else {
        return;
    };
    let mut frame = Vec::with_capacity(WIRE_HEADER_LEN + input.len());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(input);
    let _ = R34Codec.decode_message(&frame);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::address::RPC_SESSION_ID_NEW;

    fn rt_codec() -> R34Codec {
        R34Codec
    }

    /// `R34Codec::header` rejects a body over `MAX_FRAME_LEN` like `Android13PlusCodec`, no wrap.
    #[test]
    fn header_rejects_oversize_body() {
        assert!(matches!(
            R34Codec::header(CMD_TRANSACT, MAX_FRAME_LEN + 1),
            Err(RpcError::FrameTooLarge { .. })
        ));
        assert!(R34Codec::header(CMD_TRANSACT, MAX_FRAME_LEN).is_ok());
    }

    /// encode∘decode == identity for every command, payloads sampled over 0..1 MiB.
    #[test]
    fn roundtrip_all_commands() {
        let c = rt_codec();
        for size in [0usize, 1, 4, 17, 4096, 1 << 20] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut ctr = 7u64;
            let addr = RpcAddress::unique(&mut ctr, crate::rpc::AddressSpace::Initiator);

            let txn = WireTransaction {
                address: addr,
                code: 0xdead_beef,
                flags: 1,
                async_number: 0x0102_0304_0506_0708,
                data: data.clone(),
                ..Default::default()
            };
            let enc = c.encode_transact(&txn).unwrap();
            match c.decode_message(&enc).unwrap() {
                WireMessage::Transact(t) => {
                    assert_eq!(t.address, addr);
                    assert_eq!(t.code, 0xdead_beef);
                    assert_eq!(t.flags, 1);
                    assert_eq!(t.async_number, 0x0102_0304_0506_0708);
                    assert_eq!(t.data, data);
                }
                other => panic!("expected Transact, got {other:?}"),
            }

            let reply = WireReply {
                status: -42,
                data: data.clone(),
                ..Default::default()
            };
            let enc = c.encode_reply(&reply).unwrap();
            match c.decode_message(&enc).unwrap() {
                WireMessage::Reply(r) => {
                    assert_eq!(r.status, -42);
                    assert_eq!(r.data, data);
                }
                other => panic!("expected Reply, got {other:?}"),
            }
        }

        let mut ctr = 99u64;
        let addr = RpcAddress::unique(&mut ctr, crate::rpc::AddressSpace::Initiator);
        let enc = c.encode_dec_strong(&addr, 3);
        assert_eq!(enc.len(), 3, "r34 has no amount: one frame per reference");
        for frame in &enc {
            match c.decode_message(frame).unwrap() {
                WireMessage::DecStrong(a, amount) => {
                    assert_eq!(a, addr);
                    assert_eq!(amount, 1);
                }
                other => panic!("expected DecStrong, got {other:?}"),
            }
        }
        assert!(c.encode_dec_strong(&addr, 0).is_empty());

        let pre = c.encode_session_preamble(RPC_SESSION_ID_NEW);
        assert_eq!(c.decode_session_preamble(&pre).unwrap(), RPC_SESSION_ID_NEW);
    }

    /// r34 spec-conformance gate: golden vectors byte-for-byte vs the android-12 spec, no device.
    #[test]
    fn r34_spec_golden_vectors() {
        let c = R34Codec;

        // Header: TRANSACT(0)|bodySize|rsv=0; body: addr(32)|code|flags|asyncNumber|rsv(16)|data
        let txn = WireTransaction {
            address: RpcAddress::zero(),
            code: 0, // GET_ROOT
            flags: 0,
            async_number: 0,
            data: vec![],
            ..Default::default()
        };
        let enc = c.encode_transact(&txn).unwrap();
        let mut want = Vec::new();
        want.extend_from_slice(&0u32.to_le_bytes()); // command = TRANSACT
        want.extend_from_slice(&64u32.to_le_bytes()); // bodySize = 64 (fixed, no data)
        want.extend_from_slice(&[0u8; 8]); // reserved[2]
        want.extend_from_slice(&[0u8; 32]); // RpcWireAddress = zero
        want.extend_from_slice(&0u32.to_le_bytes()); // code
        want.extend_from_slice(&0u32.to_le_bytes()); // flags
        want.extend_from_slice(&0u64.to_le_bytes()); // asyncNumber
        want.extend_from_slice(&[0u8; 16]); // reserved[4]
        assert_eq!(
            enc, want,
            "TRANSACT(zero,GET_ROOT) must be byte-identical to the r34 spec"
        );
        assert_eq!(enc.len(), WIRE_HEADER_LEN + WIRE_TXN_FIXED_LEN);
        assert_eq!(WIRE_HEADER_LEN, 16);
        assert_eq!(WIRE_TXN_FIXED_LEN, 64);

        // -- RpcWireReply: status(i32) | data --
        let reply = WireReply {
            status: 0,
            data: vec![0xAA, 0xBB],
            ..Default::default()
        };
        let enc = c.encode_reply(&reply).unwrap();
        let mut want = Vec::new();
        want.extend_from_slice(&1u32.to_le_bytes()); // command = REPLY
        want.extend_from_slice(&6u32.to_le_bytes()); // bodySize = 4 + 2
        want.extend_from_slice(&[0u8; 8]); // reserved[2]
        want.extend_from_slice(&0i32.to_le_bytes()); // status
        want.extend_from_slice(&[0xAA, 0xBB]); // data
        assert_eq!(enc, want, "REPLY must be byte-identical to the r34 spec");

        // -- DEC_STRONG: header + 32B RpcWireAddress --
        let mut ctr = 0x4142_4344u64;
        let addr = RpcAddress::unique(&mut ctr, crate::rpc::AddressSpace::Initiator);
        let enc = c.encode_dec_strong(&addr, 1).remove(0);
        assert_eq!(&enc[0..4], &2u32.to_le_bytes()); // command = DEC_STRONG
        assert_eq!(&enc[4..8], &32u32.to_le_bytes()); // bodySize = 32
        assert_eq!(&enc[8..16], &[0u8; 8]); // reserved[2]
        assert_eq!(&enc[16..48], addr.as_wire_bytes()); // address[32]
        assert_eq!(enc.len(), 48);

        // -- session preamble: bare int32, RPC_SESSION_ID_NEW = -1 --
        assert_eq!(
            c.encode_session_preamble(RPC_SESSION_ID_NEW),
            (-1i32).to_le_bytes().to_vec()
        );
    }

    /// A one-byte change in a golden header is detected: the compare is exact.
    #[test]
    fn golden_is_not_close_enough() {
        let c = R34Codec;
        let mut enc = c
            .encode_reply(&WireReply {
                status: 0,
                data: vec![],
                ..Default::default()
            })
            .unwrap();
        let original = enc.clone();
        enc[0] ^= 0x01; // flip command LSB
        assert_ne!(enc, original);
        // And the decoder rejects the corrupted command outright.
        assert!(matches!(c.decode_message(&enc), Err(RpcError::Protocol(_))));
    }

    /// Malformed input never panics or OOMs: lengths and offsets are checked before allocating.
    #[test]
    fn decoder_rejects_hostile_input_safely() {
        let c = R34Codec;
        // Too short for a header.
        assert!(c.decode_message(&[]).is_err());
        assert!(c.decode_message(&[0u8; 15]).is_err());

        // Header claims a giant bodySize but frame is short.
        let mut f = Vec::new();
        f.extend_from_slice(&0u32.to_le_bytes()); // TRANSACT
        f.extend_from_slice(&u32::MAX.to_le_bytes()); // bodySize = 4G
        f.extend_from_slice(&[0u8; 8]);
        assert!(matches!(
            c.decode_message(&f),
            Err(RpcError::FrameTooLarge { .. })
        ));

        // command=TRANSACT, bodySize=10 but only 10 body bytes (< 64).
        let mut f = Vec::new();
        f.extend_from_slice(&0u32.to_le_bytes());
        f.extend_from_slice(&10u32.to_le_bytes());
        f.extend_from_slice(&[0u8; 8]);
        f.extend_from_slice(&[0u8; 10]);
        assert!(matches!(c.decode_message(&f), Err(RpcError::Protocol(_))));

        // Unknown command.
        let mut f = Vec::new();
        f.extend_from_slice(&999u32.to_le_bytes());
        f.extend_from_slice(&0u32.to_le_bytes());
        f.extend_from_slice(&[0u8; 8]);
        assert!(matches!(c.decode_message(&f), Err(RpcError::Protocol(_))));

        // DEC_STRONG with wrong body length.
        let mut f = Vec::new();
        f.extend_from_slice(&2u32.to_le_bytes());
        f.extend_from_slice(&8u32.to_le_bytes());
        f.extend_from_slice(&[0u8; 8]);
        f.extend_from_slice(&[0u8; 8]);
        assert!(matches!(c.decode_message(&f), Err(RpcError::Protocol(_))));
    }
}
