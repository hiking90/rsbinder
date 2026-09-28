// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

/*
 * Copyright (C) 2020 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Data serialization and deserialization for binder IPC.
//!
//! This module provides the `Parcel` type for marshalling and unmarshalling data
//! in binder transactions. Parcels handle the low-level details of data layout,
//! alignment, and object references required for cross-process communication.
//!
//! All non-kernel serialization state is bundled into one struct,
//! `RpcFields` (AOSP `Parcel.h`'s `RpcFields`, the RPC arm of its
//! `std::variant<KernelFields, RpcFields> mVariantFields`). Unlike AOSP
//! there is no `KernelFields`: the kernel offset table is `Parcel::objects`,
//! a wholly separate field, so only the RPC arm has to be bundled.
//!
//! A `Parcel` carries it as `Option<RpcFields>`: `Some` ⇒ RPC mode (session
//! hooks attached, or none in the data-only mode), `None` ⇒ kernel path
//! (`Parcel::is_kernel_backed`),
//! byte-identical to the kernel wire. Tying every RPC field's existence to
//! the mode flag in the type makes "RPC mode ⇒ RPC state present" an
//! invariant the compiler enforces, instead of seven independently-defaulted
//! fields gated on a separate bool.
//!
//! Every field of `RpcFields` is behind `rpc`, the struct is not: without the
//! feature it is empty and `Some(RpcFields::default())` is exactly what
//! `Parcel::new_data_only` needs — a parcel that has no session to marshal a
//! binder or an fd through. `rpc` adds the state a session fills in.
//!
//! # RPC fields
//!
//! - `object_positions` is AOSP `RpcFields::mObjectPositions`: sorted byte
//!   offsets of flattened RPC objects (binder at android-16 v2, FD at v1+),
//!   produced by `write_binder` / the FD write while serializing and consumed
//!   by the wire codec as the trailing `u32[]` object table. The kernel path
//!   never touches it (kernel objects live in `Parcel::objects`); empty is
//!   byte-identical to a wire with no object table. Recording inserts at the
//!   `upper_bound` (AOSP `mObjectPositions.insert(upper_bound(...), dataPos)`
//!   in `flattenBinder` / `writeFileDescriptor`), an O(1) push on the usual
//!   ascending-write path. `rpc_record_object_position` is a no-op on a
//!   kernel-backed parcel, so the kernel wire never grows an object table; the
//!   caller decides whether to record (binder ⇒ v2 only, FD ⇒ v1+), mirroring
//!   AOSP's per-call version gate. Lookup is AOSP `unflattenBinder`'s v2 strict
//!   check, `std::binary_search(mObjectPositions, objectPos)`: a conformant
//!   peer sends the table sorted, and a forged or unsorted table misses the
//!   search, so the caller returns `BAD_VALUE`. An incoming table is installed
//!   by `rpc_set_object_positions` (AOSP `rpcSetDataReference`). A recorded
//!   offset is the position of the object's leading int32 (AOSP
//!   `dataPos = mDataPos` before `writeInt32(TYPE_*)`), and the v2 codec frames
//!   the table as `bodySize = fixed + parcelDataSize + 4·N`.
//! - `record_fd_positions` is set by the session from its wire profile, next
//!   to the FD mode: `true` only on the android-13+ v1+ profile. R34 has no
//!   object table, so its FD-over-RPC wire is the AOSP android-12 layout. The session
//!   records binder positions itself (it owns the profile); only the FD path
//!   needs this parcel-side flag.
//! - `fd_mode` defaults to `None`, which rejects an FD write — bit-identical
//!   to a parcel that carries no FDs. Production sets both FD fields and the
//!   hooks through `configure_rpc`; `set_rpc_fd_mode` and
//!   `set_rpc_record_fd_positions` exist for a test or fuzz target without a
//!   session. The per-message `rpc_set_in_fds` / `rpc_set_object_positions`
//!   stay separate from `configure_rpc`: they carry wire payload, not the
//!   session profile.
//! - `fds_out` collects, in `Unix` fd-mode, the owned dups serialized into
//!   this outgoing parcel; the session sends them out-of-band via
//!   `SCM_RIGHTS`, and the parcel keeps ownership and closes them on drop,
//!   after the send (the peer holds its own copies via the kernel). `fds_in`
//!   holds the fds received with an incoming parcel, indexed by the in-body
//!   fd-table index, each taken at most once.
//! - `leaving_addrs` records the session addresses of local binders whose
//!   `timesSent` was bumped (`RpcState::on_binder_leaving`) while flattened
//!   into this outgoing parcel, and at wire v2 of the peer's addresses whose
//!   proxy send was counted (`RpcState::on_proxy_leaving`). The parcel owns those bumps while it is
//!   `NotSent`: its `Drop` (and `set_for_rpc(false)`) hands them back through
//!   `RpcParcelOps::cancel_leaving`, so a parcel written and never sent, or
//!   refused before the send (`WouldBlock`, `DeadObject`, an encode failure),
//!   leaks no node and can be sent again as is. The give-back sends nothing to the peer.
//! - `send_state` is AOSP `RpcFields::mSendState`. `client_transact` and
//!   `send_reply_parcel` claim the parcel (`NotSent` → `InFlight`) for one
//!   send and, on success, mark it `Sent`, which hands `leaving_addrs` to the
//!   peer's `DEC_STRONG`; a `Sent` or `InFlight` parcel is refused with
//!   `InvalidOperation`, so one parcel is sent once. A parcel built from a
//!   peer's bytes is `Received` (AOSP `RECEIVED`) and is refused the same
//!   way. Both send paths first compare `RpcParcelOps::session_id` with their
//!   own session (AOSP `validateParcel`, `BadType`), and `append_from` into a
//!   session parcel takes a source of that session only. `Drop` settles unless the
//!   parcel is `Sent`: an `InFlight` one at drop is a claim a panic left,
//!   and no send completed. AOSP settles an unsent
//!   parcel on destruction only at wire v2 (`mObjectPositions` holds binder
//!   positions only there); rsbinder records `leaving_addrs` on every profile
//!   and settles on all of them.
//! - `pinned` holds the remote proxies flattened into this outgoing parcel
//!   until it drops (AOSP keeps argument refs until the reply is out). Below
//!   wire v2 that is what orders a proxy's `DEC_STRONG` after the send naming
//!   it; at v2 the counted send holds the release until the peer pays it back
//!   (`rpc::state` "Ref-count model"). A binder, proxy or local, is written
//!   only into a `NotSent` parcel.
//! - `entered` is AOSP `RpcFields::mAcquiredEnteringBinders` (android-16.0.0_r4):
//!   for a `Received` parcel, the address and binder each object position
//!   entered, sorted by position and found by binary search. The first read of
//!   a position enters it (`RpcSessionInner::read_binder`: one of our nodes is
//!   paid back with `DEC_STRONG` 1 at once, a peer address resolves to its
//!   deduped proxy) and records it. A later read of the position consumes the
//!   same bytes and returns the recorded binder without entering again; an
//!   address other than the recorded one is `BadValue`. At wire v2 the session
//!   enters every binder position when the parcel arrives. Only a position
//!   below `received_len`, the data length at receipt, is entered. A parcel
//!   that is not `Received`, or a position past that length, only looks the
//!   address up (one of our nodes, then a live proxy, else `BadValue`) and owes
//!   nothing. The parcel holds what it entered until it drops, when an entered
//!   proxy sends its `DEC_STRONG`; so a received parcel must not drop under the
//!   `RpcState` lock, which that send re-takes.
//!
//! # Data-only parcels
//!
//! `Parcel::new_data_only` is the session-less RPC mode: the mode the RPC
//! transport uses, without the session that would give a binder somewhere to
//! go. With no ops attached and `fd_mode` at `None`, the RPC write paths reject
//! a binder and an fd on their own, and `append_from` — the one path that
//! copies another parcel's bytes wholesale — has its own write-time refusal.
//! All of them refuse before anything is written, so there is no second check
//! on the finished bytes to keep in step. It is not behind `rpc`: the
//! refusals follow from the missing session, not from the transport. On the
//! read side (`Parcel::from_slice`) data-only makes `read_object` an immediate
//! `BadType`, so a forged `flat_binder_object` in the input cannot become a
//! binder.
//!
//! `Parcel::is_self_contained` checks the kernel object table, the RPC object
//! positions and the out-of-band fds. The kernel table is empty by
//! construction on an RPC parcel, so checking it alone would hand out the
//! bytes of a parcel carrying binders — bytes meaningless without the object
//! table that travelled beside them.
//!
//! # `ParcelPod`
//!
//! `Parcel::write_aligned`, `write_array` and `read_array` reinterpret a `&T`
//! / `&[T]` as raw bytes (and, on read, raw bytes as `T`). That is sound only
//! for a type with no padding or otherwise-uninitialized bytes (those bytes
//! would leak process memory to the peer, and reading them is UB) and for
//! which every bit pattern is a valid value (a peer's bytes become a `T`
//! without validation). The `T: ParcelPod` bound carries that obligation in
//! the signature instead of leaving it to whichever caller instantiates the
//! generic.
//!
//! Safety contract for an implementor: `#[repr(C)]` / `#[repr(transparent)]`
//! or a primitive, no padding, and valid for every bit pattern of its size.
//! For the bindgen binder-ABI structs every union member must also be written
//! full-width by the constructor (`flat_binder_object::new_*` do; see the
//! `*_full_width_init` tests in `binder_object.rs`).
//!
//! # Wire and native scalars
//!
//! `WireScalar` is the wire layer. The data-parcel wire is little-endian on
//! every host, so a scalar is encoded with `to_le_bytes` rather than copied out
//! of memory. On a little-endian host that is the identity, so the emitted
//! bytes and the generated code are unchanged; a big-endian host pays a swap
//! and gains a parcel its peers can read. `NativeScalar` is the kernel command
//! stream — the `BC_*` / `BR_*` ioctl buffer the driver parses with native
//! loads — and stays host-native, as do the UAPI structs (`ParcelPod`). Both
//! traits exist because the two layers share one `Parcel` and the same 4-byte
//! slots, so only the name at the call site says which contract is in force:
//! a value going to the driver must not be byte-swapped, a value going to a
//! peer must. `Bytes` is `[u8; size_of::<Self>()]` spelled as an associated
//! type, because stable Rust cannot use an associated const as an array
//! length in a trait signature.
//!
//! `Parcel::write_le` (L1) takes a value already widened to its wire type
//! (`i8`/`u8`/`i16` → `i32`, `u16` → `u32`): writing the narrow value would
//! emit one or two bytes and desync everything after it, which the
//! `wire_golden` widening test pins. `Parcel::write_native` / `read_native`
//! (L2) carry the command stream — `BC_*` codes, handles, cookies — and the
//! command stream reaches them only through `command_stream::CommandStream`,
//! whose private field exposes no L1 method: the type enforces the layer, not
//! a list of spellings.
//!
//! # Array length checks
//!
//! `checked_array_layout` computes `(size, padded)` for `len` elements of
//! `elem_size` bytes and returns `BadValue` when either would overflow
//! `usize`. On 32-bit targets (armv7 Android, i686 Linux) a hostile
//! `len * size_of::<D>()` could wrap to a small `size` that passes the later
//! `padded > data_avail()` check and reaches `Vec::with_capacity(len)`, which
//! aborts with a capacity-overflow panic — a remote DoS through parcel input.
//! On 64-bit, any `i32` `len` times a realistic element size stays far below
//! `usize::MAX`, so the result there equals unchecked arithmetic. The caller
//! validates `len >= 1` first: `len == 0` would give `size == 0`,
//! indistinguishable from a wrap to zero, and a `debug_assert!` rejects it.
//!
//! # Kernel receive buffers
//!
//! For an empty IPC parcel the binder driver still allocates a buffer and
//! returns its user-space address, which must go back verbatim in
//! `BC_FREE_BUFFER`. `ParcelData::from_raw_parts_mut` therefore keeps a
//! non-null `data` with `len == 0` as a real slice and falls back to a
//! dangling `&[]` only when `data` itself is null.
//!
//! That buffer is read-only in this process: `binder_mmap` refuses a
//! mapping with `VM_WRITE`, and `ProcessState` maps it `PROT_READ`. So
//! `ParcelData::Slice` holds a shared `&[T]`, and every `ParcelData` method
//! that would write through it (`as_mut_slice`, `as_mut_ptr`, `reserve`,
//! `set_len`, `push`) panics instead. Every write path reserves first, so a
//! write into a received parcel stops at that panic.
//!
//! `ParcelData::set_len` grows in two places: the write paths, after
//! initializing `old_len..end` themselves ("Buffer growth"), and
//! `Parcel::set_data_size_driver_filled`, after the binder driver filled the
//! spare capacity through `as_mut_ptr` — `talk_with_driver` calls it with
//! `read_consumed`, the count the driver reports having written.
//! `Parcel::set_data_size` only shrinks.
//!
//! # Kernel proxies
//!
//! A kernel parcel holds every proxy (`BINDER_TYPE_HANDLE`) written into it
//! strong in `kernel_pinned` until it drops (AOSP `acquire_object`); a parcel
//! the driver filled pins nothing. A handler that
//! returns a proxy it received may drop every other strong ref to it before
//! `BC_REPLY` is written; `BC_RELEASE` and `BC_FREE_BUFFER` queued ahead of the
//! reply would then take the handle's last strong ref and the kernel would
//! reject the reply. A binder of the process's own travels as
//! `BINDER_TYPE_BINDER` and needs no pin.
//!
//! # Buffer growth
//!
//! Every write path (`write_aligned_data`, `write_array`, `write_array_char`,
//! `append_from`) bounds its end position to `i32::MAX` before reserving: wire
//! offsets are `int32`, so a larger position can only come from a misuse of the
//! public `set_data_position`, and it is refused as `BadValue` rather than
//! overflowed into a wild `reserve`/`write_bytes` (AOSP `Parcel::growData`
//! returns `BAD_VALUE` for `len > INT32_MAX`).
//!
//! `reserve` allocates without initializing, and `set_len` then marks the bytes
//! initialized, so every byte below the new length is written first:
//!
//! - the `len..pos` gap a forward `set_data_position` leaves is zero-filled
//!   (AOSP `growData` zero-fills grown capacity the same way);
//! - the 0-3 trailing pad bytes of an unaligned write are zeroed. The pad is
//!   transmitted (it counts in `data_size`), and AOSP masks it to zero too.
//!
//! Skipping either is UB and sends uninitialized process memory to the peer.
//!
//! # `append_from`
//!
//! `Parcel::append_from` copies a byte range but never the table that made
//! those bytes safe, so it refuses, before any byte is copied, each case where
//! the bytes would change meaning in the destination:
//!
//! - RPC or data-only source into a kernel destination: `read_object`'s
//!   null-meta shortcut accepts a null-pointer, null-cookie
//!   `flat_binder_object` with no offset-table entry, so 24 bytes of payload
//!   would become `BINDER_TYPE_HANDLE` handle 0, a live proxy to the context
//!   manager. The opposite direction needs no gate: `read_object` is an
//!   immediate `BadType` on an RPC-mode parcel.
//! - A source with session hooks into a hook-less destination: the source can
//!   carry an `RpcAddress` flattened into its body, and only the v2 wire
//!   records where, so no table proves a range clean. The hook-less sink is
//!   the one whose bytes get exported (`Parcel::as_bytes`).
//! - Into a session destination, any source but a parcel of the same session:
//!   a kernel parcel, a data-only one (`from_bytes`, a stream item) or another
//!   session's. AOSP refuses the same pairs by `isForRpc()` and `mSession`
//!   (`Parcel::appendFrom`, android-16.0.0_r4). Otherwise bytes no session
//!   vouched for would reach the peer, which reads a `1` followed by an address
//!   as a binder: its own node, or a proxy to one of ours that took no
//!   reference, whose `DEC_STRONG` then frees a node another proxy still uses.
//! - Kernel objects into an RPC-mode destination: it has no object table, so a
//!   `flat_binder_object` would survive as payload carrying a process-local
//!   handle or fd number, indistinguishable from data once copied.
//!
//! A copy within one session carries its objects, as AOSP `appendFrom` does
//! from android-16.0.0_r4. Each source position inside the range moves to the
//! destination, shifted by `start - offset`. At a binder the address is looked
//! up and takes one more `timesSent` (`RpcParcelOps::acquire_copied`, AOSP
//! `lookupAddress` + `onBinderLeaving`), recorded in `leaving_addrs` so the
//! destination settles it like one it wrote; a proxy of the peer's is pinned,
//! as `write_binder` pins it. The copy runs only at wire v2 (below), where a
//! received source entered every binder position when it arrived and holds
//! the proxy in its `entered` table ("RPC fields"), as does a
//! `ParcelableHolder` payload cut from it, so a peer address from a received
//! source finds a live proxy to pin whether or not the position was read. A
//! peer address with no live proxy, from a source that neither received nor
//! wrote it, is copied without a pin. All addresses of one copy
//! go under one `RpcState` lock. An address that names no node of ours, or whose node
//! answers with another address, refuses the copy with `BadValue`; AOSP shuts
//! the session down there, rsbinder keeps it as `remote_proxy` does for an
//! unknown address. At an fd the source's fd is dup'd into the destination's
//! outgoing table and the in-body index is rewritten to that slot. The slot
//! is the destination's table size before the push, because fds written
//! before the append already hold the lower slots; AOSP android17-release
//! writes `otherRpcFields->mFds.size() - 1`, the source's size, which
//! names the right slot only when the destination had no fd and the range
//! holds the source's last one.
//!
//! This needs binder positions, which only the v2 wire records. On r34 and
//! android-13+ v0 and v1 a binder is a `1` and an address with nothing marking
//! where, so no copy can find its binders to take their references, and no
//! range can be proven binder-free. There every non-empty copy into a session
//! parcel is `BadType`: the payload has to be decoded and written again. v1
//! records fd positions, but the refusal is about binders and holds there too.
//! Every step that can fail (the refusals, the fd dups, `acquire_copied`) runs
//! before a byte is copied, so a refused copy leaves the destination as it was.
//!
//! `Parcel::sub_parcel` is the read-side counterpart, for `ParcelableHolder`:
//! the payload becomes its own parcel with the source's mode and session
//! profile (hooks, fd mode, fd-position flag), the positions inside the range
//! shifted to its start, and each v1+ fd in range moved out of the source's
//! received table into its own, with the index rewritten. It is `Received`
//! exactly when the source is, and it gets a clone of each `entered` entry in
//! range (a clone, so the source's own reads of those positions still find
//! them). At v2 that is every binder in the payload. On r34, v0 and v1 the
//! source entered none of them, so the sub-parcel enters each on its first
//! read and keeps it for as long as the holder keeps the payload: a
//! `get_parcelable` that fails after a binder and is retried reads the
//! recorded binder instead of paying the receipt twice. A sub-parcel of a
//! parcel that was not received only looks addresses up. C++
//! `ParcelableHolder` has no counterpart to follow: it cannot receive a
//! non-empty holder over RPC at all (android-17.0.0_r1 `ParcelableHolder.cpp:86-90`
//! reads into a kernel parcel, whose `appendFrom` refuses an RPC source,
//! `Parcel.cpp:607-611`). A copy of it sent later goes
//! through `append_from` above. Kernel mode uses `append_from` itself. r34 and
//! v0 fds carry no position, so they stay behind and reading one fails.
//!
//! Relocation scans the source's object table, as AOSP `Parcel.cpp::appendFrom`
//! iterates `other`'s `mObjects`: the destination's table may be empty (a fresh
//! `ParcelableHolder` parcel) and would drop every nested object. Each offset
//! (AOSP `off = pos - offset + startPos`) is pushed into `objects` only after
//! the object is acquired and, for an fd, dup'd and rewritten with the
//! destination's own fd. When either step fails the offset is not committed,
//! so `Drop` (`release_objects`) never releases the source's still-owned fd
//! (double close) or a refcount it never took. AOSP gets the same result by
//! never aborting its loop.
//!
//! # Binder-ABI structs
//!
//! `flat_binder_object`, `binder_transaction_data` and
//! `binder_transaction_data_secctx` implement `ParcelPod`: bindgen `#[repr(C)]`
//! structs of integers, pointers-as-integers and unions of those, with no
//! padding (8-byte-multiple field groups), so every bit pattern is valid. Their
//! union members are written full-width by the constructors in
//! `binder_object.rs` and by `write_transaction_data` (`ptr: 0` before
//! `.handle`).

use std::default::Default;
use std::vec::Vec;

use pretty_hex::*;
use rustix::fd::IntoRawFd;

use crate::{
    binder,
    binder_object::{read_flat_binder, write_flat_binder},
    error::{Result, StatusCode},
    parcelable::*,
    sys::binder::{binder_size_t, flat_binder_object},
    sys::{binder_uintptr_t, BINDER_TYPE_FD, BINDER_TYPE_HANDLE},
    thread_state,
};

const STRICT_MODE_PENALTY_GATHER: i32 = 1 << 31;

/// Implementors: no padding, every bit pattern valid; `# Safety` is module doc "`ParcelPod`".
#[allow(clippy::missing_safety_doc)]
pub(crate) unsafe trait ParcelPod: Copy {}

macro_rules! impl_parcel_pod {
    ($($t:ty),* $(,)?) => { $(
        // SAFETY: primitive integer/float types: no padding, every bit pattern valid.
        unsafe impl ParcelPod for $t {}
    )* };
}
impl_parcel_pod!(i8, u8, i16, u16, i32, u32, i64, u64, u128, f32, f64);

/// A scalar whose *wire* form is little-endian; see module doc "Wire and native scalars".
pub(crate) trait WireScalar: Copy {
    /// `[u8; size_of::<Self>()]`; a type because a const can't size an array in a trait signature.
    type Bytes: AsRef<[u8]>;

    fn to_wire(self) -> Self::Bytes;
    fn from_wire(bytes: &[u8]) -> Result<Self>;
}

/// Host-native `BC_*`/`BR_*` command-stream scalar; see module doc "Wire and native scalars".
pub(crate) trait NativeScalar: Copy {
    type Bytes: AsRef<[u8]>;

    fn to_native(self) -> Self::Bytes;
    fn from_native(bytes: &[u8]) -> Result<Self>;
}

macro_rules! impl_scalar_codecs {
    ($($t:ty),* $(,)?) => { $(
        impl WireScalar for $t {
            type Bytes = [u8; std::mem::size_of::<$t>()];

            fn to_wire(self) -> Self::Bytes {
                self.to_le_bytes()
            }

            fn from_wire(bytes: &[u8]) -> Result<Self> {
                Ok(<$t>::from_le_bytes(bytes.try_into()?))
            }
        }

        impl NativeScalar for $t {
            type Bytes = [u8; std::mem::size_of::<$t>()];

            fn to_native(self) -> Self::Bytes {
                self.to_ne_bytes()
            }

            fn from_native(bytes: &[u8]) -> Result<Self> {
                Ok(<$t>::from_ne_bytes(bytes.try_into()?))
            }
        }
    )* };
}
impl_scalar_codecs!(i8, u8, i16, u16, i32, u32, i64, u64, u128, f32, f64);

// SAFETY: padding-free integer-only `#[repr(C)]` structs; see module doc "Binder-ABI structs".
unsafe impl ParcelPod for flat_binder_object {}
unsafe impl ParcelPod for crate::sys::binder_transaction_data {}
unsafe impl ParcelPod for crate::sys::binder_transaction_data_secctx {}

#[inline]
pub(crate) fn pad_size(len: usize) -> usize {
    (len + 3) & (!3)
}

/// Overflow-checked `(size, padded)` for `len >= 1` elements; see module doc "Array length checks".
#[inline]
pub(crate) fn checked_array_layout(len: i32, elem_size: usize) -> Result<(usize, usize)> {
    debug_assert!(
        len >= 1,
        "checked_array_layout: caller must validate len >= 1"
    );
    let size = (len as usize)
        .checked_mul(elem_size)
        .ok_or(StatusCode::BadValue)?;
    // `pad_size` would overflow at `size + 3`, hence `checked_add`.
    let padded = size.checked_add(3).ok_or(StatusCode::BadValue)? & !3;
    Ok((size, padded))
}

/// Byte cap of AOSP `Parcel::readOutVectorSizeWithCheck` for an `out`/`inout` array.
const MAX_OUT_VEC_BYTES: usize = 1_000_000;

// An out-vec length is not backed by parcel bytes, so it is capped by size, not `data_avail()`.
fn check_out_vec_size<D>(len: usize) -> Result<()> {
    match len.checked_mul(std::mem::size_of::<D>()) {
        Some(bytes) if bytes < MAX_OUT_VEC_BYTES => Ok(()),
        _ => Err(StatusCode::NoMemory),
    }
}

pub(crate) trait CharType: Clone {
    type Output;
    fn as_i32(&self) -> i32;
    fn from(v: &i32) -> Self::Output;
}

impl CharType for i16 {
    type Output = i16;
    fn as_i32(&self) -> i32 {
        *self as _
    }
    fn from(v: &i32) -> Self::Output {
        *v as _
    }
}

impl CharType for u16 {
    type Output = u16;
    fn as_i32(&self) -> i32 {
        *self as _
    }
    fn from(v: &i32) -> Self::Output {
        *v as _
    }
}

pub(crate) enum ParcelData<T: Clone + Default + 'static> {
    Vec(Vec<T>),
    Slice(&'static [T]),
}

impl<T: Clone + Default> ParcelData<T> {
    fn new() -> Self {
        ParcelData::Vec(Vec::new())
    }

    fn with_capacity(capacity: usize) -> Self {
        ParcelData::Vec(Vec::with_capacity(capacity))
    }

    // Adopts a ready-made buffer: the RPC stack, and `from_bytes` decoding a stored value.
    fn from_vec(data: Vec<T>) -> Self {
        ParcelData::Vec(data)
    }

    /// # Safety: null only with `len == 0`; else aligned readable `len` `T`s, exclusive till freed.
    // A non-null empty buffer stays a slice: see module doc "Kernel receive buffers".
    unsafe fn from_raw_parts_mut(data: *mut T, len: usize) -> Self {
        ParcelData::Slice(if data.is_null() {
            debug_assert_eq!(len, 0, "non-zero length with null data is invalid");
            &[]
        } else {
            // SAFETY: non-null here; the `# Safety` contract covers reads, alignment, exclusivity.
            unsafe { std::slice::from_raw_parts(data, len) }
        })
    }

    fn as_slice(&self) -> &[T] {
        match self {
            ParcelData::Vec(v) => v.as_slice(),
            ParcelData::Slice(s) => s,
        }
    }

    fn as_mut_slice(&mut self) -> &mut [T] {
        // A `Slice` is a read-only kernel mapping: module doc "Kernel receive buffers".
        match self {
            ParcelData::Vec(v) => v.as_mut_slice(),
            _ => panic!("&[u8] can't support as_mut_slice()."),
        }
    }

    pub(crate) fn as_ptr(&self) -> *const T {
        match self {
            ParcelData::Vec(ref v) => v.as_ptr(),
            ParcelData::Slice(s) => s.as_ptr(),
        }
    }

    fn as_mut_ptr(&mut self) -> *mut T {
        match self {
            ParcelData::Vec(ref mut v) => v.as_mut_ptr(),
            _ => panic!("&[u8] can't support as_mut_ptr()."),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.as_slice().len()
    }

    /// # Safety: `len <= capacity()`, `0..len` initialized (module doc "Kernel receive buffers").
    unsafe fn set_len(&mut self, len: usize) {
        match self {
            // SAFETY: the caller upholds `len <= capacity` and that `0..len` is initialized.
            ParcelData::Vec(v) => unsafe { v.set_len(len) },
            _ => panic!("&[u8] can't support set_len()."),
        }
    }

    fn capacity(&self) -> usize {
        match self {
            ParcelData::Vec(v) => v.capacity(),
            ParcelData::Slice(s) => s.len(),
        }
    }

    fn reserve(&mut self, additional: usize) {
        match self {
            ParcelData::Vec(v) => v.reserve(additional),
            _ => panic!("&[u8] can't support reserve()."),
        }
    }

    fn push(&mut self, other: T) {
        match self {
            ParcelData::Vec(v) => v.push(other),
            _ => panic!("push() is only available for ParcelData::Vec."),
        }
    }
}

pub(crate) type FnFreeBuffer =
    fn(Option<&Parcel>, binder_uintptr_t, usize, binder_uintptr_t, usize) -> Result<()>;

/// RPC-mode `SIBinder` (de)serialization hooks, implemented by `rpc` (AOSP `mSession`/`RpcState`).
#[cfg(feature = "rpc")]
pub(crate) trait RpcParcelOps: Send + Sync {
    /// Append a possibly-null binder as the r34 RPC object (`i32` present flag + 32B address).
    fn write_binder(
        &self,
        binder: Option<&crate::binder::SIBinder>,
        parcel: &mut Parcel,
    ) -> Result<()>;
    /// Unmarshal a binder entering this process from the RPC encoding.
    fn read_binder(&self, parcel: &mut Parcel) -> Result<Option<crate::binder::SIBinder>>;
    /// Give back the `timesSent` bumps of a parcel dropped unsent (AOSP `cancelBinderLeaving`).
    fn cancel_leaving(&self, addrs: &[crate::rpc::RpcAddress]);
    /// The session's address (AOSP `RpcFields::mSession`); null for session-less test ops.
    fn session_id(&self) -> *const ();
    /// Whether the wire records binder positions (android-16 v2); `DeadObject` after session end.
    fn records_binder_positions(&self) -> Result<bool>;
    /// AOSP `appendFrom` `TYPE_BINDER` arm for each copied address (bytes after the type word).
    fn acquire_copied(&self, objects: &[&[u8]]) -> Result<CopiedBinders>;
}

/// What [`RpcParcelOps::acquire_copied`] took: bumps the copy settles, proxies it pins.
#[cfg(feature = "rpc")]
#[derive(Default)]
pub(crate) struct CopiedBinders {
    /// Counted sends (local nodes; proxies at v2), recorded in the destination's `leaving_addrs`.
    pub(crate) leaving: Vec<crate::rpc::RpcAddress>,
    /// Live proxies of the peer's addresses, held like `write_binder`'s pin.
    pub(crate) pinned: Vec<crate::binder::SIBinder>,
}

/// A session `append_from` after its fallible steps: offsets are relative to the copied range.
#[cfg(feature = "rpc")]
struct StagedRpcCopy {
    positions: Vec<usize>,
    /// Offset of each fd's index word, and the dup that takes that slot.
    fds: Vec<(usize, std::os::fd::OwnedFd)>,
    binders: CopiedBinders,
}

/// AOSP `RpcFields::ObjectType::TYPE_BINDER`, the word at a recorded binder position.
#[cfg(feature = "rpc")]
const RPC_TYPE_BINDER: i32 = 1;

/// The little-endian `i32` at `pos`, or `None` past the end of `bytes`.
#[cfg(feature = "rpc")]
fn le_i32_at(bytes: &[u8], pos: usize) -> Option<i32> {
    let word = bytes.get(pos..pos.checked_add(4)?)?;
    Some(i32::from_le_bytes(word.try_into().ok()?))
}

/// AOSP `RpcFields::mSendState`; see the module doc "RPC fields".
#[cfg(feature = "rpc")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum RpcSendState {
    NotSent = 0,
    /// Claimed by one `client_transact` or `send_reply_parcel`; a second sender is refused.
    InFlight = 1,
    Sent = 2,
    /// A peer's bytes (AOSP `RECEIVED`): refused as a send, local-binder or `append_from` sink.
    Received = 3,
}

#[cfg(feature = "rpc")]
impl RpcSendState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::InFlight,
            2 => Self::Sent,
            3 => Self::Received,
            _ => Self::NotSent,
        }
    }
}

/// All non-kernel serialization state of a [`Parcel`] (AOSP `RpcFields`); see the module doc.
#[derive(Default)]
struct RpcFields {
    /// Object-marshalling hooks (AOSP `mSession`); `Some` only on a parcel that carries binders.
    #[cfg(feature = "rpc")]
    ops: Option<std::sync::Arc<dyn RpcParcelOps>>,
    /// Negotiated FD-over-RPC mode; the default `None` rejects FD writes.
    #[cfg(feature = "rpc")]
    fd_mode: crate::rpc::FileDescriptorTransportMode,
    /// Outgoing fds (`Unix` fd-mode), sent out-of-band via `SCM_RIGHTS`, closed on drop.
    #[cfg(feature = "rpc")]
    fds_out: Vec<std::os::fd::OwnedFd>,
    /// Incoming out-of-band fds, indexed by the in-body fd-table index.
    #[cfg(feature = "rpc")]
    fds_in: Vec<Option<std::os::fd::OwnedFd>>,
    /// AOSP `mObjectPositions`: sorted RPC object offsets; see module doc "RPC fields".
    #[cfg(feature = "rpc")]
    object_positions: Vec<u32>,
    /// Whether a flattened FD records its position (android-13+ v1+ profile only).
    #[cfg(feature = "rpc")]
    record_fd_positions: bool,
    /// Local binders whose `timesSent` this parcel bumped; settled by `Drop` unless it was sent.
    #[cfg(feature = "rpc")]
    leaving_addrs: Vec<crate::rpc::RpcAddress>,
    /// AOSP `mSendState`; `NotSent` owns `leaving_addrs`, `Sent` handed them to the peer.
    #[cfg(feature = "rpc")]
    send_state: std::sync::atomic::AtomicU8,
    /// Remote proxies held until drop; below v2 this orders their `DEC_STRONG` after this send.
    #[cfg(feature = "rpc")]
    pinned: Vec<crate::binder::SIBinder>,
    /// AOSP `mAcquiredEnteringBinders`: what each object position entered, sorted by position.
    #[cfg(feature = "rpc")]
    entered: Vec<(u32, crate::rpc::RpcAddress, crate::binder::SIBinder)>,
    /// Data length when marked `Received`; a binder past it did not arrive with the parcel.
    #[cfg(feature = "rpc")]
    received_len: usize,
}

/// Logic behind the `Parcel::rpc_*` accessors, which pick the kernel-mode no-op/default.
#[cfg(feature = "rpc")]
impl RpcFields {
    fn send_state(&self) -> RpcSendState {
        RpcSendState::from_u8(self.send_state.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Unless `Sent`, hand the recorded bumps back once; `InFlight` here is a claim a panic left.
    fn settle_unsent(&mut self) {
        if self.send_state() == RpcSendState::Sent || self.leaving_addrs.is_empty() {
            return;
        }
        let addrs = std::mem::take(&mut self.leaving_addrs);
        if let Some(ops) = &self.ops {
            ops.cancel_leaving(&addrs);
        }
    }

    /// Sorted insert at the `upper_bound` (AOSP), an O(1) push on ascending writes.
    fn record_object_position(&mut self, pos: usize) {
        let pos = pos as u32;
        let at = self.object_positions.partition_point(|&p| p <= pos);
        self.object_positions.insert(at, pos);
    }

    /// AOSP `unflattenBinder` v2 strict check; a forged/unsorted table misses (`BAD_VALUE`).
    fn object_position_present(&self, pos: usize) -> bool {
        let Ok(pos) = u32::try_from(pos) else {
            return false;
        };
        self.object_positions.binary_search(&pos).is_ok()
    }

    /// Stash an outgoing fd (already an owned dup); return its in-body table index.
    fn push_out_fd(&mut self, fd: std::os::fd::OwnedFd) -> i32 {
        let idx = self.fds_out.len() as i32;
        self.fds_out.push(fd);
        idx
    }

    /// Install the fds received out-of-band, before deserialization.
    fn set_in_fds(&mut self, fds: Vec<std::os::fd::OwnedFd>) {
        self.fds_in = fds.into_iter().map(Some).collect();
    }

    /// Take the received fd at table `index` (consumed once).
    fn take_in_fd(&mut self, index: usize) -> Option<std::os::fd::OwnedFd> {
        self.fds_in.get_mut(index).and_then(Option::take)
    }

    /// The fd a copy dups for table `index`: a received table if there is one, else the outgoing.
    fn source_fd(&self, index: usize) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        if self.fds_in.is_empty() {
            self.fds_out.get(index).map(AsFd::as_fd)
        } else {
            self.fds_in.get(index)?.as_ref().map(AsFd::as_fd)
        }
    }
}

/// Max [`Parcel::sized_read`] nesting; stops a hostile nested payload before stack overflow.
const MAX_NESTED_READ_DEPTH: usize = 1000;

/// Parcel converts data into a byte stream (serialization), making it transferable.
/// The receiving side then transforms this byte stream back into its original data form (deserialization).
///
/// A `Parcel` is the fundamental data container for binder IPC, handling serialization
/// and deserialization of primitive types, strings, objects, and file descriptors.
/// It maintains proper alignment and object reference tracking required by the binder protocol.
///
/// # Byte order
///
/// The data a parcel carries is **little-endian on every host**, so its
/// bytes mean the same thing on the machine that reads them as on the one
/// that wrote them — see the crate docs' *Wire byte order*. What a parcel
/// holds is not all wire, though: an object written into a kernel parcel
/// is a `flat_binder_object`, a UAPI struct the driver parses with native
/// loads, and it stays host-native. On a big-endian host a kernel parcel
/// carrying a binder is therefore a mixture — little-endian scalars
/// around a native object header — and correctly so, because only the
/// scalars are going to a peer.
///
/// Bytes that leave the process as *data* do not contain that mixture for
/// a value carrying no object: `rsbinder::to_bytes` encodes through a
/// parcel that refuses binders and file descriptors outright, so what it
/// hands back is pure wire. One shape still slips past those refusals —
/// see `to_bytes`'s *Byte order*.
pub struct Parcel {
    data: ParcelData<u8>,
    pub(crate) objects: ParcelData<binder_size_t>,
    pos: usize,
    next_object_hint: usize,
    /// End of the innermost [`Parcel::sized_read`] block, which bounds [`Parcel::has_more_data`].
    read_boundary: Option<usize>,
    /// Current [`Parcel::sized_read`] depth, capped at [`MAX_NESTED_READ_DEPTH`].
    nested_read_depth: usize,
    request_header_present: bool,
    work_source_request_header_pos: usize,
    free_buffer: Option<FnFreeBuffer>,
    /// RPC state, `None` on the kernel path; only objects and FDs branch on it. See [`RpcFields`].
    rpc: Option<RpcFields>,
    /// Kernel proxies written here, held strong until the parcel drops (AOSP `acquire_object`).
    kernel_pinned: Vec<crate::binder::SIBinder>,
}

impl Default for Parcel {
    fn default() -> Self {
        Parcel::with_capacity(256)
    }
}

impl Parcel {
    /// Create a new empty parcel with default capacity.
    pub fn new() -> Self {
        Parcel::with_capacity(256)
    }

    /// Create a new parcel with the specified initial capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Parcel {
            data: ParcelData::with_capacity(capacity),
            objects: ParcelData::new(),
            pos: 0,
            next_object_hint: 0,
            read_boundary: None,
            nested_read_depth: 0,
            request_header_present: false,
            work_source_request_header_pos: 0,
            free_buffer: None,
            rpc: None,
            kernel_pinned: Vec::new(),
        }
    }

    /// # Safety
    /// - `data` must be valid for reads of `length` bytes, or null if `length` is 0
    /// - `objects` must be valid for reads of `object_count` elements, or null if `object_count` is 0
    /// - Neither pointer needs alignment. A 32-bit kernel places the offsets array at
    ///   `ALIGN(data_size, sizeof(void *))`, 4 bytes, so `objects` may be misaligned for
    ///   `binder_size_t` (`u64`); such an array is copied out instead of borrowed
    /// - The memory must remain valid until the Parcel is dropped or `free_buffer` is called
    /// - Neither buffer may be accessed through any other pointer or reference, nor passed to
    ///   another `from_ipc_parts` call, until the Parcel is dropped or `free_buffer` is called
    pub unsafe fn from_ipc_parts(
        data: *mut u8,
        length: usize,
        objects: *mut binder_size_t,
        object_count: usize,
        free_buffer: fn(
            Option<&Parcel>,
            binder_uintptr_t,
            usize,
            binder_uintptr_t,
            usize,
        ) -> Result<()>,
    ) -> Self {
        Parcel {
            // SAFETY: `# Safety`: `data` readable, unshared until freed; `u8` needs no alignment.
            data: unsafe { ParcelData::from_raw_parts_mut(data, length) },
            objects: if (objects as usize) % std::mem::align_of::<binder_size_t>() == 0 {
                // SAFETY: `# Safety`: `objects` readable, unshared; alignment checked above.
                unsafe { ParcelData::from_raw_parts_mut(objects, object_count) }
            } else {
                ParcelData::Vec(
                    (0..object_count)
                        // SAFETY: `# Safety` makes `object_count` elements readable; no alignment.
                        .map(|i| unsafe { objects.add(i).read_unaligned() })
                        .collect(),
                )
            },
            pos: 0,
            next_object_hint: 0,
            read_boundary: None,
            nested_read_depth: 0,
            request_header_present: false,
            work_source_request_header_pos: 0,
            free_buffer: Some(free_buffer),
            rpc: None,
            kernel_pinned: Vec::new(),
        }
    }

    pub(crate) fn from_vec(data: Vec<u8>) -> Self {
        Parcel {
            data: ParcelData::from_vec(data),
            objects: ParcelData::new(),
            pos: 0,
            next_object_hint: 0,
            read_boundary: None,
            nested_read_depth: 0,
            request_header_present: false,
            work_source_request_header_pos: 0,
            free_buffer: None,
            rpc: None,
            kernel_pinned: Vec::new(),
        }
    }

    /// A parcel that refuses binders and fds; see module doc "Data-only parcels".
    pub(crate) fn new_data_only() -> Self {
        let mut p = Parcel::new();
        p.set_for_rpc(true);
        p
    }

    /// A data-only parcel over a copy of `bytes`; a forged object in it reads as `BadType`.
    pub(crate) fn from_slice(bytes: &[u8]) -> Self {
        let mut p = Parcel::from_vec(bytes.to_vec());
        p.set_for_rpc(true);
        p
    }

    /// No kernel object, RPC object position or out-of-band fd (all three: see module doc).
    pub(crate) fn is_self_contained(&self) -> bool {
        if self.objects.len() != 0 {
            return false;
        }
        #[cfg(feature = "rpc")]
        if !self.rpc_object_positions().is_empty() || !self.rpc_out_fds().is_empty() {
            return false;
        }
        true
    }

    /// The bytes, or `BadType` unless [`Parcel::is_self_contained`] (always true if data-only).
    pub(crate) fn as_bytes(&self) -> Result<&[u8]> {
        if !self.is_self_contained() {
            return Err(StatusCode::BadType);
        }
        Ok(self.data.as_slice())
    }

    /// [`Parcel::as_bytes`], taking ownership.
    pub(crate) fn into_bytes(self) -> Result<Vec<u8>> {
        self.as_bytes().map(<[u8]>::to_vec)
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// `len`, not `data_size()` (`max(len, pos)`), which a forward seek pushes past the buffer.
    pub(crate) fn ipc_data_size(&self) -> usize {
        self.data.len()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.data.capacity()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// Switch between kernel (default) and RPC mode; scalar/string/POD bytes are identical in both.
    pub(crate) fn set_for_rpc(&mut self, yes: bool) {
        if yes {
            // Keep any `RpcFields` already configured, e.g. by an earlier `attach_rpc_ops`.
            self.rpc.get_or_insert_with(RpcFields::default);
        } else {
            // The fields go away with the mode: an unsent parcel's bumps go back first.
            #[cfg(feature = "rpc")]
            if let Some(rpc) = self.rpc.as_mut() {
                rpc.settle_unsent();
            }
            self.rpc = None;
        }
    }

    /// Test-only door to [`Parcel::set_for_rpc`], for a test in another
    /// crate that needs a session-less RPC-mode parcel. Production code
    /// reaches this mode through `Parcel::configure_rpc`.
    #[cfg(all(feature = "rpc", feature = "test-util"))]
    #[doc(hidden)]
    pub fn __set_for_rpc(&mut self, yes: bool) {
        self.set_for_rpc(yes)
    }

    /// `true` if the kernel driver's object table backs this parcel —
    /// binders travel as `flat_binder_object`, FDs as `BINDER_TYPE_FD`,
    /// and a transaction on it carries a caller identity the kernel
    /// vouched for.
    ///
    /// `false` exactly for an RPC-mode parcel, whose binders are
    /// `RpcAddress` values marshalled through the attached session — or
    /// refused outright where there is no session, as in the data-only
    /// mode behind `to_bytes`.
    /// Kernel marshalling is the default, so a parcel that has not
    /// travelled yet — a freshly constructed [`Parcel::new`], say —
    /// reports `true` as well: this answers *which marshalling the
    /// parcel uses*, not whether a live transaction vouched for its
    /// contents. A security branch needs the second question too, and
    /// must ask it separately with [`crate::is_handling_transaction`] —
    /// see [`crate::permission_controller::check_permission`].
    pub fn is_kernel_backed(&self) -> bool {
        self.rpc.is_none()
    }

    /// Whether a file descriptor written to this parcel can travel —
    /// AOSP `Parcel::allowFds()`.
    ///
    /// A kernel parcel always can. An RPC one can only over a session
    /// that negotiated
    #[cfg_attr(
        feature = "rpc",
        doc = "[`FileDescriptorTransportMode::Unix`](crate::rpc::FileDescriptorTransportMode),"
    )]
    #[cfg_attr(
        not(feature = "rpc"),
        doc = "`FileDescriptorTransportMode::Unix` (`rpc` feature),"
    )]
    /// which rules out vsock and TLS, and rules out the session-less
    /// data-only mode behind `to_bytes` as well.
    ///
    /// This is what a *writer* asks before choosing a representation, in
    /// the way [`write_blob`](Self::write_blob) picks an inline copy
    /// where it cannot hand over a shared-memory fd. It does not gate
    /// anything on its own: writing an fd into a parcel that answers
    /// `false` still fails with
    /// [`StatusCode::FdsNotAllowed`](crate::StatusCode::FdsNotAllowed)
    /// at the one place that enforces it.
    pub fn allow_fds(&self) -> bool {
        if self.is_kernel_backed() {
            return true;
        }
        #[cfg(feature = "rpc")]
        {
            self.rpc_fd_mode() == crate::rpc::FileDescriptorTransportMode::Unix
        }
        // Without `rpc` the only non-kernel parcel is data-only, with no session to carry an fd.
        #[cfg(not(feature = "rpc"))]
        false
    }

    /// `true` if this parcel serializes binders/FDs the RPC way.
    #[deprecated(
        since = "0.11.0",
        note = "renamed and inverted: use `!parcel.is_kernel_backed()`. The old name \
                described one consumer of the mode rather than what the flag controls, \
                and read fail-open at the one security branch that uses it."
    )]
    pub fn is_for_rpc(&self) -> bool {
        !self.is_kernel_backed()
    }

    /// Attach the RPC hooks and enter RPC mode (AOSP `Parcel::markForRpc`/`mSession`).
    #[cfg(feature = "rpc")]
    pub(crate) fn attach_rpc_ops(&mut self, ops: std::sync::Arc<dyn RpcParcelOps>) {
        self.rpc.get_or_insert_with(RpcFields::default).ops = Some(ops);
    }

    /// Enter RPC mode with the session profile: hooks, FD mode, FD-position recording.
    #[cfg(feature = "rpc")]
    pub(crate) fn configure_rpc(
        &mut self,
        ops: std::sync::Arc<dyn RpcParcelOps>,
        fd_mode: crate::rpc::FileDescriptorTransportMode,
        record_fd_positions: bool,
    ) {
        let rpc = self.rpc.get_or_insert_with(RpcFields::default);
        rpc.ops = Some(ops);
        rpc.fd_mode = fd_mode;
        rpc.record_fd_positions = record_fd_positions;
    }

    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_ops(&self) -> Option<std::sync::Arc<dyn RpcParcelOps>> {
        self.rpc.as_ref().and_then(|r| r.ops.clone())
    }

    /// The written byte buffer (for placing into an RPC wire body).
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_data_bytes(&self) -> &[u8] {
        self.data.as_slice()
    }

    // ---- RPC object table (android-16 v2) --------------------------

    /// Sorted object positions (AOSP `mObjectPositions`); empty if kernel-backed or objectless.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_object_positions(&self) -> &[u32] {
        self.rpc
            .as_ref()
            .map_or(&[], |r| r.object_positions.as_slice())
    }

    /// Install an incoming object table (AOSP `rpcSetDataReference`); no-op if kernel-backed.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_set_object_positions(&mut self, positions: Vec<u32>) {
        if let Some(rpc) = self.rpc.as_mut() {
            rpc.object_positions = positions;
        }
    }

    /// Record a `timesSent` bump for `addr`, settled by `Drop` unless it was sent; kernel: no-op.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_record_leaving_addr(&mut self, addr: crate::rpc::RpcAddress) {
        if let Some(rpc) = self.rpc.as_mut() {
            rpc.leaving_addrs.push(addr);
        }
    }

    /// The session whose ops this parcel carries ([`RpcParcelOps::session_id`]); `None` if none.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_session_id(&self) -> Option<*const ()> {
        self.rpc.as_ref()?.ops.as_ref().map(|ops| ops.session_id())
    }

    /// Whether a binder may still be written for sending: kernel parcels always, RPC `NotSent`.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_is_unsent(&self) -> bool {
        self.rpc
            .as_ref()
            .is_none_or(|r| r.send_state() == RpcSendState::NotSent)
    }

    /// Mark a parcel built from a peer's bytes (AOSP `rpcSetDataReference`), so it is not resent.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_mark_received(&mut self) {
        let len = self.data.len();
        if let Some(rpc) = self.rpc.as_mut() {
            rpc.send_state.store(
                RpcSendState::Received as u8,
                std::sync::atomic::Ordering::Release,
            );
            rpc.received_len = len;
        }
    }

    /// Whether a binder at `pos` arrived with this parcel, so a read enters it (module doc).
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_received_at(&self, pos: usize) -> bool {
        self.rpc
            .as_ref()
            .is_some_and(|r| r.send_state() == RpcSendState::Received && pos < r.received_len)
    }

    /// What object position `pos` entered on an earlier read (AOSP `mAcquiredEnteringBinders`).
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_entered_at(
        &self,
        pos: usize,
    ) -> Option<(crate::rpc::RpcAddress, crate::binder::SIBinder)> {
        let rpc = self.rpc.as_ref()?;
        let pos = u32::try_from(pos).ok()?;
        let at = rpc.entered.binary_search_by_key(&pos, |e| e.0).ok()?;
        let (_, addr, binder) = &rpc.entered[at];
        Some((*addr, binder.clone()))
    }

    /// Record that position `pos` entered `binder` at `addr`; the parcel holds it until it drops.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_record_entered(
        &mut self,
        pos: usize,
        addr: crate::rpc::RpcAddress,
        binder: crate::binder::SIBinder,
    ) {
        let (Some(rpc), Ok(pos)) = (self.rpc.as_mut(), u32::try_from(pos)) else {
            return;
        };
        match rpc.entered.binary_search_by_key(&pos, |e| e.0) {
            Ok(at) => rpc.entered[at] = (pos, addr, binder),
            Err(at) => rpc.entered.insert(at, (pos, addr, binder)),
        }
    }

    /// Claim this parcel for one send; `InvalidOperation` unless `NotSent`; kernel: always `Ok`.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_begin_send(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        let Some(rpc) = self.rpc.as_ref() else {
            return Ok(());
        };
        match rpc.send_state.compare_exchange(
            RpcSendState::NotSent as u8,
            RpcSendState::InFlight as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(state) => {
                let state = RpcSendState::from_u8(state);
                log::error!("RPC: a parcel is sent once; build a new request (state {state:?})");
                Err(StatusCode::InvalidOperation)
            }
        }
    }

    /// Release the claim: `sent` hands `leaving_addrs` to the peer, `!sent` returns to `NotSent`.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_end_send(&self, sent: bool) {
        let Some(rpc) = self.rpc.as_ref() else {
            return;
        };
        let next = if sent {
            RpcSendState::Sent
        } else {
            RpcSendState::NotSent
        };
        rpc.send_state
            .store(next as u8, std::sync::atomic::Ordering::Release);
    }

    /// Keep `binder` alive for this parcel's lifetime (see `RpcFields::pinned`).
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_pin_binder(&mut self, binder: crate::binder::SIBinder) {
        if let Some(rpc) = self.rpc.as_mut() {
            rpc.pinned.push(binder);
        }
    }

    /// AOSP `unflattenBinder` v2: a binder is read only at a listed position (module doc).
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_object_position_present(&self, pos: usize) -> bool {
        self.rpc
            .as_ref()
            .is_some_and(|r| r.object_position_present(pos))
    }

    /// Record a flattened RPC object's offset; no-op if kernel-backed (module doc "RPC fields").
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_record_object_position(&mut self, pos: usize) {
        if let Some(rpc) = self.rpc.as_mut() {
            rpc.record_object_position(pos);
        }
    }

    // ---- FD-over-RPC (opt-in, Unix mode) ---------------------------

    /// Test/fuzz setter for the FD mode; production uses `configure_rpc`.
    #[cfg(all(feature = "rpc", any(test, feature = "fuzzing")))]
    pub(crate) fn set_rpc_fd_mode(&mut self, mode: crate::rpc::FileDescriptorTransportMode) {
        if let Some(rpc) = self.rpc.as_mut() {
            rpc.fd_mode = mode;
        }
    }

    /// The negotiated FD-over-RPC mode.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_fd_mode(&self) -> crate::rpc::FileDescriptorTransportMode {
        self.rpc.as_ref().map_or(Default::default(), |r| r.fd_mode)
    }

    /// Test/fuzz setter for FD-position recording; production uses `configure_rpc`.
    #[cfg(all(feature = "rpc", any(test, feature = "fuzzing")))]
    pub(crate) fn set_rpc_record_fd_positions(&mut self, yes: bool) {
        if let Some(rpc) = self.rpc.as_mut() {
            rpc.record_fd_positions = yes;
        }
    }

    /// Whether the FD-write path records its object position.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_record_fd_positions(&self) -> bool {
        self.rpc.as_ref().is_some_and(|r| r.record_fd_positions)
    }

    /// Stash an outgoing fd and return its index (`ParcelFileDescriptor`, `Unix` fd-mode).
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_push_out_fd(&mut self, fd: std::os::fd::OwnedFd) -> i32 {
        // Callers are RPC-only: a kernel parcel here is a bug; a panic beats a bogus index.
        self.rpc
            .as_mut()
            .expect("rpc_push_out_fd on kernel parcel")
            .push_out_fd(fd)
    }

    /// Outgoing fds for `SCM_RIGHTS`; the parcel keeps ownership and closes them on drop.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_out_fds(&self) -> &[std::os::fd::OwnedFd] {
        self.rpc.as_ref().map_or(&[], |r| r.fds_out.as_slice())
    }

    /// Install the fds received out-of-band, before deserialization.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_set_in_fds(&mut self, fds: Vec<std::os::fd::OwnedFd>) {
        if let Some(rpc) = self.rpc.as_mut() {
            rpc.set_in_fds(fds);
        }
    }

    /// Take the received fd at table `index` (consumed once).
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_take_in_fd(&mut self, index: usize) -> Option<std::os::fd::OwnedFd> {
        self.rpc.as_mut().and_then(|r| r.take_in_fd(index))
    }

    /// Shrink only; the one grow over initialized bytes is `set_data_size_driver_filled`.
    pub(crate) fn set_data_size(&mut self, new_len: usize) -> Result<()> {
        if new_len > self.data.len() {
            log::error!(
                "set_data_size({new_len}) would grow past the initialized length {}",
                self.data.len()
            );
            return Err(StatusCode::BadValue);
        }
        // SAFETY: shrinking only, so `0..new_len` is an initialized prefix within capacity.
        unsafe { self.data.set_len(new_len) };
        if new_len < self.pos {
            self.pos = new_len;
        }
        Ok(())
    }

    /// # Safety: bytes `0..new_len` initialized (the driver's `read_consumed`); see module doc.
    pub(crate) unsafe fn set_data_size_driver_filled(&mut self, new_len: usize) -> Result<()> {
        if new_len > self.data.capacity() {
            // A driver claiming more bytes than the buffer holds broke the contract.
            log::error!(
                "set_data_size_driver_filled({new_len}) exceeds capacity {}",
                self.data.capacity()
            );
            return Err(StatusCode::BadValue);
        }
        // SAFETY: within capacity (checked above); the caller vouches `0..new_len` is initialized.
        unsafe { self.data.set_len(new_len) };
        if new_len < self.pos {
            self.pos = new_len;
        }
        Ok(())
    }

    pub(crate) fn close_file_descriptors(&self) {
        // RPC-mode parcels never carry kernel FD objects; nothing to close here.
        if self.rpc.is_some() {
            return;
        }

        for offset in self.objects.as_slice() {
            let Ok(obj) = read_flat_binder(self.data.as_slice(), *offset as usize) else {
                log::error!("Parcel: unable to read object at offset {offset}");
                continue;
            };
            if obj.header_type() == BINDER_TYPE_FD {
                // Close the file descriptor
                obj.owned_fd();
            }
        }
    }

    /// Move the read/write cursor. AOSP `Parcel::setDataPosition`.
    ///
    /// `pos` may legitimately sit past the end of the written bytes —
    /// the write paths zero-fill the `[len..pos]` gap when they next
    /// grow the buffer. A `pos` above `i32::MAX` is refused (and the
    /// cursor left unchanged) rather than accepted: AOSP
    /// `LOG_ALWAYS_FATAL`s there to catch a negative `int` that was
    /// converted to `size_t`, and every wire length is an `i32`.
    pub fn set_data_position(&mut self, pos: usize) {
        if pos > i32::MAX as usize {
            log::error!("Parcel::set_data_position({pos}) exceeds i32::MAX; ignored");
            return;
        }
        self.pos = pos;
    }

    pub fn data_position(&self) -> usize {
        self.pos
    }

    pub fn data_size(&self) -> usize {
        if self.data.len() > self.pos {
            self.data.len()
        } else {
            self.pos
        }
    }

    /// Read a type that implements [`Deserialize`] from the sub-parcel.
    pub fn read<D: Deserialize>(&mut self) -> Result<D> {
        D::deserialize(self)
    }

    /// Attempt to read a type that implements [`Deserialize`] from this parcel
    /// onto an existing value. This operation will overwrite the old value
    /// partially or completely, depending on how much data is available.
    pub fn read_onto<D: Deserialize>(&mut self, x: &mut D) -> Result<()> {
        x.deserialize_from(self)
    }

    // Thin by-value wrappers over the generic read/write; wire-identical, names mirror AOSP.

    /// Write an `i32` (AOSP `writeInt32`).
    pub fn write_i32(&mut self, val: i32) -> Result<()> {
        self.write(&val)
    }
    /// Write a `u32` (AOSP `writeUint32`).
    pub fn write_u32(&mut self, val: u32) -> Result<()> {
        self.write(&val)
    }
    /// Write an `i64` (AOSP `writeInt64`).
    pub fn write_i64(&mut self, val: i64) -> Result<()> {
        self.write(&val)
    }
    /// Write a `u64` (AOSP `writeUint64`).
    pub fn write_u64(&mut self, val: u64) -> Result<()> {
        self.write(&val)
    }
    /// Write an `f32` (AOSP `writeFloat`).
    pub fn write_f32(&mut self, val: f32) -> Result<()> {
        self.write(&val)
    }
    /// Write an `f64` (AOSP `writeDouble`).
    pub fn write_f64(&mut self, val: f64) -> Result<()> {
        self.write(&val)
    }
    /// Write a `bool` as an `i32` (AOSP `writeBool`).
    pub fn write_bool(&mut self, val: bool) -> Result<()> {
        self.write(&val)
    }
    /// Write an `i8`, widened to a 4-byte word (AOSP `writeByte`).
    pub fn write_i8(&mut self, val: i8) -> Result<()> {
        self.write(&val)
    }
    /// Write a `u8`, widened to a 4-byte word.
    pub fn write_u8(&mut self, val: u8) -> Result<()> {
        self.write(&val)
    }

    /// Read an `i32` (AOSP `readInt32`).
    pub fn read_i32(&mut self) -> Result<i32> {
        self.read()
    }
    /// Read a `u32` (AOSP `readUint32`).
    pub fn read_u32(&mut self) -> Result<u32> {
        self.read()
    }
    /// Read an `i64` (AOSP `readInt64`).
    pub fn read_i64(&mut self) -> Result<i64> {
        self.read()
    }
    /// Read a `u64` (AOSP `readUint64`).
    pub fn read_u64(&mut self) -> Result<u64> {
        self.read()
    }
    /// Read an `f32` (AOSP `readFloat`).
    pub fn read_f32(&mut self) -> Result<f32> {
        self.read()
    }
    /// Read an `f64` (AOSP `readDouble`).
    pub fn read_f64(&mut self) -> Result<f64> {
        self.read()
    }
    /// Read a `bool` (AOSP `readBool`).
    pub fn read_bool(&mut self) -> Result<bool> {
        self.read()
    }
    /// Read an `i8` (AOSP `readByte`).
    pub fn read_i8(&mut self) -> Result<i8> {
        self.read()
    }
    /// Read a `u8`.
    pub fn read_u8(&mut self) -> Result<u8> {
        self.read()
    }

    pub fn data_avail(&self) -> usize {
        // `pos` may sit past `len` after a seek, so saturate: nothing is available there.
        let result = self.data.len().saturating_sub(self.pos);
        assert!(result < i32::MAX as _, "data too big: {result}");

        result
    }

    pub(crate) fn read_aligned_data(&mut self, len: usize) -> Result<&[u8]> {
        let aligned = pad_size(len);
        let pos = self.pos;

        if aligned <= self.data_avail() {
            self.pos = pos + aligned;
            Ok(&self.data.as_slice()[pos..pos + len])
        } else {
            log::error!(
                "Not enough data to read aligned data.: {aligned} <= {}",
                self.data_avail()
            );
            Err(StatusCode::NotEnoughData)
        }
    }

    pub(crate) fn read_object(&mut self, null_meta: bool) -> Result<flat_binder_object> {
        // RPC parcels carry no `flat_binder_object`: an object read here is a protocol error.
        if self.rpc.is_some() {
            return Err(StatusCode::BadType);
        }

        let data_pos = self.pos as u64;
        let size = std::mem::size_of::<flat_binder_object>();

        let obj = read_flat_binder(self.read_aligned_data(size)?, 0)?;

        if !null_meta && obj.cookie == 0 && obj.pointer() == 0 {
            return Ok(obj);
        }

        let objects = self.objects.as_slice();
        let count = objects.len();
        let mut opos = self.next_object_hint;

        if count > 0 {
            log::trace!("Parcel looking for obj at {data_pos}, hint={opos}");
            if opos < count {
                while opos < (count - 1) && objects[opos] < data_pos {
                    opos += 1;
                }
            } else {
                opos = count - 1;
            }
            if objects[opos] == data_pos {
                self.next_object_hint = opos + 1;
                return Ok(obj);
            }

            while opos > 0 && objects[opos] > data_pos {
                opos -= 1;
            }

            if objects[opos] == data_pos {
                self.next_object_hint = opos + 1;
                return Ok(obj);
            }
        }
        log::error!("Parcel: unable to find object at index {data_pos}");
        Err(StatusCode::BadType)
    }

    /// Safely read a sized parcelable.
    ///
    /// Read the size of a parcelable, compute the end position
    /// of that parcelable, then build a sized readable sub-parcel
    /// and call a closure with the sub-parcel as its parameter.
    /// The closure can keep reading data from the sub-parcel
    /// until it runs out of input data.
    /// After the closure returns, skip to the end of the current
    /// parcelable regardless of how much the closure has read.
    ///
    /// A self-referential parcelable (e.g. AIDL `RecursiveList`) recurses
    /// through this method as it reads each `next` node, so a hostile
    /// deeply-nested payload would recurse until the worker-thread stack
    /// overflows — a hard `SIGABRT`, not a recoverable [`StatusCode`]. The
    /// nesting is capped at `MAX_NESTED_READ_DEPTH` (1000); a payload exceeding it
    /// is rejected with [`StatusCode::BadValue`]. This is defense-in-depth
    /// beyond AOSP (whose `Parcel` has no equivalent guard) and is set far
    /// above any legitimate AIDL nesting, so conforming traffic is unaffected.
    pub fn sized_read<F>(&mut self, f: F) -> Result<()>
    where
        for<'b> F: FnOnce(&mut Parcel) -> Result<()>,
    {
        let start = self.data_position();
        let parcelable_size: i32 = self.read()?;
        if parcelable_size < 4 {
            log::error!("Parcel: bad size for object: {parcelable_size}");
            return Err(StatusCode::BadValue);
        }

        let end = start.checked_add(parcelable_size as _).ok_or_else(|| {
            log::error!("Parcel: check_add error: {parcelable_size}");
            StatusCode::BadValue
        })?;
        if end > self.data_size() {
            log::error!("Parcel: not enough data: {} > {}", end, self.data_size());
            return Err(StatusCode::NotEnoughData);
        }

        if self.nested_read_depth >= MAX_NESTED_READ_DEPTH {
            log::error!("Parcel: nested parcelable read depth exceeded {MAX_NESTED_READ_DEPTH}");
            return Err(StatusCode::BadValue);
        }
        self.nested_read_depth += 1;

        // Bound `has_more_data()` to this block for the closure; restored after, for nesting.
        let prev_boundary = self.read_boundary;
        self.read_boundary = Some(end);
        let result = f(self);
        self.read_boundary = prev_boundary;
        self.nested_read_depth -= 1;
        result?;

        // Skip to the block end even if the closure read less.
        self.set_data_position(end);

        Ok(())
    }

    /// Whether the read cursor has more data *within the current
    /// [`Parcel::sized_read`] block* (or the whole buffer when not inside
    /// one). Generated `read_from_parcel` guards each field read with this
    /// so a version-N reader cleanly leaves trailing fields at their default
    /// when reading a version-M (< N) peer's shorter parcelable — the
    /// stable-AIDL forward-compatibility contract. Mirrors AOSP
    /// `Parcel::hasMoreData()`.
    pub fn has_more_data(&self) -> bool {
        let end = self.read_boundary.unwrap_or_else(|| self.data_size());
        self.pos < end
    }

    pub(crate) fn read_array<D: Deserialize + ParcelPod + WireScalar>(
        &mut self,
    ) -> Result<Option<Vec<D>>> {
        let len: i32 = self.read()?;
        if len < -1 {
            log::error!("Parcel: bad array length: {len}");
            return Err(StatusCode::UnexpectedNull);
        }
        if len == -1 {
            return Ok(None);
        }
        if len == 0 {
            return Ok(Some(Vec::new()));
        }

        // Checked so a hostile `len` cannot wrap `size` on 32-bit; see `checked_array_layout`.
        let (size, padded) = checked_array_layout(len, std::mem::size_of::<D>())?;

        if padded > self.data_avail() {
            log::error!(
                "Parcel: not enough data to read array: {} > {}",
                padded,
                self.data_avail()
            );
            return Err(StatusCode::NotEnoughData);
        }

        let pos = self.pos;

        // Safer approach: bounds-checked access using slice
        let data_slice = self
            .data
            .as_slice()
            .get(pos..pos + size)
            .ok_or(StatusCode::NotEnoughData)?;

        let mut result = Vec::with_capacity(len as usize);
        // SAFETY: `data_slice` is `size` = `len` `D`s; `result` has capacity `len`; `D: ParcelPod`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data_slice.as_ptr(),
                result.as_mut_ptr() as *mut u8,
                size,
            );
            result.set_len(len as usize);

            // On LE hosts the memcpy is the decode; BE hosts reverse each element (f32/f64 too).
            if cfg!(target_endian = "big") && std::mem::size_of::<D>() > 1 {
                let bytes = std::slice::from_raw_parts_mut(result.as_mut_ptr() as *mut u8, size);
                for chunk in bytes.chunks_exact_mut(std::mem::size_of::<D>()) {
                    chunk.reverse();
                }
            }
        }

        self.set_data_position(pos + padded);

        Ok(Some(result))
    }

    pub(crate) fn read_array_char<D: CharType>(
        &mut self,
    ) -> Result<Option<Vec<<D as CharType>::Output>>> {
        let len: i32 = self.read()?;
        if len < -1 {
            log::error!("Parcel: bad array length: {len}");
            return Err(StatusCode::UnexpectedNull);
        }
        if len == -1 {
            return Ok(None);
        }
        if len == 0 {
            return Ok(Some(Vec::new()));
        }

        // Checked as in `read_array`; a char-array element is always 4 wire bytes.
        let (size, padded) = checked_array_layout(len, std::mem::size_of::<i32>())?;

        if padded > self.data_avail() {
            log::error!(
                "Parcel: not enough data to read array char: {} > {}",
                padded,
                self.data_avail()
            );
            return Err(StatusCode::NotEnoughData);
        }

        let pos = self.pos;
        // The buffer is only 1-byte aligned, so copy by value; `align_to` would drop a prefix.
        let result = self.data.as_slice()[pos..pos + size]
            .chunks_exact(std::mem::size_of::<i32>())
            .map(|c| D::from(&i32::from_le_bytes([c[0], c[1], c[2], c[3]])))
            .collect();

        self.set_data_position(pos + padded);

        Ok(Some(result))
    }

    /// Read a vector size from the parcel and resize the given output vector to
    /// be correctly sized for that amount of data.
    ///
    /// This method is used in AIDL-generated server side code for methods that
    /// take a mutable slice reference parameter.
    pub fn resize_out_vec<D: Default + Deserialize>(&mut self, out_vec: &mut Vec<D>) -> Result<()> {
        let len: i32 = self.read()?;

        if len < 0 {
            return Err(StatusCode::UnexpectedNull);
        }

        // usize in Rust may be 16-bit, so i32 may not fit
        let len = len.try_into().or(Err(StatusCode::BadValue))?;
        check_out_vec_size::<D>(len)?;
        out_vec.resize_with(len, Default::default);

        Ok(())
    }

    /// Read a vector size from the parcel and either create a correctly sized
    /// vector for that amount of data or set the output parameter to None if
    /// the vector should be null.
    ///
    /// This method is used in AIDL-generated server side code for methods that
    /// take a mutable slice reference parameter.
    pub fn resize_nullable_out_vec<D: Default + Deserialize>(
        &mut self,
        out_vec: &mut Option<Vec<D>>,
    ) -> Result<()> {
        let len: i32 = self.read()?;

        if len < 0 {
            *out_vec = None;
        } else {
            // usize in Rust may be 16-bit, so i32 may not fit
            let len = len.try_into().or(Err(StatusCode::BadValue))?;
            check_out_vec_size::<D>(len)?;
            let mut vec = Vec::with_capacity(len);
            vec.resize_with(len, Default::default);
            *out_vec = Some(vec);
        }

        Ok(())
    }

    pub(crate) fn update_work_source_request_header_pos(&mut self) {
        if !self.request_header_present {
            self.work_source_request_header_pos = self.data.len();
            self.request_header_present = true;
        }
    }

    pub fn write<S: Serialize + ?Sized>(&mut self, parcelable: &S) -> Result<()> {
        parcelable.serialize(self)
    }

    pub(crate) fn write_array<S: Serialize + ParcelPod + WireScalar>(
        &mut self,
        parcelable: &[S],
    ) -> Result<()> {
        let len = parcelable.len();
        // The length word is an `i32`: an oversized slice is `BadValue`, not a truncated count.
        let len_i32: i32 = len.try_into().or(Err(StatusCode::BadValue))?;
        self.write::<i32>(&len_i32)?;

        if len == 0 {
            return Ok(());
        }

        let size = std::mem::size_of_val(parcelable);
        let padded = pad_size(size);
        let pos = self.pos;

        // Bound `end` to `i32::MAX`; see module doc "Buffer growth".
        let end = pos
            .checked_add(padded)
            .filter(|&e| e <= i32::MAX as usize)
            .ok_or(StatusCode::BadValue)?;

        self.data.reserve(end.saturating_sub(self.data.len()));
        // SAFETY: `reserve` covers `pos..end`; gap, copy and pad writes initialize all of it.
        unsafe {
            // Zero a forward-seek gap before `set_len`; see module doc "Buffer growth".
            let old_len = self.data.len();
            if pos > old_len {
                std::ptr::write_bytes(self.data.as_mut_ptr().add(old_len), 0, pos - old_len);
            }
            std::ptr::copy_nonoverlapping::<u8>(
                parcelable.as_ptr() as _,
                self.data.as_mut_ptr().add(pos),
                size,
            );
            if padded > size {
                std::ptr::write_bytes(self.data.as_mut_ptr().add(pos + size), 0, padded - size);
            }
            if self.data.len() < end {
                self.data.set_len(end);
            }
        }

        // Big-endian hosts byte-reverse each element in place (covers f32/f64); LE compiles it out.
        if cfg!(target_endian = "big") && std::mem::size_of::<S>() > 1 {
            for chunk in
                self.data.as_mut_slice()[pos..pos + size].chunks_exact_mut(std::mem::size_of::<S>())
            {
                chunk.reverse();
            }
        }

        self.set_data_position(end);

        Ok(())
    }

    pub(crate) fn write_array_char<S: CharType>(&mut self, parcelable: &[S]) -> Result<()> {
        let len = parcelable.len();
        // Length word is an `i32`, and `4 * len` must not overflow `usize`.
        let len_i32: i32 = len.try_into().or(Err(StatusCode::BadValue))?;
        self.write::<i32>(&len_i32)?;

        let size = len.checked_mul(4).ok_or(StatusCode::BadValue)?;
        let padded = pad_size(size);

        // Bound `end` to `i32::MAX`; see module doc "Buffer growth".
        let end = self
            .pos
            .checked_add(padded)
            .filter(|&e| e <= i32::MAX as usize)
            .ok_or(StatusCode::BadValue)?;
        self.data.reserve(end.saturating_sub(self.data.len()));
        for c in parcelable {
            self.write(&c.as_i32())?;
        }

        Ok(())
    }

    /// Writes the length of a slice to the parcel.
    ///
    /// This is used in AIDL-generated client side code to indicate the
    /// allocated space for an output array parameter.
    ///
    /// Wire encoding (the convention shared by every array codec here): an
    /// array is a leading `i32` element count, where `-1` denotes a *null*
    /// array. The read side decodes this in [`resize_out_vec`](Self::resize_out_vec)
    /// (rejects `< 0` as `UnexpectedNull`) and
    /// [`resize_nullable_out_vec`](Self::resize_nullable_out_vec) (`-1` → `None`).
    pub fn write_slice_size<T>(&mut self, slice: Option<&[T]>) -> Result<()> {
        if let Some(slice) = slice {
            let len: i32 = slice.len().try_into().or(Err(StatusCode::BadValue))?;
            self.write(&len)
        } else {
            self.write(&-1i32)
        }
    }

    /// Writes an L1 wire scalar (LE), already widened to its wire type; see module doc.
    pub(crate) fn write_le<T: WireScalar>(&mut self, val: &T) -> Result<()> {
        self.write_aligned_data(val.to_wire().as_ref())
    }

    /// Reads an L1 wire scalar written by [`Parcel::write_le`].
    pub(crate) fn read_le<T: WireScalar>(&mut self) -> Result<T> {
        let data = self.read_aligned_data(std::mem::size_of::<T>())?;
        T::from_wire(data)
    }

    /// Writes a host-native L2 command-stream scalar (`BC_*`, handles, cookies); see module doc.
    pub(crate) fn write_native<T: NativeScalar>(&mut self, val: &T) -> Result<()> {
        self.write_aligned_data(val.to_native().as_ref())
    }

    /// Reads an L2 scalar written by the kernel driver. See [`Parcel::write_native`].
    pub(crate) fn read_native<T: NativeScalar>(&mut self) -> Result<T> {
        let data = self.read_aligned_data(std::mem::size_of::<T>())?;
        T::from_native(data)
    }

    pub(crate) fn write_aligned<T: ParcelPod>(&mut self, val: &T) -> Result<()> {
        let unaligned = std::mem::size_of::<T>();
        // SAFETY: `T: ParcelPod` has no padding, so every byte of the live `val` is initialized.
        let val_bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(val as *const T as *const u8, unaligned) };

        self.write_aligned_data(val_bytes)
    }

    pub(crate) fn write_aligned_data(&mut self, data: &[u8]) -> Result<()> {
        let unaligned = data.len();
        let aligned = pad_size(unaligned);
        let pos = self.pos;

        // Bound `end` to `i32::MAX` like AOSP `growData`; see module doc "Buffer growth".
        let end = pos
            .checked_add(aligned)
            .filter(|&e| e <= i32::MAX as usize)
            .ok_or(StatusCode::BadValue)?;

        self.data.reserve(end.saturating_sub(self.data.len()));
        // SAFETY: `reserve` covers `pos..end`; gap, copy and pad writes initialize all of it.
        unsafe {
            // Zero a forward-seek gap before `set_len`; see module doc "Buffer growth".
            let old_len = self.data.len();
            if pos > old_len {
                std::ptr::write_bytes(self.data.as_mut_ptr().add(old_len), 0, pos - old_len);
            }
            std::ptr::copy_nonoverlapping::<u8>(
                data.as_ptr(),
                self.data.as_mut_ptr().add(pos),
                unaligned,
            );
            if aligned > unaligned {
                std::ptr::write_bytes(
                    self.data.as_mut_ptr().add(pos + unaligned),
                    0,
                    aligned - unaligned,
                );
            }
            if end > self.data.len() {
                self.data.set_len(end);
            }
        }

        self.set_data_position(end);
        Ok(())
    }

    pub(crate) fn write_object(&mut self, obj: &flat_binder_object, null_meta: bool) -> Result<()> {
        self.write_object_pinned(obj, null_meta, None)
    }

    // `obj` is `binder` flattened: a HANDLE pins that `Arc` instead of looking the handle up.
    pub(crate) fn write_binder_object(
        &mut self,
        obj: &flat_binder_object,
        binder: &crate::binder::SIBinder,
    ) -> Result<()> {
        self.write_object_pinned(obj, false, Some(binder))
    }

    fn write_object_pinned(
        &mut self,
        obj: &flat_binder_object,
        null_meta: bool,
        binder: Option<&crate::binder::SIBinder>,
    ) -> Result<()> {
        // RPC mode: no offset-table entry and no kernel `acquire()`; RPC keeps its own refcount.
        if self.rpc.is_some() {
            self.write_aligned(obj)?;
            return Ok(());
        }

        let data_pos = self.pos;
        self.write_aligned(obj)?;

        if null_meta || obj.pointer() != 0 {
            // Pin first: `acquire` then hits the cached proxy instead of a temporary one.
            match binder.filter(|_| obj.header_type() == BINDER_TYPE_HANDLE) {
                Some(b) => {
                    debug_assert_eq!(b.as_proxy().map(|p| p.handle()), Some(obj.handle()));
                    self.kernel_pinned.push(b.clone());
                }
                None => self.pin_kernel_handle(obj)?,
            }
            obj.acquire()?;
            self.objects.push(data_pos as _);
        }

        Ok(())
    }

    // A proxy's strong ref must outlive the send; its own drop may queue BC_RELEASE before it.
    fn pin_kernel_handle(&mut self, obj: &flat_binder_object) -> Result<()> {
        if obj.header_type() == BINDER_TYPE_HANDLE {
            let proxy = crate::process_state::ProcessState::as_self()
                .strong_proxy_for_handle(obj.handle())?;
            self.kernel_pinned.push(proxy);
        }
        Ok(())
    }

    pub(crate) fn write_interface_token(&mut self, interface: &str) -> Result<()> {
        self.write(&(thread_state::get_strict_mode_policy() | STRICT_MODE_PENALTY_GATHER))?;
        self.update_work_source_request_header_pos();
        let work_source: i32 = if thread_state::should_propagate_work_source() {
            thread_state::get_calling_work_source_uid() as _
        } else {
            thread_state::UNSET_WORK_SOURCE
        };
        self.write(&work_source)?;
        if crate::sdk_at_least(30) {
            self.write(&binder::INTERFACE_HEADER)?;
        }
        self.write(&interface)?;

        Ok(())
    }

    /// Perform a series of writes to the parcel, prepended with the length
    /// (in bytes) of the written data.
    ///
    /// The length `0i32` will be written to the parcel first, followed by the
    /// writes performed by the callback. The initial length will then be
    /// updated to the length of all data written by the callback, plus the
    /// size of the length elemement itself (4 bytes).
    ///
    /// # Examples
    ///
    /// After the following call:
    ///
    /// ```
    /// # use rsbinder::{Binder, Interface, Parcel};
    /// # let mut parcel = Parcel::new();
    /// parcel.sized_write(|subparcel| {
    ///     subparcel.write(&1u32)?;
    ///     subparcel.write(&2u32)?;
    ///     subparcel.write(&3u32)
    /// });
    /// ```
    ///
    /// `parcel` will contain the following:
    ///
    /// ```ignore
    /// [16i32, 1u32, 2u32, 3u32]
    /// ```
    pub fn sized_write<F>(&mut self, f: F) -> Result<()>
    where
        for<'b> F: FnOnce(&mut Parcel) -> Result<()>,
    {
        let start = self.data_position();
        self.write(&0i32)?;
        {
            f(self)?;
        }
        let end = self.data_position();
        self.set_data_position(start);
        assert!(end >= start);
        self.write::<i32>(&((end - start) as _))?;
        self.set_data_position(end);
        Ok(())
    }

    pub(crate) fn append_all_from(&mut self, other: &mut Parcel) -> Result<()> {
        self.append_from(other, 0, other.data_size())
    }

    pub(crate) fn append_from(
        &mut self,
        other: &mut Parcel,
        offset: usize,
        size: usize,
    ) -> Result<()> {
        if size == 0 {
            return Ok(());
        }
        if size > i32::MAX as usize {
            log::error!("Parcel::append_from: the size is too large: {size}");
            return Err(StatusCode::BadValue);
        }
        // Bound by `data.len()`, not `data_size()`: a seek past the end would panic the index.
        let other_len = other.data.len();
        if offset > other_len || size > other_len || (offset + size) > other_len {
            log::error!("Parcel::append_from: The given offset({offset}) and size({size}) exceed the data range of the parcel.");
            return Err(StatusCode::BadValue);
        }

        // RPC bytes in a kernel parcel could forge handle 0; see module doc "append_from".
        if self.rpc.is_none() && other.rpc.is_some() {
            log::error!("Parcel::append_from: refusing RPC/data-only bytes into a kernel parcel");
            return Err(StatusCode::BadType);
        }

        // A session source may carry an `RpcAddress` in its body; see module doc "append_from".
        #[cfg(feature = "rpc")]
        if other.rpc.as_ref().is_some_and(|r| r.ops.is_some())
            && self.rpc.as_ref().is_none_or(|r| r.ops.is_none())
        {
            log::error!("Parcel::append_from: refusing session bytes into a session-less parcel");
            return Err(StatusCode::BadType);
        }

        // AOSP Parcel.cpp `appendFrom`: `isForRpc()` and `mSession` must match (module doc).
        #[cfg(feature = "rpc")]
        if let Some(ours) = self.rpc_session_id() {
            if other.rpc_session_id() != Some(ours) {
                log::error!(
                    "Parcel::append_from: a session parcel takes bytes from its own session only"
                );
                return Err(StatusCode::BadType);
            }
        }

        // Copied address bytes carry no bump: the source must own none, or two parcels settle one.
        #[cfg(feature = "rpc")]
        if other
            .rpc
            .as_ref()
            .is_some_and(|r| !r.leaving_addrs.is_empty())
        {
            log::error!(
                "Parcel::append_from: the source still owns binder reservations; append a \
                 received or binder-free parcel"
            );
            return Err(StatusCode::BadType);
        }

        // AOSP Parcel.cpp `appendFrom`: "Can only build a Parcel when preparing to send it".
        #[cfg(feature = "rpc")]
        if self
            .rpc
            .as_ref()
            .is_some_and(|r| r.send_state() != RpcSendState::NotSent)
        {
            log::error!("Parcel::append_from: the destination is being sent, sent or received");
            return Err(StatusCode::BadType);
        }

        // Bound `end` to `i32::MAX` as `write_aligned_data` does, rather than overflow the reserve.
        let end = self
            .pos
            .checked_add(size)
            .filter(|&e| e <= i32::MAX as usize)
            .ok_or(StatusCode::BadValue)?;

        // Every fallible RPC step runs here, before a byte moves; see module doc "append_from".
        #[cfg(feature = "rpc")]
        let staged = match self.rpc.as_ref().and_then(|r| r.ops.clone()) {
            Some(ops) => Some(self.stage_rpc_copy(&*ops, other, offset, size)?),
            None => None,
        };

        let start_pos = self.pos;
        let mut first_idx: i32 = -1;
        let mut last_idx: i32 = -2;
        {
            let object_size = std::mem::size_of::<flat_binder_object>() as u64;
            // Scan the source's table as AOSP `appendFrom` does; the destination's may be empty.
            let objects = other.objects.as_slice();

            for (i, &off) in objects.iter().enumerate() {
                if off >= offset as _ && (off + object_size) <= (offset + size) as u64 {
                    if first_idx == -1 {
                        first_idx = i as i32;
                    }
                    last_idx = i as i32;
                }
            }
        }

        let num_objects = last_idx - first_idx + 1;

        // An RPC destination has no object table: a copied object would pass as plain data.
        if self.rpc.is_some() && num_objects > 0 {
            let src_data = other.data.as_slice();
            let src_objects = other.objects.as_slice();
            let only_fds = (first_idx..=last_idx).all(|i| {
                matches!(
                    read_flat_binder(src_data, src_objects[i as usize] as usize),
                    Ok(flat) if flat.header_type() == BINDER_TYPE_FD
                )
            });
            return Err(if only_fds {
                StatusCode::FdsNotAllowed
            } else {
                StatusCode::BadType
            });
        }

        self.data.reserve(end.saturating_sub(self.data.len()));
        // SAFETY: `reserve` covers `..end`, all initialized below; source is a checked slice.
        unsafe {
            // Zero a forward-seek gap before `set_len`; see module doc "Buffer growth".
            let old_len = self.data.len();
            if self.pos > old_len {
                std::ptr::write_bytes(self.data.as_mut_ptr().add(old_len), 0, self.pos - old_len);
            }
            std::ptr::copy_nonoverlapping::<u8>(
                other.data.as_slice()[offset..offset + size].as_ptr(),
                self.data.as_mut_ptr().add(self.pos),
                size,
            );
            if end > self.data.len() {
                self.data.set_len(end);
            }
        }
        self.set_data_position(end);

        #[cfg(feature = "rpc")]
        if let (Some(staged), Some(rpc)) = (staged, self.rpc.as_mut()) {
            for rel in staged.positions {
                rpc.record_object_position(start_pos + rel);
            }
            // The index names a slot of this parcel's own table (module doc "append_from").
            for (rel, fd) in staged.fds {
                let at = start_pos + rel;
                let idx = rpc.push_out_fd(fd);
                self.data.as_mut_slice()[at..at + 4].copy_from_slice(&idx.to_le_bytes());
            }
            rpc.leaving_addrs.extend(staged.binders.leaving);
            rpc.pinned.extend(staged.binders.pinned);
        }

        // In RPC mode `num_objects > 0` returned above, so nothing is left to relocate.
        let skip_objects = self.rpc.is_some();

        if num_objects > 0 && !skip_objects {
            self.objects.reserve(num_objects as usize);

            // Push an offset only after acquire and FD dup succeed; see module doc "append_from".
            let src_objects = other.objects.as_slice();
            for i in first_idx..=last_idx {
                let off = src_objects[i as usize] as usize - offset + start_pos;
                let mut flat = read_flat_binder(self.data.as_slice(), off)?;
                self.pin_kernel_handle(&flat)?;
                flat.acquire()?;
                if flat.header_type() == BINDER_TYPE_FD {
                    let newfd = match rustix::io::fcntl_dupfd_cloexec(flat.borrowed_fd(), 0) {
                        Ok(newfd) => newfd,
                        Err(e) => {
                            // FD `acquire()` is a no-op and `off` is uncommitted: nothing to undo.
                            return Err(std::io::Error::from(e).into());
                        }
                    };
                    flat.set_handle(newfd.into_raw_fd() as _);
                    flat.set_cookie(1);
                    write_flat_binder(self.data.as_mut_slice(), off, &flat)?;
                }
                self.objects.push(off as _);
            }
        }

        Ok(())
    }

    /// AOSP `appendFrom` RPC arm up to the copy, for a session destination (module doc).
    #[cfg(feature = "rpc")]
    fn stage_rpc_copy(
        &self,
        ops: &dyn RpcParcelOps,
        other: &Parcel,
        offset: usize,
        size: usize,
    ) -> Result<StagedRpcCopy> {
        // Without positions a copied binder cannot be found, so nothing proves a range binder-free.
        if !ops.records_binder_positions()? {
            log::error!(
                "Parcel::append_from: this session's wire records no binder positions, so a \
                 copied binder could not take its reference; decode the payload and write it"
            );
            return Err(StatusCode::BadType);
        }
        let range = &other.data.as_slice()[offset..offset + size];
        let positions: Vec<usize> = other
            .rpc_object_positions()
            .iter()
            .map(|&p| p as usize)
            .filter(|&p| offset <= p && p < offset + size)
            .map(|p| p - offset)
            .collect();
        let mut binders: Vec<&[u8]> = Vec::new();
        let mut fds = Vec::new();
        for &rel in &positions {
            match le_i32_at(range, rel) {
                Some(RPC_TYPE_BINDER) => binders.push(&range[rel + 4..]),
                Some(crate::rpc::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR) => {
                    if self.rpc_fd_mode() != crate::rpc::FileDescriptorTransportMode::Unix {
                        return Err(StatusCode::FdsNotAllowed);
                    }
                    let fd = le_i32_at(range, rel + 4)
                        .and_then(|idx| usize::try_from(idx).ok())
                        .and_then(|idx| other.rpc.as_ref()?.source_fd(idx))
                        .ok_or(StatusCode::BadValue)?;
                    let dup = rustix::io::fcntl_dupfd_cloexec(fd, 0)
                        .map_err(|e| StatusCode::from(std::io::Error::from(e)))?;
                    fds.push((rel + 4, dup));
                }
                Some(_) => {
                    log::error!("Parcel::append_from: an RPC object that is neither binder nor fd");
                    return Err(StatusCode::InvalidOperation);
                }
                None => return Err(StatusCode::BadValue),
            }
        }
        let binders = if binders.is_empty() {
            CopiedBinders::default()
        } else {
            ops.acquire_copied(&binders)?
        };
        Ok(StagedRpcCopy {
            positions,
            fds,
            binders,
        })
    }

    /// `[offset, offset + size)` as its own parcel to read, with the objects in it (module doc).
    pub(crate) fn sub_parcel(&mut self, offset: usize, size: usize) -> Result<Parcel> {
        if self.rpc.is_none() {
            let mut sub = Parcel::new();
            sub.append_from(self, offset, size)?;
            return Ok(sub);
        }
        let end = offset
            .checked_add(size)
            .filter(|&e| e <= self.data.len() && size <= i32::MAX as usize)
            .ok_or_else(|| {
                log::error!("Parcel::sub_parcel: {offset} + {size} exceeds the parcel");
                StatusCode::BadValue
            })?;
        let mut sub = Parcel::from_vec(self.data.as_slice()[offset..end].to_vec());
        sub.set_for_rpc(true);
        #[cfg(feature = "rpc")]
        self.carry_rpc_objects(&mut sub, offset, end);
        Ok(sub)
    }

    /// Give `sub` this parcel's session profile and the objects in `[offset, end)`, shifted.
    #[cfg(feature = "rpc")]
    fn carry_rpc_objects(&mut self, sub: &mut Parcel, offset: usize, end: usize) {
        let (Some(src), Some(dst)) = (self.rpc.as_mut(), sub.rpc.as_mut()) else {
            return;
        };
        dst.ops = src.ops.clone();
        dst.fd_mode = src.fd_mode;
        dst.record_fd_positions = src.record_fd_positions;
        // A payload that arrived is entered as it arrived; module doc "`append_from`".
        if src.send_state() == RpcSendState::Received {
            dst.send_state.store(
                RpcSendState::Received as u8,
                std::sync::atomic::Ordering::Release,
            );
            dst.received_len = end - offset;
        }
        // Cloned, not moved: the outer parcel's own reads of these positions must still hit.
        for (pos, addr, binder) in &src.entered {
            let pos = *pos as usize;
            if offset <= pos && pos < end {
                dst.entered
                    .push(((pos - offset) as u32, *addr, binder.clone()));
            }
        }
        let data = sub.data.as_mut_slice();
        use crate::rpc::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR;
        // Source order kept: a forged unsorted table must miss the same lookups here as there.
        for i in 0..src.object_positions.len() {
            let pos = src.object_positions[i] as usize;
            if pos < offset || pos >= end {
                continue;
            }
            let rel = pos - offset;
            dst.object_positions.push(rel as u32);
            let is_fd = src.record_fd_positions
                && le_i32_at(data, rel) == Some(TYPE_NATIVE_FILE_DESCRIPTOR);
            let Some(idx) = le_i32_at(data, rel + 4).filter(|_| is_fd) else {
                continue;
            };
            // Moved, not dup'd: the outer parcel skips this range, and each fd is taken once.
            let fd = usize::try_from(idx).ok().and_then(|i| src.take_in_fd(i));
            let new_idx = dst.fds_in.len() as i32;
            dst.fds_in.push(fd);
            data[rel + 4..rel + 8].copy_from_slice(&new_idx.to_le_bytes());
        }
    }

    fn release_objects(&self) {
        // RPC objects live by DecStrong; kernel `release()` must never run on them.
        if self.rpc.is_some() {
            return;
        }

        if self.objects.len() == 0 {
            return;
        }

        for pos in self.objects.as_slice() {
            let Ok(obj) = read_flat_binder(self.data.as_slice(), *pos as usize) else {
                log::error!("Parcel: unable to read object at position {pos}");
                continue;
            };
            obj.release()
                .map_err(|e| log::error!("Parcel: unable to release object: {e:?}"))
                .ok();
        }
    }
}

impl Drop for Parcel {
    fn drop(&mut self) {
        match self.free_buffer {
            Some(free_buffer) => {
                // No panic in Drop: a double panic during unwind aborts the process; leak instead.
                if let Err(e) = free_buffer(
                    Some(self),
                    self.data.as_ptr() as _,
                    self.data.len(),
                    self.objects.as_ptr() as _,
                    self.objects.len(),
                ) {
                    log::error!("Failed to free parcel buffer ({e}); leaking the kernel buffer");
                }
            }
            None => {
                #[cfg(feature = "rpc")]
                if let Some(rpc) = self.rpc.as_mut() {
                    rpc.settle_unsent();
                }
                self.release_objects();
            }
        }
    }
}

impl std::fmt::Debug for Parcel {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        writeln!(f, "Parcel: pos {}, len {}", self.pos, self.data.len())?;
        if self.objects.len() > 0 {
            // SAFETY: `objects`, a `Vec` or an adopted IPC slice, is readable as bytes for `&self`.
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    self.objects.as_ptr() as *const u8,
                    self.objects.len() * std::mem::size_of::<binder_size_t>(),
                )
            };
            writeln!(
                f,
                "Object count {}\n{}",
                self.objects.len(),
                pretty_hex(&bytes)
            )?;
        }
        write!(f, "{}", pretty_hex(&self.data.as_slice()))
    }
}

impl<const N: usize> TryFrom<&mut Parcel> for [u8; N] {
    type Error = StatusCode;

    fn try_from(parcel: &mut Parcel) -> Result<Self> {
        let data = parcel.read_aligned_data(N)?;
        Ok(<[u8; N] as TryFrom<&[u8]>>::try_from(data)?)
    }
}

/// Encodes one value to bytes, using the same codec the IPC paths use.
///
/// Anything that is `Serialize` works, but a type you intend to store
/// should be a parcelable — `#[derive(rsbinder::Parcelable)]` or a type
/// generated from `.aidl`. That is where the forward-compatibility comes
/// from: a parcelable writes a length header, so a reader built against
/// an older definition stops at the boundary the writer wrote and a
/// field appended later is simply not read. `to_bytes(&42i32)` is four
/// bytes with no such header and no way to evolve.
///
/// ```no_run
/// # fn main() {}
/// # #[cfg(feature = "macros")]
/// # mod example {
/// # #[derive(rsbinder::Parcelable, Default, Debug, Clone, PartialEq)]
/// # struct Settings { volume: i32, name: String }
/// # fn run() -> rsbinder::Result<()> {
/// let settings = Settings { volume: 7, name: "quiet".into() };
/// std::fs::write("settings.bin", rsbinder::to_bytes(&settings)?)?;
/// # Ok(())
/// # }
/// # }
/// ```
///
/// # Errors
///
/// A binder or a file descriptor cannot be encoded — neither means
/// anything outside the process that produced it — so a value containing
/// one is refused rather than turned into bytes that would be a lie:
/// `FdsNotAllowed` for a file descriptor, matching what AOSP returns for
/// a session that permits none, and `BadType` for a binder, which has no
/// session here to be marshalled through. Beyond that, only what the
/// value's own `Serialize` reports — plus `BadValue` for a value too
/// large to address with an `i32` offset.
///
/// # Byte order
///
/// The wire is little-endian on every host, so for a value that carries
/// no object these bytes are portable across architectures and identical
/// to what an IPC peer would receive for the same value.
///
/// One shape escapes that. A *null* binder is 24 bytes of
/// `flat_binder_object` whose type word is written host-native, and it
/// gets no object-table entry — so the refusals above, which key on that
/// table, do not see it. It can only reach here inside a
/// [`crate::ParcelableHolder`] that copied its bytes in from a kernel
/// parcel. Bytes carrying one are not portable across endianness, and the
/// value they came from does not read back on any host: decoding the
/// payload returns `BadType`, since the decoder has no session to marshal
/// a binder through.
pub fn to_bytes<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    let mut parcel = Parcel::new_data_only();
    parcel.write(value)?;
    parcel.into_bytes()
}

/// Decodes one value from bytes written by [`to_bytes`].
///
/// # Errors
///
/// - `NotEnoughData` — the input ends inside the value.
/// - `BadValue` — the value decoded but bytes are left over. A partial
///   read is not success here: it usually means the bytes were written
///   as a different type, and returning the value anyway would hide
///   that. Also returned for an input of `i32::MAX` bytes or more, which
///   no parcel can address.
/// - `BadType` — the input claims to contain a binder. Such bytes are
///   never turned into an object; the decoder has no object table and
///   refuses outright.
/// - `FdsNotAllowed` — the input claims to contain a file descriptor.
///   The decoder has no session and therefore no negotiated fd mode,
///   which is the condition AOSP answers this way.
pub fn from_bytes<T: Deserialize>(bytes: &[u8]) -> Result<T> {
    // Parcel offsets are `i32`; past that the read path asserts.
    if bytes.len() >= i32::MAX as usize {
        return Err(StatusCode::BadValue);
    }
    let mut parcel = Parcel::from_slice(bytes);
    let value = parcel.read::<T>()?;
    if parcel.data_avail() != 0 {
        return Err(StatusCode::BadValue);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    #[should_panic(expected = "can't support as_mut_slice()")]
    fn parcel_data_slice_refuses_as_mut_slice() {
        // A `Slice` stands for the read-only kernel mapping: reads alias it, writes panic.
        let leaked: &'static mut [u8] = Box::leak(vec![0u8, 1, 2, 3].into_boxed_slice());
        let ptr = leaked.as_mut_ptr();
        let len = leaked.len();
        // SAFETY: `Box::leak` yields a valid, exclusive, `'static` buffer of `len` bytes.
        let mut pd: super::ParcelData<u8> =
            unsafe { super::ParcelData::from_raw_parts_mut(ptr, len) };

        assert_eq!(pd.as_slice(), &[0u8, 1, 2, 3][..]);
        assert_eq!(pd.as_ptr(), ptr as *const u8);
        let _ = pd.as_mut_slice();
    }

    #[test]
    fn write_array_zeroes_trailing_pad() {
        // The 1-3 pad bytes are transmitted (in `data_size`), so they must be zero, as in AOSP.
        let mut parcel = Parcel::new();
        // Poison with 0xFF so a missing zero-fill shows as leftover bytes, not incidental zeros.
        parcel.write(&(-1i32)).unwrap();
        parcel.write(&(-1i32)).unwrap();
        parcel.set_data_position(0);

        // 1-byte payload -> [i32 len=1][1 data byte][3 pad bytes].
        let payload: &[u8] = &[0xAB];
        SerializeArray::serialize_array(payload, &mut parcel).unwrap();

        let bytes = parcel.data.as_slice();
        assert_eq!(&bytes[4..8], &[0xAB, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn checked_array_layout_normal_case() {
        // 10 × 4-byte ints — exact pad alignment.
        let (size, padded) = super::checked_array_layout(10, 4).unwrap();
        assert_eq!((size, padded), (40, 40));
        // 3 × 5-byte elements — pad_size((3*5) + 3) & !3 = 16.
        let (size, padded) = super::checked_array_layout(3, 5).unwrap();
        assert_eq!((size, padded), (15, 16));
    }

    #[test]
    fn checked_array_layout_rejects_size_mul_overflow() {
        // `i32::MAX × usize::MAX` overflows on every target, so this also runs on 64-bit hosts.
        assert_eq!(
            super::checked_array_layout(i32::MAX, usize::MAX),
            Err(StatusCode::BadValue)
        );
    }

    #[test]
    fn checked_array_layout_rejects_pad_overflow() {
        // `size` lands at `usize::MAX`: the multiply passes and the `+ 3` pad step overflows.
        assert_eq!(
            super::checked_array_layout(1, usize::MAX),
            Err(StatusCode::BadValue)
        );
        // Same surface via the `len`-side product.
        assert_eq!(
            super::checked_array_layout(7, usize::MAX / 3),
            Err(StatusCode::BadValue)
        );
    }

    #[test]
    fn checked_array_layout_passes_i32_max_on_64_bit_and_rejects_it_on_32() {
        // `(i32::MAX, 4)` must pass on 64-bit (gating it breaks valid arrays) and fail on 32-bit.
        #[cfg(target_pointer_width = "64")]
        {
            let (size, padded) = super::checked_array_layout(i32::MAX, 4).unwrap();
            assert_eq!(size, (i32::MAX as usize) * 4);
            assert_eq!(padded, super::pad_size(size));
        }
        #[cfg(not(target_pointer_width = "64"))]
        assert_eq!(
            super::checked_array_layout(i32::MAX, 4),
            Err(StatusCode::BadValue)
        );
    }

    #[test]
    fn read_array_rejects_hostile_len_gracefully() {
        // A bare length word far beyond `data_avail()` must be `Err`, never a capacity panic.
        let mut parcel = Parcel::new();
        parcel.write::<i32>(&1_000_000_000).unwrap();
        parcel.set_data_position(0);
        let r = parcel.read_array::<i32>();
        assert!(r.is_err(), "expected Err, got {r:?}");
    }

    #[test]
    fn read_array_char_rejects_hostile_len_gracefully() {
        // Char-array twin of the test above; both go through `checked_array_layout`.
        let mut parcel = Parcel::new();
        parcel.write::<i32>(&1_000_000_000).unwrap();
        parcel.set_data_position(0);
        let r = parcel.read_array_char::<u16>();
        assert!(r.is_err(), "expected Err, got {r:?}");
    }

    #[test]
    fn test_primitives() -> Result<()> {
        let v_i32: i32 = 1234;
        let v_f32: f32 = 5678.0;
        let v_u32: u32 = 9012;
        let v_i64: i64 = 3456;
        let v_u64: u64 = 7890;
        let v_f64: f64 = 9876.0;

        let v_str = "Hello World".to_owned();

        let mut parcel = Parcel::new();

        {
            parcel.write::<i32>(&v_i32)?;
            parcel.write::<u32>(&v_u32)?;
            parcel.write::<f32>(&v_f32)?;
            parcel.write::<i64>(&v_i64)?;
            parcel.write::<u64>(&v_u64)?;
            parcel.write::<f64>(&v_f64)?;

            parcel.write(&v_str)?;
        }

        parcel.set_data_position(0);

        {
            assert_eq!(parcel.read::<i32>()?, v_i32);
            assert_eq!(parcel.read::<u32>()?, v_u32);
            assert_eq!(parcel.read::<f32>()?, v_f32);
            assert_eq!(parcel.read::<i64>()?, v_i64);
            assert_eq!(parcel.read::<u64>()?, v_u64);
            assert_eq!(parcel.read::<f64>()?, v_f64);
            assert_eq!(parcel.read::<String>()?, v_str);
        }

        Ok(())
    }

    #[test]
    fn test_array_byte() {
        let array = vec![255u8, 0u8, 127u8];
        let mut reverse = array.clone();
        reverse.reverse();
        let mut parcel = Parcel::new();

        parcel.write_array(&array).unwrap();
        parcel.write_array(&reverse).unwrap();

        parcel.set_data_position(0);

        let res = parcel.read_array::<u8>().unwrap();
        assert_eq!(array, res.unwrap());
        let res = parcel.read_array::<u8>().unwrap();
        assert_eq!(reverse, res.unwrap());
    }

    #[test]
    fn parcel_array_empty_is_not_null() {
        let mut parcel = Parcel::new();
        parcel.write_array::<u8>(&[]).unwrap();
        parcel.write_array_char::<u16>(&[]).unwrap();
        parcel.set_data_position(0);

        assert_eq!(parcel.read_array::<u8>(), Ok(Some(Vec::new())));
        assert_eq!(parcel.read_array_char::<u16>(), Ok(Some(Vec::new())));
    }

    #[test]
    fn vec_deserialize_rejects_null_but_accepts_empty() {
        let mut parcel = Parcel::new();
        parcel.write(&-1i32).unwrap();
        parcel.write(&0i32).unwrap();
        parcel.write(&0i32).unwrap();
        parcel.write(&-2i32).unwrap();
        parcel.set_data_position(0);

        assert_eq!(parcel.read::<Vec<u8>>(), Err(StatusCode::UnexpectedNull));
        assert_eq!(parcel.read::<Vec<u8>>(), Ok(Vec::new()));
        assert_eq!(parcel.read::<Option<Vec<u8>>>(), Ok(Some(Vec::<u8>::new())));
        assert_eq!(parcel.read::<Vec<u8>>(), Err(StatusCode::UnexpectedNull));
    }

    #[test]
    fn test_array_double() {
        let array = vec![1.0f64 / 3.0f64, 1.0f64 / 7.0f64, 42.0f64];
        let mut reverse = array.clone();
        reverse.reverse();
        let mut parcel = Parcel::new();

        parcel.write_array(&array).unwrap();
        parcel.write_array(&reverse).unwrap();

        println!("{parcel:?}");

        parcel.set_data_position(0);

        let res = parcel.read_array::<f64>().unwrap();
        assert_eq!(array, res.unwrap());
        let res = parcel.read_array::<f64>().unwrap();
        assert_eq!(reverse, res.unwrap());
    }

    #[test]
    fn test_array_char() {
        let array = vec![255u16, 0u16, 127u16];
        let mut reverse = array.clone();
        reverse.reverse();
        let mut parcel = Parcel::new();

        parcel.write_array_char(&array).unwrap();
        parcel.write_array_char(&reverse).unwrap();

        parcel.set_data_position(0);

        let res = parcel.read_array_char::<u16>().unwrap();
        assert_eq!(array, res.unwrap());
        let res = parcel.read_array_char::<u16>().unwrap();
        assert_eq!(reverse, res.unwrap());
    }

    // Typed scalar helpers round-trip and stay wire-identical to generic read::<T>/write::<T>.
    #[test]
    fn test_typed_scalar_helpers() -> Result<()> {
        let mut p = Parcel::new();
        p.write_i32(-7)?;
        p.write_u32(7)?;
        p.write_i64(-8)?;
        p.write_u64(8)?;
        p.write_f32(1.5)?;
        p.write_f64(2.5)?;
        p.write_bool(true)?;
        p.write_i8(-9)?;
        p.write_u8(9)?;
        p.set_data_position(0);
        assert_eq!(p.read_i32()?, -7);
        assert_eq!(p.read_u32()?, 7);
        assert_eq!(p.read_i64()?, -8);
        assert_eq!(p.read_u64()?, 8);
        assert_eq!(p.read_f32()?, 1.5);
        assert_eq!(p.read_f64()?, 2.5);
        assert!(p.read_bool()?);
        assert_eq!(p.read_i8()?, -9);
        assert_eq!(p.read_u8()?, 9);

        // write_i32 is byte-identical to write::<i32>: a generic read decodes it.
        let mut q = Parcel::new();
        q.write_i32(0x1234_5678)?;
        q.set_data_position(0);
        assert_eq!(q.read::<i32>()?, 0x1234_5678);
        Ok(())
    }

    // Issue #97: an empty reply's kernel buffer address must reach BC_FREE_BUFFER unchanged.
    #[test]
    fn from_ipc_parts_preserves_data_pointer_when_length_is_zero() {
        use crate::sys::binder::binder_uintptr_t;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static FREED_DATA_PTR: AtomicUsize = AtomicUsize::new(0);

        fn capture(
            _: Option<&Parcel>,
            data: binder_uintptr_t,
            _: usize,
            _: binder_uintptr_t,
            _: usize,
        ) -> Result<()> {
            FREED_DATA_PTR.store(data as usize, Ordering::SeqCst);
            Ok(())
        }

        // Page-aligned allocation stands in for the kernel-mapped buffer.
        let mut backing = vec![0u8; 4096];
        let original = backing.as_mut_ptr();

        {
            // SAFETY: `original` is a live allocation; a null `objects` with count 0 is allowed.
            let parcel =
                unsafe { Parcel::from_ipc_parts(original, 0, std::ptr::null_mut(), 0, capture) };
            assert_eq!(
                parcel.as_ptr() as usize,
                original as usize,
                "as_ptr() must return the original buffer pointer for empty IPC parcels",
            );
        }

        assert_eq!(
            FREED_DATA_PTR.load(Ordering::SeqCst),
            original as usize,
            "BC_FREE_BUFFER must be issued with the original kernel-supplied pointer",
        );
    }

    #[test]
    fn from_ipc_parts_with_null_data_uses_empty_slice() {
        // A null `data` with `len == 0` is valid and must never reach `slice::from_raw_parts`.
        fn noop(
            _: Option<&Parcel>,
            _: crate::sys::binder::binder_uintptr_t,
            _: usize,
            _: crate::sys::binder::binder_uintptr_t,
            _: usize,
        ) -> Result<()> {
            Ok(())
        }

        // SAFETY: both pointers are null with length 0, the documented empty `from_ipc_parts` case.
        let parcel = unsafe {
            Parcel::from_ipc_parts(std::ptr::null_mut(), 0, std::ptr::null_mut(), 0, noop)
        };
        // Empty slice fallback — pointer is the dangling NonNull but no UB.
        assert_eq!(parcel.data_size(), 0);
        drop(parcel);
    }

    /// A 32-bit kernel's 4-aligned offsets array is copied out, never borrowed as `&[u64]`.
    #[test]
    fn from_ipc_parts_copies_a_misaligned_offsets_array() {
        fn noop(
            _: Option<&Parcel>,
            _: crate::sys::binder::binder_uintptr_t,
            _: usize,
            _: crate::sys::binder::binder_uintptr_t,
            _: usize,
        ) -> Result<()> {
            Ok(())
        }

        let mut backing = vec![0u64; 3];
        let base = backing.as_mut_ptr() as *mut u8;
        // SAFETY: 4 bytes into a 24-byte allocation; the 20 left hold two unaligned `u64`s.
        let objects = unsafe { base.add(4) } as *mut crate::sys::binder::binder_size_t;
        // SAFETY: both writes stay inside `backing`, and `write_unaligned` needs no alignment.
        unsafe {
            objects.write_unaligned(8);
            objects.add(1).write_unaligned(24);
        }
        let mut data = [0u8; 32];
        // SAFETY: `data` and the 2-element `objects` are live and untouched until the drop below.
        let parcel = unsafe { Parcel::from_ipc_parts(data.as_mut_ptr(), 32, objects, 2, noop) };
        assert!(matches!(parcel.objects, super::ParcelData::Vec(_)));
        assert_eq!(parcel.objects.as_slice(), &[8, 24]);
        drop(parcel);
    }

    // Growing into spare capacity would be `Vec::set_len` UB; only the unsafe driver path may grow.
    #[test]
    fn set_data_size_only_shrinks() {
        let mut parcel = Parcel::new();
        parcel.write(&0u64).expect("write u64");
        assert_eq!(parcel.data_size(), 8);

        // Growing past the initialized length is refused, even though the capacity would hold it.
        assert!(parcel.capacity() > 8);
        assert_eq!(parcel.set_data_size(9), Err(StatusCode::BadValue));
        assert_eq!(parcel.data_size(), 8);

        // Shrinking is fine and drags the cursor back with it.
        assert!(parcel.set_data_size(4).is_ok());
        assert_eq!(parcel.data_size(), 4);
        assert_eq!(parcel.data_position(), 4);
        assert!(parcel.set_data_size(0).is_ok());
    }

    #[test]
    fn set_data_size_driver_filled_is_bounded_by_capacity() {
        let mut parcel = Parcel::new();
        let cap = parcel.capacity();
        // SAFETY: like the driver, every byte of the capacity is written before it is claimed.
        unsafe {
            std::ptr::write_bytes(parcel.as_mut_ptr(), 0xAB, cap);
            assert!(parcel.set_data_size_driver_filled(cap).is_ok());
            assert_eq!(
                parcel.set_data_size_driver_filled(cap + 1),
                Err(StatusCode::BadValue)
            );
        }
        assert_eq!(parcel.data_size(), cap);
        assert!(parcel.data.as_slice().iter().all(|&b| b == 0xAB));
    }

    // A bare forward seek must not push the kernel-facing length past the allocated bytes.
    #[test]
    fn ipc_data_size_never_exceeds_backing_buffer() {
        let mut parcel = Parcel::new();
        parcel.write(&1u32).expect("write");
        parcel.set_data_position(1024);
        assert_eq!(parcel.data_size(), 1024, "AOSP dataSize() = max(len, pos)");
        assert_eq!(
            parcel.ipc_data_size(),
            4,
            "only the written bytes go to the kernel"
        );

        // Once something is written the gap is zero-filled and the two agree.
        parcel.write(&2u32).expect("write past gap");
        assert_eq!(parcel.ipc_data_size(), 1028);
        assert_eq!(parcel.ipc_data_size(), parcel.data_size());
    }

    #[test]
    fn set_data_position_rejects_past_i32_max() {
        let mut parcel = Parcel::new();
        parcel.write(&7u32).expect("write");
        parcel.set_data_position(0);
        parcel.set_data_position(i32::MAX as usize + 1);
        assert_eq!(parcel.data_position(), 0, "out-of-range seek is ignored");
        parcel.set_data_position(i32::MAX as usize);
        assert_eq!(parcel.data_position(), i32::MAX as usize);
    }

    // `data_avail` saturates, never underflows, once a seek puts the cursor past the end.
    #[test]
    fn data_avail_saturates_when_pos_past_end() {
        let mut parcel = Parcel::new();
        parcel.write(&0u64).expect("write u64");
        assert_eq!(parcel.data_avail(), 0, "cursor at end → nothing available");

        parcel.set_data_position(0);
        assert_eq!(parcel.data_avail(), 8, "8 bytes available from start");

        // Cursor far past the end must not underflow-panic.
        parcel.set_data_position(9999);
        assert_eq!(
            parcel.data_avail(),
            0,
            "saturating_sub, not underflow panic"
        );
    }

    /// Stable-AIDL compat: missing trailing fields stay default, extra ones are skipped.
    #[test]
    fn sized_read_field_truncation_via_has_more_data() {
        // A "V1" writer emits a length-prefixed parcelable of two i32s.
        let mut wv1 = Parcel::new();
        wv1.sized_write(|p| {
            p.write(&11i32)?;
            p.write(&22i32)
        })
        .expect("v1 write");
        // Trailing sentinel after the parcelable exposes any over-read past the block boundary.
        wv1.write(&0x7777_7777i32).expect("sentinel");

        // A "V3" reader expects three i32s, each guarded by has_more_data.
        wv1.set_data_position(0);
        let (mut a, mut b, mut c) = (0i32, 0i32, -1i32);
        wv1.sized_read(|p| {
            if !p.has_more_data() {
                return Ok(());
            }
            a = p.read()?;
            if !p.has_more_data() {
                return Ok(());
            }
            b = p.read()?;
            if !p.has_more_data() {
                return Ok(());
            }
            c = p.read()?;
            Ok(())
        })
        .expect("v3 read of v1 data");
        assert_eq!((a, b), (11, 22), "present fields read");
        assert_eq!(c, -1, "absent trailing field left at default, no over-read");
        // The cursor is parked at the parcelable end → sentinel reads next.
        assert_eq!(wv1.read::<i32>().expect("sentinel"), 0x7777_7777);

        // Reverse: a "V1" reader skips a "V3" writer's third i32 and lands on the sentinel.
        let mut wv3 = Parcel::new();
        wv3.sized_write(|p| {
            p.write(&1i32)?;
            p.write(&2i32)?;
            p.write(&3i32)
        })
        .expect("v3 write");
        wv3.write(&0x5555_5555i32).expect("sentinel");

        wv3.set_data_position(0);
        let (mut x, mut y) = (0i32, 0i32);
        wv3.sized_read(|p| {
            if !p.has_more_data() {
                return Ok(());
            }
            x = p.read()?;
            if !p.has_more_data() {
                return Ok(());
            }
            y = p.read()?;
            Ok(())
        })
        .expect("v1 read of v3 data");
        assert_eq!((x, y), (1, 2), "first two fields read");
        assert_eq!(
            wv3.read::<i32>().expect("sentinel"),
            0x5555_5555,
            "extra field skipped to block end, sentinel intact"
        );
    }

    /// Nesting past [`MAX_NESTED_READ_DEPTH`] is `BadValue`; the depth never leaks across reads.
    #[test]
    fn sized_read_depth_is_bounded() {
        // Mirrors a generated `RecursiveList` write: a sized block of a marker plus the next node.
        fn write_nested(p: &mut Parcel, depth: usize) -> Result<()> {
            p.sized_write(|s| {
                s.write(&(depth as i32))?;
                if depth > 1 {
                    write_nested(s, depth - 1)?;
                }
                Ok(())
            })
        }
        fn read_nested(p: &mut Parcel) -> Result<()> {
            p.sized_read(|s| {
                let _marker: i32 = s.read()?;
                if s.has_more_data() {
                    read_nested(s)?;
                }
                Ok(())
            })
        }

        // Nesting beyond the cap → BadValue, not a stack-overflow abort.
        let mut over = Parcel::new();
        write_nested(&mut over, super::MAX_NESTED_READ_DEPTH + 50).expect("write over-deep");
        over.set_data_position(0);
        assert_eq!(read_nested(&mut over).unwrap_err(), StatusCode::BadValue);

        // Reading twice proves the depth counter is restored on success and on the error above.
        let mut ok = Parcel::new();
        write_nested(&mut ok, 8).expect("write shallow");
        for _ in 0..2 {
            ok.set_data_position(0);
            read_nested(&mut ok).expect("shallow read");
        }
    }

    /// Object table per module doc "RPC fields": offsets, sort, kernel refusal, v2 round trip.
    #[cfg(feature = "rpc")]
    #[test]
    fn rpc_object_position_table_is_aosp_faithful_and_sorted() {
        use crate::rpc::wire::{WireCodec, WireMessage, WireTransaction};
        use crate::rpc::wire_android13::Android13PlusCodec;

        // ---- kernel parcel: recording is a hard no-op ----
        let mut kparcel = Parcel::new();
        kparcel.write(&7i32).unwrap();
        kparcel.rpc_record_object_position(0); // kernel-backed ⇒ ignored
        assert!(
            kparcel.rpc_object_positions().is_empty(),
            "kernel parcel must never grow an object table"
        );

        // ---- RPC parcel: AOSP-faithful flatten sequence ----
        let mut p = Parcel::new();
        p.set_for_rpc(true);
        p.set_rpc_record_fd_positions(true);

        // A position is the leading i32's offset, taken before it is written (AOSP flattenBinder).
        p.write(&0xDEAD_BEEFu32).unwrap(); // token-ish
        p.write(&"iface".to_owned()).unwrap(); // a String arg

        let mut expect: Vec<u32> = Vec::new();

        // binder #1
        let pos = p.data_position();
        expect.push(pos as u32);
        p.write(&1i32).unwrap(); // present/TYPE_BINDER
        p.write_aligned_data(&[0u8; 8]).unwrap(); // 8B RpcWireAddress
        p.write(&0x0Ci32).unwrap(); // stability
        p.rpc_record_object_position(pos);

        p.write(&123i64).unwrap(); // an interleaved scalar

        // fd #1
        let pos = p.data_position();
        expect.push(pos as u32);
        p.write(&1i32).unwrap(); // present
        p.write(&0i32).unwrap(); // ancillary index
        p.rpc_record_object_position(pos);

        // binder #2
        let pos = p.data_position();
        expect.push(pos as u32);
        p.write(&1i32).unwrap();
        p.write_aligned_data(&[0u8; 8]).unwrap();
        p.write(&0x0Ci32).unwrap();
        p.rpc_record_object_position(pos);

        // Already ascending (objects written front-to-back).
        assert_eq!(
            p.rpc_object_positions(),
            &expect[..],
            "AOSP dataPos offsets"
        );
        assert!(
            p.rpc_object_positions().windows(2).all(|w| w[0] < w[1]),
            "table strictly ascending"
        );

        // AOSP inserts at `upper_bound`, so an out-of-order record still lands sorted.
        let mut q = Parcel::new();
        q.set_for_rpc(true);
        for pos in [40u32, 8, 24, 8, 0] {
            q.rpc_record_object_position(pos as usize);
        }
        assert_eq!(
            q.rpc_object_positions(),
            &[0, 8, 8, 24, 40],
            "upper_bound insert keeps the table sorted (dups allowed)"
        );

        // v2 strict receive (AOSP `unflattenBinder`): an unrecorded position ⇒ BAD_VALUE.
        for &good in &[0u32, 8, 24, 40] {
            assert!(q.rpc_object_position_present(good as usize), "pos {good}");
        }
        for bad in [4usize, 12, 41, 9999] {
            assert!(
                !q.rpc_object_position_present(bad),
                "unrecorded pos {bad} must fail the v2 binary_search"
            );
        }

        // ---- v2 codec: positions round-trip; bodySize = 40 + dataSize + 4·N (AOSP RpcState) ----
        let c = Android13PlusCodec::android16();
        let data = p.rpc_data_bytes().to_vec();
        let positions = p.rpc_object_positions().to_vec();
        let txn = WireTransaction {
            address: crate::rpc::address::RpcAddress::zero(),
            code: 1,
            flags: 0,
            async_number: 0,
            data: data.clone(),
            object_positions: positions.clone(),
        };
        let enc = c.encode_transact(&txn).unwrap();
        let body = u32::from_le_bytes([enc[4], enc[5], enc[6], enc[7]]) as usize;
        assert_eq!(
            body,
            40 + data.len() + 4 * positions.len(),
            "bodySize = fixed(40) + parcelDataSize + 4·N"
        );
        match c.decode_message(&enc).unwrap() {
            WireMessage::Transact(d) => {
                assert_eq!(d.data, data, "parcel data intact");
                assert_eq!(d.object_positions, positions, "object table intact");
            }
            o => panic!("expected Transact, got {o:?}"),
        }
    }
}

#[cfg(test)]
mod wire_golden {
    //! Absolute little-endian byte goldens for the data-parcel wire.
    //!
    //! Every assertion here is a **literal byte sequence**, never a round
    //! trip. A round trip re-reads with the same codec, so it passes on a
    //! big-endian host even when the bytes are wrong, and the rest of the
    //! suite cannot see the wire layout at all.
    //!
    //! These goldens are therefore the *definition* of what a little-endian
    //! peer puts on the wire, and the `cross`/qemu s390x job runs them
    //! unchanged: a big-endian build that produces these bytes is
    //! cross-endian compatible by construction, no networking required.
    //!
    //! Two things are deliberately absent. `flat_binder_object` and the
    //! kernel command stream are **not** wire — they are the kernel's own
    //! ABI and stay host-native; the one exception below pins that as a
    //! decision rather than an omission. Anything needing a live
    //! `ProcessState` (a non-null binder, a real fd) belongs in the
    //! kernel-host suite, not here — this module must stay hermetic so it
    //! can run under qemu.

    use super::*;

    /// The bytes a fresh kernel-mode parcel holds after writing `value`.
    fn enc<S: Serialize + ?Sized>(value: &S) -> Vec<u8> {
        let mut parcel = Parcel::new();
        parcel.write(value).unwrap();
        parcel.data.as_slice().to_vec()
    }

    #[test]
    fn scalar_wire_is_absolute_little_endian() {
        assert_eq!(enc(&true), [0x01, 0x00, 0x00, 0x00]);
        assert_eq!(enc(&false), [0x00, 0x00, 0x00, 0x00]);

        assert_eq!(enc(&0x0102_0304i32), [0x04, 0x03, 0x02, 0x01]);
        assert_eq!(enc(&0xDEAD_BEEFu32), [0xEF, 0xBE, 0xAD, 0xDE]);
        assert_eq!(
            enc(&0x0102_0304_0506_0708i64),
            [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
        assert_eq!(
            enc(&0xDEAD_BEEF_CAFE_BABEu64),
            [0xBE, 0xBA, 0xFE, 0xCA, 0xEF, 0xBE, 0xAD, 0xDE]
        );
        assert_eq!(
            enc(&0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10u128),
            [
                0x10, 0x0F, 0x0E, 0x0D, 0x0C, 0x0B, 0x0A, 0x09, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03,
                0x02, 0x01
            ]
        );

        // IEEE-754 bit patterns, byte-reversed: 1.0f32 = 0x3F80_0000.
        assert_eq!(enc(&1.0f32), [0x00, 0x00, 0x80, 0x3F]);
        assert_eq!(enc(&-2.0f32), [0x00, 0x00, 0x00, 0xC0]);
        assert_eq!(
            enc(&1.0f64),
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F]
        );
    }

    #[test]
    fn scalar_widening_matches_the_aidl_wire() {
        // Scalars widen to 32 bits *before* the byte swap; swapping un-widened desyncs the rest.
        assert_eq!(enc(&-2i8), [0xFE, 0xFF, 0xFF, 0xFF]);
        assert_eq!(enc(&0xABu8), [0xAB, 0x00, 0x00, 0x00]);
        assert_eq!(enc(&-2i16), [0xFE, 0xFF, 0xFF, 0xFF]);
        assert_eq!(enc(&0xBEEFu16), [0xEF, 0xBE, 0x00, 0x00]);
    }

    #[test]
    fn array_wire_is_absolute_little_endian() {
        // Arrays invert the widening: `i8`/`u8` are one byte per element, zero-padded to 4 bytes.
        assert_eq!(
            enc(&[0xABu8, 0xCD, 0xEF][..]),
            [0x03, 0x00, 0x00, 0x00, 0xAB, 0xCD, 0xEF, 0x00]
        );
        assert_eq!(
            enc(&[-2i8, 1][..]),
            [0x02, 0x00, 0x00, 0x00, 0xFE, 0x01, 0x00, 0x00]
        );

        // ...while `i16`/`u16` are four bytes per element.
        assert_eq!(
            enc(&[-2i16, 3][..]),
            [0x02, 0x00, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF, 0x03, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            enc(&[0xBEEFu16][..]),
            [0x01, 0x00, 0x00, 0x00, 0xEF, 0xBE, 0x00, 0x00]
        );

        assert_eq!(
            enc(&[0x0102_0304i32, -1][..]),
            [0x02, 0x00, 0x00, 0x00, 0x04, 0x03, 0x02, 0x01, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(
            enc(&[1i64][..]),
            [0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            enc(&[1.0f64][..]),
            [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F]
        );

        // The length word is itself a wire `i32`.
        assert_eq!(enc(&[0i32; 0][..]), [0x00, 0x00, 0x00, 0x00]);
        assert_eq!(enc(&None::<Vec<i32>>), [0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn string16_wire_is_absolute_little_endian() {
        // [i32 code-unit count][UTF-16 units][NUL unit][pad to 4].
        assert_eq!(
            enc("AB"),
            [0x02, 0x00, 0x00, 0x00, 0x41, 0x00, 0x42, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        // U+D55C's two bytes differ, so it catches a native-endian `u16` view; ASCII cannot.
        assert_eq!(enc("한"), [0x01, 0x00, 0x00, 0x00, 0x5C, 0xD5, 0x00, 0x00]);
        assert_eq!(enc(""), [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(enc(&None::<String>), [0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn stability_word_is_little_endian() {
        // The category repr varies by platform (android-12), so only the byte order is pinned.
        let level = i32::from(crate::Stability::Vintf);
        assert_eq!(
            enc(&level),
            [
                (level & 0xFF) as u8,
                ((level >> 8) & 0xFF) as u8,
                ((level >> 16) & 0xFF) as u8,
                ((level >> 24) & 0xFF) as u8,
            ]
        );
        #[cfg(not(target_os = "android"))]
        assert_eq!(enc(&level), [0x3F, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn a_parcel_of_mixed_fields_keeps_every_slot_aligned() {
        // Each field's padding sets the next one's offset, which a per-type golden cannot catch.
        let mut parcel = Parcel::new();
        parcel.write(&-2i8).unwrap();
        parcel.write("한").unwrap();
        parcel.write(&[0x0102_0304i32][..]).unwrap();

        assert_eq!(
            parcel.data.as_slice().to_vec(),
            [
                0xFE, 0xFF, 0xFF, 0xFF, // i8 -2, widened to i32
                0x01, 0x00, 0x00, 0x00, // String16: 1 code unit
                0x5C, 0xD5, 0x00, 0x00, //   U+D55C then the NUL unit
                0x01, 0x00, 0x00, 0x00, // i32[]: 1 element
                0x04, 0x03, 0x02, 0x01, //   element 0
            ]
        );
    }

    #[test]
    fn the_command_stream_is_native_not_wire() {
        // The driver reads `BC_*` natively; only the accessor name picks native vs wire per slot.
        let cmd: u32 = crate::sys::binder::BC_ACQUIRE;

        let mut native = Parcel::new();
        native.write_native::<u32>(&cmd).unwrap();
        assert_eq!(native.data.as_slice().to_vec(), cmd.to_ne_bytes());

        let mut wire = Parcel::new();
        wire.write_le::<u32>(&cmd).unwrap();
        assert_eq!(wire.data.as_slice().to_vec(), cmd.to_le_bytes());

        // Equal on a little-endian host; only a big-endian run can tell the two layers apart.
        if cfg!(target_endian = "big") {
            assert_ne!(native.data.as_slice(), wire.data.as_slice());
        }

        native.set_data_position(0);
        assert_eq!(native.read_native::<u32>().unwrap(), cmd);
    }

    #[test]
    fn null_binder_stays_a_native_island() {
        // `flat_binder_object` is driver-parsed UAPI: it stays host-native on a little-endian wire.
        let mut parcel = Parcel::new();
        // A null binder skips `acquire()`, so the test needs no `ProcessState`.
        SerializeOption::serialize_option(None::<&crate::SIBinder>, &mut parcel).unwrap();
        let bytes = parcel.data.as_slice();

        let obj_len = std::mem::size_of::<flat_binder_object>();
        assert_eq!(
            bytes[..4],
            crate::sys::BINDER_TYPE_BINDER.to_ne_bytes()[..],
            "object header is native, not little-endian"
        );
        assert!(
            bytes[4..obj_len].iter().all(|&b| b == 0),
            "a null binder carries no handle, cookie or flags"
        );
    }
}

/// `to_bytes` output must be self-contained; not behind `rpc`, as non-`rpc` builds rely on it too.
#[cfg(test)]
mod data_serde {
    use super::*;

    #[test]
    fn a_value_survives_the_round_trip_and_encodes_the_same_way_twice() {
        let value: Vec<i32> = vec![1, -2, 0x0102_0304];
        let bytes = to_bytes(&value).unwrap();
        assert_eq!(
            bytes,
            to_bytes(&value).unwrap(),
            "encoding is deterministic"
        );
        assert_eq!(from_bytes::<Vec<i32>>(&bytes).unwrap(), value);

        let text = String::from("한 quiet");
        assert_eq!(
            from_bytes::<String>(&to_bytes(&text).unwrap()).unwrap(),
            text
        );
    }

    #[test]
    fn the_bytes_are_the_ipc_bytes() {
        // Reusing the IPC codec means the stored bytes are exactly what a peer would receive.
        let value = -2i64;
        let mut kernel = Parcel::new();
        kernel.write(&value).unwrap();
        assert_eq!(to_bytes(&value).unwrap(), kernel.data.as_slice());
    }

    #[test]
    fn trailing_bytes_are_an_error_rather_than_a_partial_read() {
        let mut bytes = to_bytes(&7i32).unwrap();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(from_bytes::<i32>(&bytes), Err(StatusCode::BadValue));
    }

    #[test]
    fn a_truncated_input_is_not_enough_data() {
        let bytes = to_bytes(&7i64).unwrap();
        assert_eq!(
            from_bytes::<i64>(&bytes[..4]),
            Err(StatusCode::NotEnoughData)
        );
    }

    #[test]
    fn a_binder_field_is_refused_at_write_time() {
        // Refused at write time: even a null binder needs an object table this parcel lacks.
        let mut parcel = Parcel::new_data_only();
        let binder: Option<&crate::SIBinder> = None;
        assert_eq!(
            crate::SerializeOption::serialize_option(binder, &mut parcel),
            Err(StatusCode::BadType),
            "even a null binder needs an object table it will not get"
        );
    }

    #[test]
    fn a_file_descriptor_field_is_refused_before_the_dup() {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        let pfd = crate::ParcelFileDescriptor::new(file);

        let mut parcel = Parcel::new_data_only();
        assert_eq!(parcel.write(&pfd), Err(StatusCode::FdsNotAllowed));
        assert!(
            pfd.as_raw_fd() >= 0,
            "the caller's fd is untouched by the refusal"
        );
        // No fd-count assertion: the count is process-global and sibling tests dup fds in parallel.
    }

    #[test]
    fn a_forged_object_in_the_input_never_becomes_a_binder() {
        // Forged object bytes have no object table to resolve against, so no reference is made.
        let mut forged = Vec::new();
        forged.extend_from_slice(&crate::sys::BINDER_TYPE_BINDER.to_ne_bytes());
        forged.extend_from_slice(&[0u8; 20]);
        // Kernel parcel only (RPC short-circuits); the 8-byte case shows BadType is not constant.
        assert_eq!(
            Parcel::from_vec(forged.clone()).read_object(true).err(),
            Some(StatusCode::BadType),
            "the object decoder refuses an object with no offset-table entry"
        );
        assert_eq!(
            Parcel::from_vec(vec![0u8; 8]).read_object(true).err(),
            Some(StatusCode::NotEnoughData),
            "too few bytes to be an object fails before the table lookup"
        );
        // Data-only mode has no session, so the public decoder refuses whatever the bytes claim.
        assert_eq!(
            from_bytes::<crate::SIBinder>(&forged),
            Err(StatusCode::BadType)
        );
    }

    // An object position is RPC state; without the feature no parcel can acquire one.
    #[cfg(feature = "rpc")]
    #[test]
    fn a_parcel_holding_a_reference_refuses_to_hand_out_its_bytes() {
        // RPC mode keeps `objects` empty, so the guard must consult the object positions too.
        let mut parcel = Parcel::new_data_only();
        parcel.write(&1i32).unwrap();
        assert!(parcel.as_bytes().is_ok());

        parcel.rpc_record_object_position(0);
        assert_eq!(
            parcel.as_bytes().err(),
            Some(StatusCode::BadType),
            "an object position makes the bytes incomplete"
        );
    }

    #[test]
    fn an_appended_object_table_is_refused_before_the_bytes_are_copied() {
        // Neither source object owns a reference, so the source's drop has nothing to undo.
        for (object, expected) in [
            (
                flat_binder_object::new_binder_with_flags(0),
                StatusCode::BadType,
            ),
            (
                flat_binder_object::new_with_fd(0, false),
                StatusCode::FdsNotAllowed,
            ),
        ] {
            let mut source = Parcel::new();
            source.write_object(&object, true).unwrap();

            let mut sink = Parcel::new_data_only();
            assert_eq!(sink.append_all_from(&mut source), Err(expected));
            assert!(
                sink.as_bytes().unwrap().is_empty(),
                "refused before the copy — no object bytes reached the sink"
            );
        }
    }

    // Needs a session's marshalling hooks, which only `rpc` supplies.
    #[cfg(feature = "rpc")]
    #[test]
    fn a_holder_cut_from_a_session_parcel_cannot_be_exported() {
        // The binder lives in the body, which no copied table marks; the refusal keys on session.
        use crate::Parcelable;

        struct SessionOps;
        impl RpcParcelOps for SessionOps {
            fn write_binder(&self, _b: Option<&crate::SIBinder>, _p: &mut Parcel) -> Result<()> {
                Err(StatusCode::DeadObject)
            }
            fn read_binder(&self, _p: &mut Parcel) -> Result<Option<crate::SIBinder>> {
                Err(StatusCode::DeadObject)
            }
            fn cancel_leaving(&self, _addrs: &[crate::rpc::RpcAddress]) {}
            fn session_id(&self) -> *const () {
                std::ptr::null()
            }
            fn records_binder_positions(&self) -> Result<bool> {
                Ok(true)
            }
            fn acquire_copied(&self, _objects: &[&[u8]]) -> Result<CopiedBinders> {
                Err(StatusCode::DeadObject)
            }
        }

        const ADDR: [u8; 8] = *b"\xde\xad\xbe\xef\xfe\xed\xfa\xce";

        // A holder as it arrives in an RPC transaction: stability, length, then a flattened binder.
        let mut txn = Parcel::new();
        txn.set_for_rpc(true);
        txn.attach_rpc_ops(std::sync::Arc::new(SessionOps));
        txn.write(&0i32).unwrap(); // STABILITY_LOCAL
        txn.write(&12i32).unwrap(); // payload length
        let obj_pos = txn.data_position();
        txn.write(&1i32).unwrap(); // binder present
        txn.write_aligned_data(&ADDR).unwrap();
        txn.rpc_record_object_position(obj_pos);
        txn.set_data_position(0);

        let mut holder = crate::ParcelableHolder::new(crate::Stability::Local);
        holder.read_from_parcel(&mut txn).unwrap();
        assert_eq!(
            holder.with_payload_parcel(|p| p.map(|p| p.rpc_object_positions().to_vec())),
            Some(vec![0]),
            "the sub-parcel keeps the binder's position, shifted to its own start"
        );

        let exported = to_bytes(&holder);
        assert!(
            !exported
                .as_deref()
                .is_ok_and(|b| b.windows(ADDR.len()).any(|w| w == ADDR)),
            "the session address reached the exported bytes"
        );
        assert_eq!(exported.err(), Some(StatusCode::BadType));
    }

    /// `RpcParcelOps` that records every `cancel_leaving` call, standing in for a session.
    #[cfg(feature = "rpc")]
    #[derive(Default)]
    struct RecordingOps(std::sync::Mutex<Vec<Vec<crate::rpc::RpcAddress>>>);
    #[cfg(feature = "rpc")]
    impl RpcParcelOps for RecordingOps {
        fn write_binder(&self, _b: Option<&crate::SIBinder>, _p: &mut Parcel) -> Result<()> {
            Err(StatusCode::DeadObject)
        }
        fn read_binder(&self, _p: &mut Parcel) -> Result<Option<crate::SIBinder>> {
            Err(StatusCode::DeadObject)
        }
        fn cancel_leaving(&self, addrs: &[crate::rpc::RpcAddress]) {
            self.0.lock().unwrap().push(addrs.to_vec());
        }
        fn session_id(&self) -> *const () {
            (self as *const Self).cast()
        }
        fn records_binder_positions(&self) -> Result<bool> {
            Ok(true)
        }
        fn acquire_copied(&self, _objects: &[&[u8]]) -> Result<CopiedBinders> {
            Ok(CopiedBinders::default())
        }
    }

    /// Inode of `fd`: which open file an fd table slot holds.
    #[cfg(feature = "rpc")]
    #[allow(clippy::unnecessary_cast)] // `st_ino` is not `u64` on every target.
    fn inode_of(fd: impl std::os::fd::AsFd) -> u64 {
        rustix::fs::fstat(fd).unwrap().st_ino as u64
    }

    /// AOSP `appendFrom` `isForRpc()`/`mSession` check: data-only bytes stay out of a session.
    #[cfg(feature = "rpc")]
    #[test]
    fn append_from_refuses_a_session_less_source_into_a_session_parcel() {
        let mut source = Parcel::new_data_only();
        source.write(&1i32).unwrap();
        let mut dest = parcel_with_reservations(std::sync::Arc::default(), 0);
        assert_eq!(dest.append_all_from(&mut source), Err(StatusCode::BadType));
        assert_eq!(dest.data_size(), 0, "refused before the copy");
    }

    #[cfg(feature = "rpc")]
    #[test]
    fn append_from_refuses_a_kernel_source_into_a_session_parcel() {
        // No kernel object in range, so only the session check can refuse it.
        let mut source = Parcel::new();
        source.write(&1i32).unwrap();
        let mut dest = parcel_with_reservations(std::sync::Arc::default(), 0);
        assert_eq!(dest.append_all_from(&mut source), Err(StatusCode::BadType));
        assert_eq!(dest.data_size(), 0, "refused before the copy");
    }

    /// AOSP `isForRpc()` mismatch the other way: neither RPC mode reaches a kernel parcel.
    #[cfg(feature = "rpc")]
    #[test]
    fn append_from_refuses_an_rpc_source_into_a_kernel_parcel() {
        let session = parcel_with_reservations(std::sync::Arc::default(), 0);
        for mut source in [session, Parcel::new_data_only()] {
            source.write(&1i32).unwrap();
            let mut dest = Parcel::new();
            assert_eq!(dest.append_all_from(&mut source), Err(StatusCode::BadType));
            assert_eq!(dest.data_size(), 0, "refused before the copy");
        }
    }

    /// A holder's sub-parcel keeps the positions and v1+ fds of its own range, shifted.
    #[cfg(feature = "rpc")]
    #[test]
    fn holder_sub_parcel_positions_are_shifted_and_bounded() {
        use crate::rpc::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR;
        use crate::Parcelable;

        let (outside, inside) = std::os::unix::net::UnixStream::pair().unwrap();
        let (outside_ino, inside_ino) = (inode_of(&outside), inode_of(&inside));

        let mut txn = Parcel::new();
        txn.configure_rpc(
            std::sync::Arc::new(RecordingOps::default()),
            crate::rpc::FileDescriptorTransportMode::Unix,
            true,
        );
        let binder = |p: &mut Parcel| {
            p.rpc_record_object_position(p.data_position());
            p.write(&RPC_TYPE_BINDER).unwrap();
            p.write_aligned_data(&[0xa5u8; 8]).unwrap();
        };
        let fd = |p: &mut Parcel, idx: i32| {
            p.rpc_record_object_position(p.data_position());
            p.write(&TYPE_NATIVE_FILE_DESCRIPTOR).unwrap();
            p.write(&idx).unwrap();
        };
        binder(&mut txn); // 0: before the holder
        txn.write(&0i32).unwrap(); // STABILITY_LOCAL
        txn.write(&20i32).unwrap(); // payload length
        fd(&mut txn, 1); // 20: first byte of the payload
        binder(&mut txn); // 28
        fd(&mut txn, 0); // 40: first byte after the payload
        txn.rpc_set_in_fds(vec![outside.into(), inside.into()]);
        txn.set_data_position(12);

        let mut holder = crate::ParcelableHolder::new(crate::Stability::Local);
        holder.read_from_parcel(&mut txn).unwrap();
        assert_eq!(
            txn.data_position(),
            40,
            "the outer read resumes after the payload"
        );
        holder.with_payload_parcel(|sub| {
            let sub = sub.expect("an undecoded payload");
            assert_eq!(sub.data_size(), 20);
            assert_eq!(
                sub.rpc_object_positions(),
                &[0, 8],
                "in range only, minus its start"
            );
            assert_eq!(
                sub.rpc_fd_mode(),
                crate::rpc::FileDescriptorTransportMode::Unix
            );
            assert!(sub.rpc_record_fd_positions());
            assert_eq!(
                le_i32_at(sub.rpc_data_bytes(), 4),
                Some(0),
                "the index names the sub-parcel's own table"
            );
            assert_eq!(sub.rpc_take_in_fd(0).map(inode_of), Some(inside_ino));
        });
        assert!(
            txn.rpc_take_in_fd(1).is_none(),
            "the payload's fd moved out"
        );
        assert_eq!(txn.rpc_take_in_fd(0).map(inode_of), Some(outside_ino));
    }

    /// A copied fd's index names the destination's own table, after the fds it already holds.
    #[cfg(feature = "rpc")]
    #[test]
    fn append_from_rewrites_a_copied_fd_index_into_the_destination_table() {
        use crate::ParcelFileDescriptor;

        let ops = std::sync::Arc::new(RecordingOps::default());
        let rpc_parcel = || {
            let mut p = Parcel::new();
            p.configure_rpc(
                ops.clone(),
                crate::rpc::FileDescriptorTransportMode::Unix,
                true,
            );
            p
        };
        let (first, copied) = std::os::unix::net::UnixStream::pair().unwrap();
        let (first_ino, copied_ino) = (inode_of(&first), inode_of(&copied));

        let mut source = rpc_parcel();
        source.write(&ParcelFileDescriptor::new(copied)).unwrap();
        let mut dest = rpc_parcel();
        dest.write(&ParcelFileDescriptor::new(first)).unwrap();
        let start = dest.data_position();
        dest.append_all_from(&mut source).unwrap();

        let shifted: Vec<u32> = source
            .rpc_object_positions()
            .iter()
            .map(|&p| p + start as u32)
            .collect();
        assert_eq!(&dest.rpc_object_positions()[1..], &shifted[..]);
        let copied_pos = shifted[0] as usize;
        assert_eq!(le_i32_at(dest.rpc_data_bytes(), copied_pos + 4), Some(1));
        let inodes: Vec<u64> = dest.rpc_out_fds().iter().map(inode_of).collect();
        assert_eq!(inodes, vec![first_ino, copied_ino]);
        assert_eq!(
            source.rpc_out_fds().len(),
            1,
            "the source keeps its fd: the copy is a dup"
        );
    }

    /// An RPC parcel bound to `ops` with `n` recorded `leaving_addrs`.
    #[cfg(feature = "rpc")]
    fn parcel_with_reservations(ops: std::sync::Arc<RecordingOps>, n: u64) -> Parcel {
        use crate::rpc::{AddressSpace, RpcAddress};
        let mut p = Parcel::new();
        p.configure_rpc(ops, crate::rpc::FileDescriptorTransportMode::None, false);
        let mut counter = 0u64;
        for _ in 0..n {
            p.rpc_record_leaving_addr(RpcAddress::unique(&mut counter, AddressSpace::Acceptor));
        }
        p
    }

    #[cfg(feature = "rpc")]
    #[test]
    fn unsent_rpc_parcel_settles_once_on_drop() {
        let ops = std::sync::Arc::new(RecordingOps::default());
        let p = parcel_with_reservations(ops.clone(), 2);
        assert!(
            ops.0.lock().unwrap().is_empty(),
            "nothing settles before the drop"
        );
        drop(p);
        {
            let calls = ops.0.lock().unwrap();
            assert_eq!(calls.len(), 1, "one `cancel_leaving` per dropped parcel");
            assert_eq!(calls[0].len(), 2, "every recorded address is handed back");
        }

        // Leaving RPC mode discards the fields, so it settles the same way; drop finds nothing.
        let mut p = parcel_with_reservations(ops.clone(), 1);
        p.set_for_rpc(false);
        assert_eq!(ops.0.lock().unwrap().len(), 2);
        drop(p);
        assert_eq!(
            ops.0.lock().unwrap().len(),
            2,
            "no second settlement after `set_for_rpc`"
        );
    }

    #[cfg(feature = "rpc")]
    #[test]
    fn sent_rpc_parcel_does_not_settle() {
        let ops = std::sync::Arc::new(RecordingOps::default());
        let p = parcel_with_reservations(ops.clone(), 1);
        p.rpc_begin_send().unwrap();
        p.rpc_end_send(true);
        drop(p);
        assert!(
            ops.0.lock().unwrap().is_empty(),
            "a `Sent` parcel's bumps belong to the peer's DEC_STRONG"
        );
    }

    /// A claim a panic left behind (`InFlight` at drop) means no send completed: it settles.
    #[cfg(feature = "rpc")]
    #[test]
    fn in_flight_rpc_parcel_settles_on_drop() {
        let ops = std::sync::Arc::new(RecordingOps::default());
        let p = parcel_with_reservations(ops.clone(), 1);
        p.rpc_begin_send().unwrap();
        drop(p);
        assert_eq!(ops.0.lock().unwrap().len(), 1);
    }

    /// AOSP Parcel.cpp `appendFrom`: "Cannot append Parcels from different sessions".
    #[cfg(feature = "rpc")]
    #[test]
    fn append_from_refuses_another_session() {
        let mut source = parcel_with_reservations(std::sync::Arc::default(), 0);
        source.write(&7i32).unwrap();
        let mut dest = parcel_with_reservations(std::sync::Arc::default(), 0);
        assert_eq!(dest.append_all_from(&mut source), Err(StatusCode::BadType));
        assert!(
            dest.as_bytes().unwrap().is_empty(),
            "refused before the copy"
        );
    }

    #[cfg(feature = "rpc")]
    #[test]
    fn kernel_parcel_ignores_send_state() {
        let p = Parcel::new();
        assert_eq!(p.rpc_begin_send(), Ok(()));
        assert_eq!(
            p.rpc_begin_send(),
            Ok(()),
            "no state to claim: every claim succeeds"
        );
        p.rpc_end_send(true);
        p.rpc_end_send(false);
        assert!(p.rpc.is_none(), "the calls do not create `RpcFields`");
        drop(p);
    }

    #[cfg(feature = "rpc")]
    #[test]
    fn append_from_refuses_a_source_with_reservations() {
        let ops = std::sync::Arc::new(RecordingOps::default());
        let mut source = parcel_with_reservations(ops.clone(), 1);
        source.write(&7i32).unwrap();
        let mut dest = parcel_with_reservations(ops.clone(), 0);
        assert_eq!(dest.append_all_from(&mut source), Err(StatusCode::BadType));
        assert!(
            dest.as_bytes().unwrap().is_empty(),
            "refused before the copy"
        );

        // A source without reservations is fine until the destination has been sent.
        let mut clean = parcel_with_reservations(ops.clone(), 0);
        clean.write(&7i32).unwrap();
        assert_eq!(dest.append_all_from(&mut clean), Ok(()));
        dest.rpc_begin_send().unwrap();
        dest.rpc_end_send(true);
        assert_eq!(dest.append_all_from(&mut clean), Err(StatusCode::BadType));
    }

    #[test]
    fn parcel_is_send_and_sync() {
        // The send state is an atomic, not a `Cell`, so these auto traits survive.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Parcel>();
    }
}
