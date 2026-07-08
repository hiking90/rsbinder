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
    sys::{binder_uintptr_t, BINDER_TYPE_FD},
    thread_state,
};

const STRICT_MODE_PENALTY_GATHER: i32 = 1 << 31;

#[inline]
pub(crate) fn pad_size(len: usize) -> usize {
    (len + 3) & (!3)
}

/// Compute `(size, padded)` for a wire-encoded array of `len` elements
/// each of `elem_size` bytes, returning `BadValue` if either
/// calculation would overflow `usize`.
///
/// Used by `Parcel::read_array` / `Parcel::read_array_char` to keep
/// 32-bit targets (armv7 Android, i686 Linux) safe from a hostile
/// `len * size_of::<D>()` wrap that the subsequent
/// `padded > data_avail()` check would otherwise miss — the wrap-to-
/// small `size` would let the call through to
/// `Vec::with_capacity(len)` and abort with a capacity-overflow panic
/// (i.e. a remote DoS via parcel input).
///
/// Caller must have validated `len >= 1` already; passing `len < 1`
/// is a programming error (the result `size` would be `0` which is
/// indistinguishable from a wrapped-to-zero overflow, so we reject
/// it as `BadValue` via the `debug_assert!`).
///
/// On 64-bit `usize` no realistic `i32` `len` and `elem_size` (which
/// for any Rust type is at most `isize::MAX = 2^63 - 1`) can make
/// the multiplication overflow — the worst-case product is far
/// below `usize::MAX`. The protection is purely 32-bit-target
/// hardening; the 64-bit codepath is byte-identical to the unchecked
/// arithmetic.
#[inline]
pub(crate) fn checked_array_layout(len: i32, elem_size: usize) -> Result<(usize, usize)> {
    debug_assert!(
        len >= 1,
        "checked_array_layout: caller must validate len >= 1"
    );
    let size = (len as usize)
        .checked_mul(elem_size)
        .ok_or(StatusCode::BadValue)?;
    // `pad_size` itself can overflow at `size + 3`, so go through
    // `checked_add` rather than reusing it directly.
    let padded = size.checked_add(3).ok_or(StatusCode::BadValue)? & !3;
    Ok((size, padded))
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
    Slice(&'static mut [T]),
}

impl<T: Clone + Default> ParcelData<T> {
    fn new() -> Self {
        ParcelData::Vec(Vec::new())
    }

    fn with_capacity(capacity: usize) -> Self {
        ParcelData::Vec(Vec::with_capacity(capacity))
    }

    fn from_vec(data: Vec<T>) -> Self {
        ParcelData::Vec(data)
    }

    /// # Safety
    ///
    /// If `data` is non-null, it must be valid for reads and writes of
    /// `len` `T`-aligned elements, exclusively owned for the lifetime of
    /// the returned `ParcelData::Slice` (until the surrounding `Parcel`
    /// is dropped or its `free_buffer` runs). `data == null` with
    /// `len == 0` is the only well-defined null case.
    ///
    /// For an empty IPC parcel the binder driver still allocates a buffer
    /// and returns its user-space address; that address must be returned
    /// verbatim in BC_FREE_BUFFER, so we cannot collapse `len == 0` to a
    /// dangling `&mut []`. Only fall back to `&mut []` when `data` itself
    /// is null.
    unsafe fn from_raw_parts_mut(data: *mut T, len: usize) -> Self {
        ParcelData::Slice(if data.is_null() {
            debug_assert_eq!(len, 0, "non-zero length with null data is invalid");
            &mut []
        } else {
            // SAFETY: caller upholds the `# Safety` contract — `data` is
            // non-null, properly aligned, and valid for `len` elements
            // exclusively owned for the parcel's lifetime.
            unsafe { std::slice::from_raw_parts_mut(data, len) }
        })
    }

    fn as_slice(&self) -> &[T] {
        match self {
            ParcelData::Vec(v) => v.as_slice(),
            ParcelData::Slice(s) => s,
        }
    }

    fn as_mut_slice(&mut self) -> &mut [T] {
        // The `Slice` variant already holds a `&'static mut [T]` —
        // exclusive, mutable, and granted by the constructor
        // (`from_raw_parts_mut`, only called on a kernel-supplied
        // transaction buffer the driver explicitly allows the
        // userspace to modify in place, e.g. for FD-cookie / handle
        // patches inside `Parcel::append_from`). The previous
        // `panic!()` arm was a latent crash: it would have fired on
        // any `append_from` of a kernel-incoming parcel whose
        // destination was Slice-backed, since `objects.as_mut_slice()`
        // and `data.as_mut_slice()` flow through this method. Mirror
        // the `as_slice` arm and just hand the slice back; the
        // compiler reborrows it to the lifetime of `&mut self`.
        match self {
            ParcelData::Vec(v) => v.as_mut_slice(),
            ParcelData::Slice(s) => s,
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
            ParcelData::Slice(s) => s.as_mut_ptr(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn set_len(&mut self, len: usize) {
        match self {
            // SAFETY: Vec::set_len requires `len <= capacity` and that the
            // first `len` bytes are initialized. Element type is `u8`, so any
            // byte pattern is a valid value; every caller reserves capacity
            // and writes the bytes (copy_nonoverlapping) before calling this,
            // so the caller must uphold `len <= capacity`.
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
            _ => panic!("extend_from_slice() is only available for ParcelData::Vec."),
        }
    }
}

pub type FnFreeBuffer =
    fn(Option<&Parcel>, binder_uintptr_t, usize, binder_uintptr_t, usize) -> Result<()>;

/// RPC object-marshalling hooks attached to an RPC-mode `Parcel`
/// (the rsbinder equivalent of android's `Parcel::mSession`/`RpcState`).
///
/// `parcel.rs` only knows this trait; the implementation lives in the
/// `rpc` module's session/state. When a `Parcel` is in RPC mode the
/// `SIBinder` (de)serializers route through these hooks instead of the
/// kernel `flat_binder_object` path — the kernel path is byte-identical
/// when `is_for_rpc == false`.
#[cfg(feature = "rpc")]
pub trait RpcParcelOps: Send + Sync {
    /// Marshal a possibly-null binder leaving this process: append the
    /// r34 RPC object encoding (`i32` present flag + 32B address).
    fn write_binder(
        &self,
        binder: Option<&crate::binder::SIBinder>,
        parcel: &mut Parcel,
    ) -> Result<()>;
    /// Unmarshal a binder entering this process from the RPC encoding.
    fn read_binder(&self, parcel: &mut Parcel) -> Result<Option<crate::binder::SIBinder>>;
}

/// All RPC-mode serialization state for a [`Parcel`], bundled into one
/// struct (AOSP `Parcel.h`'s `RpcFields`, the RPC arm of its
/// `std::variant<KernelFields, RpcFields> mVariantFields`). Unlike
/// AOSP we need no `KernelFields`: rsbinder keeps the kernel offset
/// table in [`Parcel::objects`], a wholly separate field, so only the
/// RPC arm has to be bundled.
///
/// A `Parcel` carries this as `Option<RpcFields>`: `Some` ⇒ RPC mode
/// (the former `is_for_rpc == true`), `None` ⇒ kernel path,
/// byte-identical to the kernel wire. Tying every RPC field's
/// existence to the mode flag in the type makes "RPC mode ⇒ RPC state
/// present" an invariant the compiler enforces, instead of seven
/// independently-defaulted fields gated on a separate bool.
#[cfg(feature = "rpc")]
#[derive(Default)]
struct RpcFields {
    /// Object-marshalling hooks for RPC mode (android `mSession`
    /// equivalent). `Some` only on an RPC-mode parcel that will carry
    /// binders.
    ops: Option<std::sync::Arc<dyn RpcParcelOps>>,
    /// Negotiated FD-over-RPC mode. Default `None` ⇒ FD writes are
    /// rejected, bit-identical to a parcel that carries no FDs.
    fd_mode: crate::rpc::FileDescriptorTransportMode,
    /// FDs collected while serializing this (outgoing) RPC parcel in
    /// `Unix` fd-mode — sent out-of-band via `SCM_RIGHTS`.
    fds_out: Vec<std::os::fd::OwnedFd>,
    /// FDs received out-of-band with this (incoming) RPC parcel,
    /// indexed by the in-body fd-table index.
    fds_in: Vec<Option<std::os::fd::OwnedFd>>,
    /// AOSP `RpcFields::mObjectPositions` — sorted byte offsets of
    /// flattened RPC objects (binder at android-16 v2, FD at v1+),
    /// produced by `write_binder` / FD-write while serializing an
    /// RPC-mode parcel and consumed by the wire codec as the trailing
    /// `u32[]` object table. The kernel path never touches it (kernel
    /// objects live in [`Parcel::objects`]); empty ⇒ byte-identical to
    /// a wire with no object table.
    object_positions: Vec<u32>,
    /// Whether an FD flattened into this parcel records its position
    /// in `object_positions`. The session sets this from its wire
    /// profile alongside the FD mode: `true` only on the android-13+
    /// v1+ profile (R34 has no object table). Binder positions are
    /// recorded by the session directly (it owns the profile); only
    /// the FD path needs this Parcel-side flag.
    record_fd_positions: bool,
    /// Session addresses of local binders that bumped their `timesSent`
    /// (`RpcState::on_binder_leaving`) while being flattened into this
    /// outgoing parcel. If the send then fails, the session rolls each back
    /// (`cancel_binder_leaving`) so the unreceived binder's node does not leak.
    /// Write-only on the success path (the peer's DEC balances the bumps), so
    /// the wire is byte-unchanged.
    leaving_addrs: Vec<crate::rpc::RpcAddress>,
}

/// The behaviour of the RPC serialization state lives here so that the
/// `impl Parcel` accessors stay thin `Option`-lifting wrappers and the
/// real logic is unit-cohesive on `RpcFields`. These are private to
/// `parcel.rs`: callers always go through the matching `Parcel::rpc_*`
/// method (which decides the kernel-mode no-op / default), keeping each
/// `&mut self` borrow short enough to interleave with `Parcel::write`.
#[cfg(feature = "rpc")]
impl RpcFields {
    /// Record an RPC object's start offset, keeping the table sorted
    /// (AOSP `mObjectPositions.insert(upper_bound(...), dataPos)`).
    /// `upper_bound` ⇒ O(1) push on the common ascending-write path.
    fn record_object_position(&mut self, pos: usize) {
        let pos = pos as u32;
        let at = self.object_positions.partition_point(|&p| p <= pos);
        self.object_positions.insert(at, pos);
    }

    /// AOSP `unflattenBinder` v2 strict check: the position must be in
    /// the (sorted) object table (`binary_search`). A forged/unsorted
    /// table simply misses ⇒ caller returns `BAD_VALUE`.
    fn object_position_present(&self, pos: usize) -> bool {
        let Ok(pos) = u32::try_from(pos) else {
            return false;
        };
        self.object_positions.binary_search(&pos).is_ok()
    }

    /// Stash an outgoing fd (already an owned dup); return its in-body
    /// table index.
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
}

/// Maximum [`Parcel::sized_read`] nesting depth. Bounds recursion through
/// self-referential parcelables so a hostile deeply-nested payload cannot
/// overflow the worker-thread stack. Set far above any legitimate AIDL
/// nesting; conforming traffic never reaches it.
const MAX_NESTED_READ_DEPTH: usize = 1000;

/// Parcel converts data into a byte stream (serialization), making it transferable.
/// The receiving side then transforms this byte stream back into its original data form (deserialization).
///
/// A `Parcel` is the fundamental data container for binder IPC, handling serialization
/// and deserialization of primitive types, strings, objects, and file descriptors.
/// It maintains proper alignment and object reference tracking required by the binder protocol.
pub struct Parcel {
    data: ParcelData<u8>,
    pub(crate) objects: ParcelData<binder_size_t>,
    pos: usize,
    next_object_hint: usize,
    /// End offset of the innermost active [`Parcel::sized_read`] block;
    /// `None` when not inside one (⇒ [`Parcel::has_more_data`] uses the
    /// full buffer length). Lets a version-N reader stop at the parcelable
    /// boundary written by a version-M (< N) peer instead of reading into
    /// trailing bytes — the stable-AIDL forward-compatibility read path,
    /// paired with the per-field `has_more_data()` guards emitted in
    /// generated `read_from_parcel`.
    read_boundary: Option<usize>,
    /// Current [`Parcel::sized_read`] nesting depth. A self-referential
    /// parcelable (e.g. AIDL `RecursiveList`) recurses through `sized_read`
    /// on read, so a hostile deeply-nested payload would overflow the
    /// worker-thread stack (a hard `SIGABRT`, not a recoverable error).
    /// Capped at [`MAX_NESTED_READ_DEPTH`]; see [`Parcel::sized_read`].
    nested_read_depth: usize,
    request_header_present: bool,
    work_source_request_header_pos: usize,
    free_buffer: Option<FnFreeBuffer>,
    /// RPC serialization state, or `None` for the kernel path
    /// (byte-identical to the kernel wire). `Some` is the former
    /// `is_for_rpc == true`. Only object marshalling and the
    /// object/FD lifetime branch on this; scalar/string/POD paths are
    /// unaffected. See [`RpcFields`].
    #[cfg(feature = "rpc")]
    rpc: Option<RpcFields>,
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
            #[cfg(feature = "rpc")]
            rpc: None,
        }
    }

    /// # Safety
    /// - `data` must be valid for reads/writes of `length` bytes, or null if `length` is 0
    /// - `objects` must be valid for reads/writes of `object_count` elements, or null if `object_count` is 0
    /// - The memory must remain valid until the Parcel is dropped or `free_buffer` is called
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
            data: ParcelData::from_raw_parts_mut(data, length),
            objects: ParcelData::from_raw_parts_mut(objects, object_count),
            pos: 0,
            next_object_hint: 0,
            read_boundary: None,
            nested_read_depth: 0,
            request_header_present: false,
            work_source_request_header_pos: 0,
            free_buffer: Some(free_buffer),
            #[cfg(feature = "rpc")]
            rpc: None,
        }
    }

    pub fn from_vec(data: Vec<u8>) -> Self {
        Parcel {
            data: ParcelData::from_vec(data),
            objects: ParcelData::new(),
            pos: 0,
            next_object_hint: 0,
            read_boundary: None,
            nested_read_depth: 0,
            // objects: ptr::null_mut(),
            // object_count: 0,
            request_header_present: false,
            work_source_request_header_pos: 0,
            free_buffer: None,
            #[cfg(feature = "rpc")]
            rpc: None,
        }
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// Switch this parcel between the kernel and RPC serialization
    /// modes. Default is kernel mode; only object
    /// marshalling and the object/FD lifetime branch on this — scalar,
    /// string and POD bytes are identical in both modes.
    #[cfg(feature = "rpc")]
    pub fn set_for_rpc(&mut self, yes: bool) {
        if yes {
            // Idempotent: preserve any RpcFields already configured
            // (e.g. via a prior `attach_rpc_ops`).
            self.rpc.get_or_insert_with(RpcFields::default);
        } else {
            self.rpc = None;
        }
    }

    /// `true` if this parcel serializes binders/FDs the RPC way
    /// (`RpcAddress` instead of `flat_binder_object`; FD rejected).
    #[cfg(feature = "rpc")]
    pub fn is_for_rpc(&self) -> bool {
        self.rpc.is_some()
    }

    /// `false` always — without the `rpc` feature there is no RPC
    /// serialization mode. This `cfg`-off arm exists so callers that
    /// branch on transport (notably
    /// [`crate::permission_controller::check_permission`], which denies
    /// `@EnforcePermission` over RPC — Plan 2-16 Phase A) compile in any
    /// feature configuration without a `cfg` of their own.
    #[cfg(not(feature = "rpc"))]
    pub fn is_for_rpc(&self) -> bool {
        false
    }

    /// Attach the RPC object-marshalling hooks and enter RPC mode
    /// (android `Parcel::markForRpc`/`mSession` equivalent).
    #[cfg(feature = "rpc")]
    pub fn attach_rpc_ops(&mut self, ops: std::sync::Arc<dyn RpcParcelOps>) {
        self.rpc.get_or_insert_with(RpcFields::default).ops = Some(ops);
    }

    /// Configure the session's RPC profile on this parcel in one call:
    /// enter RPC mode + attach the object hooks, then stamp the
    /// negotiated FD transport mode and position-recording flag.
    /// Collapses the `attach_rpc_ops` + `set_rpc_fd_mode` +
    /// `set_rpc_record_fd_positions` triple that every proxy/session
    /// parcel-setup site otherwise repeats verbatim. The per-message
    /// `rpc_set_in_fds` / `rpc_set_object_positions` stay separate —
    /// they carry wire payload, not the session profile.
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

    /// The sorted object-position table (AOSP `mObjectPositions`),
    /// consumed by the wire codec as a trailing `u32[]`. Always empty
    /// on the kernel path (`!is_for_rpc`) and when no RPC object was
    /// flattened — i.e. byte-identical to a wire with no object table.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_object_positions(&self) -> &[u32] {
        self.rpc
            .as_ref()
            .map_or(&[], |r| r.object_positions.as_slice())
    }

    /// Install the object table that arrived with an incoming RPC
    /// parcel (AOSP `rpcSetDataReference`'s `mObjectPositions` copy),
    /// so the binder/FD deserializers can validate object positions.
    /// No-op kernel-side (`!is_for_rpc`).
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_set_object_positions(&mut self, positions: Vec<u32>) {
        if let Some(rpc) = self.rpc.as_mut() {
            rpc.object_positions = positions;
        }
    }

    /// Record that flattening a local binder into this outgoing RPC parcel
    /// bumped its `timesSent` for the address `addr`, so a later send failure
    /// can roll the bump back. No-op kernel-side.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_record_leaving_addr(&mut self, addr: crate::rpc::RpcAddress) {
        if let Some(rpc) = self.rpc.as_mut() {
            rpc.leaving_addrs.push(addr);
        }
    }

    /// The local-binder addresses whose `timesSent` this parcel bumped while
    /// serializing (see [`Parcel::rpc_record_leaving_addr`]). Empty kernel-side
    /// or when no local binder was flattened.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_leaving_addrs(&self) -> &[crate::rpc::RpcAddress] {
        self.rpc
            .as_ref()
            .map_or(&[], |r| r.leaving_addrs.as_slice())
    }

    /// AOSP `Parcel::unflattenBinder` v2 strict check: a binder may
    /// only be read from a position that is in the object table
    /// (`std::binary_search(mObjectPositions, objectPos)`). The table
    /// arrives sorted from a conformant peer; an unsorted/forged table
    /// simply fails the search ⇒ the caller returns `BAD_VALUE` (safe).
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_object_position_present(&self, pos: usize) -> bool {
        self.rpc
            .as_ref()
            .is_some_and(|r| r.object_position_present(pos))
    }

    /// Record the start offset of an RPC object just flattened into
    /// this parcel, keeping the table sorted (AOSP
    /// `Parcel::flattenBinder`/`writeFileDescriptor`:
    /// `mObjectPositions.insert(upper_bound(...), dataPos)`).
    ///
    /// **Hard-gated on `is_for_rpc`**: a stray call on a kernel parcel
    /// is a no-op, so the kernel wire can never grow an object table.
    /// The producer only calls this from the RPC `write_binder` /
    /// FD-write paths, and the caller decides *whether* to record
    /// (binder ⇒ v2 only; FD ⇒ v1+), faithfully mirroring AOSP's
    /// per-call version gate.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_record_object_position(&mut self, pos: usize) {
        if let Some(rpc) = self.rpc.as_mut() {
            rpc.record_object_position(pos);
        }
    }

    // ---- FD-over-RPC (opt-in, Unix mode) ---------------------------

    /// Set the negotiated FD-over-RPC mode for this parcel (default
    /// `None` ⇒ FD write is rejected, bit-identical).
    #[cfg(feature = "rpc")]
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

    /// Set by the session from its wire profile (alongside the FD
    /// mode): record FD object positions only on the android-13+ v1+
    /// profile. R34 stays `false` ⇒ the FD-over-RPC wire is
    /// byte-unchanged.
    #[cfg(feature = "rpc")]
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

    /// Stash an outgoing fd (already an owned dup) and return its
    /// in-body table index. Called by `ParcelFileDescriptor::serialize`
    /// only in `Unix` fd-mode.
    #[cfg(feature = "rpc")]
    pub(crate) fn rpc_push_out_fd(&mut self, fd: std::os::fd::OwnedFd) -> i32 {
        // Only ever reached from the RPC `Unix` fd-write path (guarded
        // by `is_for_rpc()` upstream), so RPC mode is a hard
        // precondition — a stray kernel-parcel call is a programming
        // error, and returning a bogus index would be worse than a
        // clear panic.
        self.rpc
            .as_mut()
            .expect("rpc_push_out_fd on kernel parcel")
            .push_out_fd(fd)
    }

    /// Borrow the collected outgoing fds. The session sends them
    /// out-of-band via `SCM_RIGHTS`; the parcel keeps ownership and
    /// closes them on drop (after the send completes — the peer
    /// already has its own dup'd copies via the kernel).
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

    pub fn set_data_size(&mut self, new_len: usize) -> Result<()> {
        if new_len > self.data.capacity() {
            // The backing buffer cannot hold `new_len` bytes — a broken
            // driver/buffer contract. Refuse rather than enter the
            // `Vec::set_len` UB of claiming uninitialized capacity.
            log::error!(
                "set_data_size({new_len}) exceeds capacity {}",
                self.data.capacity()
            );
            return Err(StatusCode::BadValue);
        }
        self.data.set_len(new_len);
        if new_len < self.pos {
            self.pos = new_len;
        }
        Ok(())
    }

    pub fn close_file_descriptors(&self) {
        // RPC-mode parcels never carry kernel FD objects (FD over RPC
        // is rejected by default / opt-in via Unix mode); nothing to close here.
        #[cfg(feature = "rpc")]
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

    pub fn set_data_position(&mut self, pos: usize) {
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
        // `pos` can legitimately be moved past `len` (set_data_position is
        // unbounded), so saturate instead of underflow-panicking: nothing
        // is available once the cursor is at/after the end.
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
        // The kernel offset-table scan below is meaningless for an
        // RPC-mode parcel (RPC carries `RpcAddress`, not
        // `flat_binder_object`). Reaching here in RPC mode is a
        // protocol error, not a silent mis-read.
        #[cfg(feature = "rpc")]
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
    /// nesting is capped at [`MAX_NESTED_READ_DEPTH`]; a payload exceeding it
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

        // Bound `has_more_data()` to this block while the closure runs, so a
        // newer reader stops at a shorter (older-peer) parcelable's end
        // rather than reading trailing bytes. Saved/restored to support
        // nested parcelables.
        let prev_boundary = self.read_boundary;
        self.read_boundary = Some(end);
        let result = f(self);
        self.read_boundary = prev_boundary;
        self.nested_read_depth -= 1;
        result?;

        // Advance the data position to the actual end,
        // in case the closure read less data than was available
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

    pub(crate) fn read_array<D: Deserialize>(&mut self) -> Result<Option<Vec<D>>> {
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

        // Checked arithmetic — protects 32-bit `usize` targets (armv7
        // Android, i686 Linux) from a hostile `len * size_of::<D>()`
        // wrapping past `usize::MAX`. Without these guards, a wrap-to-
        // small `size` would sail past the `padded > data_avail()`
        // check below and only fail later inside `Vec::with_capacity`
        // (capacity-overflow panic = DoS). On 64-bit `usize` the
        // multiplication is mathematically incapable of overflowing
        // for any `i32` `len`, so the new path is identical there.
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

        // SAFETY: We have verified bounds through data_slice.get()
        // - data_slice is a valid slice of exactly `size` bytes
        // - result has capacity for `len` elements
        // - copy_nonoverlapping copies exactly `size` bytes
        // - setting length to `len` is valid as we just initialized those elements
        let mut result = Vec::with_capacity(len as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(
                data_slice.as_ptr(),
                result.as_mut_ptr() as *mut u8,
                size,
            );
            result.set_len(len as usize);
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

        // See `read_array` — checked arithmetic guards 32-bit `usize`
        // against a hostile `len` wrapping past `usize::MAX` before
        // the `padded > data_avail()` check can catch it. Wire
        // element size is always 4 (i32) for the char-array codecs,
        // matching the `let size = len * 4` shape that lived here
        // before this hardening.
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
        // The parcel's `Vec<u8>` has only 1-byte base alignment, so
        // `align_to::<i32>()` would silently drop a mis-aligned prefix
        // (data corruption, not UB). Copy each 4-byte element out by
        // value instead — `size == len * 4` exactly (checked above), so
        // `chunks_exact` yields exactly `len` elements with no remainder.
        let result = self.data.as_slice()[pos..pos + size]
            .chunks_exact(std::mem::size_of::<i32>())
            .map(|c| D::from(&i32::from_ne_bytes([c[0], c[1], c[2], c[3]])))
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
        // No `len <= data_avail()` cap here. Unlike an `in` array, an
        // `out`/`inout` vec sends only its *length* in the parcel —
        // the elements are produced by the callee, not read from the
        // bytes that follow — so `len > data_avail()` is the normal,
        // valid case. Capping it here would regress every out-array
        // AIDL method on the live kernel binder. The unbounded-`len`
        // OOM concern is real but must be bounded by a configured
        // maximum, not by `data_avail()` (Android libbinder's
        // `resizeOutVector` is likewise unbounded).
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
            // See `resize_out_vec`: an out-vec length is not backed by
            // parcel data, so no `data_avail()` cap here.
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

    pub(crate) fn write_array<S: Serialize + Sized>(&mut self, parcelable: &[S]) -> Result<()> {
        let len = parcelable.len();
        self.write::<i32>(&(len as _))?;

        if len == 0 {
            return Ok(());
        }

        let size = std::mem::size_of_val(parcelable);
        let padded = pad_size(size);
        let pos = self.pos;

        self.data.reserve(pos + padded);
        // SAFETY: `reserve(pos + padded)` above guarantees the destination
        // has at least `pos + padded` bytes of capacity, so `add(pos)` and
        // the `size`-byte copy (size <= padded) stay in-bounds and the
        // ranges do not overlap (distinct allocations). The 0-3 trailing pad
        // bytes are then zeroed so that `set_len` exposes only initialized
        // memory: `reserve` allocates but does not initialize, and the pad is
        // transmitted (it is counted in `data_size`), so without this we would
        // both hit UB and leak uninitialized process memory to the peer. AOSP
        // masks the pad to zero as well. `set_len` only grows up to the
        // just-reserved capacity over now-initialized `u8` bytes.
        unsafe {
            // Zero any `[len..pos]` gap left by a forward `set_data_position`
            // before `set_len`, so an uninitialized hole is never exposed via
            // `as_slice()` / transmitted to the peer (UB + info-leak). See
            // `write_aligned_data` for the full rationale.
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
            if self.data.len() < pos + padded {
                self.data.set_len(pos + padded);
            }
        }

        self.set_data_position(pos + padded);

        Ok(())
    }

    pub(crate) fn write_array_char<S: CharType>(&mut self, parcelable: &[S]) -> Result<()> {
        let len = parcelable.len();
        self.write::<i32>(&(len as _))?;

        let size = 4 * len;
        let padded = pad_size(size);

        self.data.reserve(self.pos + padded);
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

    pub(crate) fn write_aligned<T>(&mut self, val: &T) {
        let unaligned = std::mem::size_of::<T>();
        // SAFETY: `val` is a live `&T`, so its `size_of::<T>()` bytes are
        // valid to read as `u8` for the borrow's duration. The resulting
        // slice does not outlive `val` (consumed synchronously below).
        let val_bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(val as *const T as *const u8, unaligned) };

        self.write_aligned_data(val_bytes);
    }

    pub(crate) fn write_aligned_data(&mut self, data: &[u8]) {
        let unaligned = data.len();
        let aligned = pad_size(unaligned);
        let pos = self.pos;

        self.data.reserve(pos + aligned);
        // SAFETY: `reserve(pos + aligned)` guarantees capacity for `add(pos)`
        // and the `unaligned`-byte copy (unaligned <= aligned). Source `data`
        // and the parcel buffer are distinct allocations (non-overlapping).
        // The 0-3 trailing pad bytes are zeroed before `set_len`: `reserve`
        // does not initialize, and the pad is transmitted (counted in
        // `data_size`), so leaving it uninitialized is both UB and an
        // info-leak to the peer. AOSP masks the pad to zero too. `set_len`
        // only grows up to the reserved capacity over now-initialized `u8`.
        unsafe {
            // If the write cursor sits past the current end (a forward
            // `set_data_position`), the skipped `[len..pos]` bytes were never
            // initialized; zero them before `set_len` marks the region
            // initialized, otherwise `as_slice()` / `data_size()` would expose
            // uninitialized heap to the peer (UB + info-leak). AOSP `growData`
            // zero-fills grown capacity the same way.
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
            if pos + aligned > self.data.len() {
                self.data.set_len(pos + aligned);
            }
        }

        self.set_data_position(pos + aligned);
    }

    pub(crate) fn write_object(&mut self, obj: &flat_binder_object, null_meta: bool) -> Result<()> {
        // RPC mode never carries `flat_binder_object`s: binders are
        // marshalled as `RpcAddress` and FDs are rejected upstream.
        // Write the bytes verbatim but never load the kernel offset
        // table or take a kernel `acquire()` — RPC has its own
        // refcount. The kernel path below is byte-identical when
        // `is_for_rpc == false`.
        #[cfg(feature = "rpc")]
        if self.rpc.is_some() {
            self.write_aligned(obj);
            return Ok(());
        }

        let data_pos = self.pos;
        self.write_aligned(obj);

        if null_meta || obj.pointer() != 0 {
            obj.acquire()?;
            self.objects.push(data_pos as _);
        }

        Ok(())
    }

    pub(crate) fn write_interface_token(&mut self, interface: &str) -> Result<()> {
        self.write(&(thread_state::get_strict_mode_policy() | STRICT_MODE_PENALTY_GATHER))?;
        self.update_work_source_request_header_pos();
        let work_source: i32 = if thread_state::should_propagate_work_source() {
            thread_state::calling_work_source_uid() as _
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
        let other_len = other.data_size();
        if offset > other_len || size > other_len || (offset + size) > other_len {
            log::error!("Parcel::append_from: The given offset({offset}) and size({size}) exceed the data range of the parcel.");
            return Err(StatusCode::BadValue);
        }

        let start_pos = self.pos;
        let mut first_idx: i32 = -1;
        let mut last_idx: i32 = -2;
        {
            let object_size = std::mem::size_of::<flat_binder_object>() as u64;
            // Scan the SOURCE parcel's object table (mirrors AOSP
            // Parcel.cpp::appendFrom, which iterates `other`'s mObjects),
            // not `self.objects`. The destination may be empty (e.g. a fresh
            // ParcelableHolder parcel), in which case using `self.objects`
            // would compute num_objects == 0 and silently drop every
            // binder/FD object nested in the copied range.
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

        self.data.reserve(self.pos + size);
        // SAFETY: the source range `other.data[offset..offset + size]` is
        // bounds-checked by the slice index above (panics if out of range),
        // and `reserve(self.pos + size)` guarantees the destination has
        // capacity for `add(self.pos)` plus `size` bytes. `other` and `self`
        // are distinct parcels (non-overlapping). `set_len` only grows up to
        // the reserved capacity over the `u8` bytes just copied.
        unsafe {
            // Zero any `[len..pos]` gap from a forward `set_data_position`
            // before `set_len` so an uninitialized hole is never exposed /
            // transmitted (UB + info-leak). See `write_aligned_data`.
            let old_len = self.data.len();
            if self.pos > old_len {
                std::ptr::write_bytes(self.data.as_mut_ptr().add(old_len), 0, self.pos - old_len);
            }
            std::ptr::copy_nonoverlapping::<u8>(
                other.data.as_slice()[offset..offset + size].as_ptr(),
                self.data.as_mut_ptr().add(self.pos),
                size,
            );
            if self.pos + size > self.data.len() {
                self.data.set_len(self.pos + size);
            }
        }
        self.set_data_position(self.pos + size);

        // An RPC-mode parcel carries no `flat_binder_object`s, so the
        // offset recompute / re-acquire / FD-dup below is kernel-only.
        // `self.objects` is already empty in RPC mode so this is also
        // defence-in-depth.
        #[cfg(feature = "rpc")]
        let skip_objects = self.rpc.is_some();
        #[cfg(not(feature = "rpc"))]
        let skip_objects = false;

        if num_objects > 0 && !skip_objects {
            self.objects.reserve(num_objects as usize);

            // Recompute each offset from the SOURCE table position
            // (`other.objects`). AOSP: `off = pos - offset + startPos`.
            //
            // Commit the relocated offset into `self.objects` only AFTER the
            // object is fully acquired and (for FDs) dup'd + rewritten with the
            // destination's own fd. If `acquire()` or the FD dup fails, the
            // offset is never committed, so this parcel's `Drop`
            // (`release_objects`) only ever iterates fully-acquired entries.
            // Committing first (as before) left a half-built entry that still
            // held the SOURCE's fd/handle bytes; on the drop that follows the
            // `?` it would `release()` the source's still-owned fd
            // (double-close) or decrement a refcount that was never incremented
            // (underflow). AOSP's `appendFrom` is double-close-safe because it
            // never aborts the loop; this push-after-success ordering achieves
            // the same invariant by construction.
            let src_objects = other.objects.as_slice();
            for i in first_idx..=last_idx {
                let off = src_objects[i as usize] as usize - offset + start_pos;
                let mut flat = read_flat_binder(self.data.as_slice(), off)?;
                flat.acquire()?;
                if flat.header_type() == BINDER_TYPE_FD {
                    let newfd = match rustix::io::fcntl_dupfd_cloexec(flat.borrowed_fd(), 0) {
                        Ok(newfd) => newfd,
                        Err(e) => {
                            // FD `acquire()` is a no-op, so nothing to undo; the
                            // source's fd at `off` stays owned by the source
                            // parcel (no double-close). Offset not committed.
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

    fn release_objects(&self) {
        // An RPC-mode parcel must never run kernel `release()` /
        // `decref_publish` — RPC objects have a different
        // (DecStrong-based) lifetime. `self.objects` is empty in RPC
        // mode anyway; this is defence-in-depth + intent.
        #[cfg(feature = "rpc")]
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
                // Never panic in Drop: a failure here may run during unwind,
                // and a double-panic aborts the whole process — strictly
                // worse than logging and leaking the kernel buffer.
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
                self.release_objects();
            }
        }
    }
}

impl std::fmt::Debug for Parcel {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        writeln!(f, "Parcel: pos {}, len {}", self.pos, self.data.len())?;
        if self.objects.len() > 0 {
            // SAFETY: `self.objects` is a live `Vec<binder_size_t>`, so its
            // `len * size_of::<binder_size_t>()` bytes are a valid contiguous
            // region readable as `u8`. The slice is consumed synchronously by
            // `pretty_hex` and does not outlive the borrow of `self.objects`.
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

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn parcel_data_slice_as_mut_slice_round_trips() {
        // The `Slice` variant carries a `&'static mut [T]` produced
        // by `ParcelData::from_raw_parts_mut`, which is the
        // kernel-buffer adoption path (`Parcel::from_ipc_parts`
        // family). Pre-fix this method panicked unconditionally on
        // that arm — a latent crash on `Parcel::append_from` of any
        // kernel-incoming parcel whose destination was Slice-backed.
        // This regression guard mirrors the `as_slice` symmetry: a
        // write through `as_mut_slice` is observable on the next
        // `as_slice` read.
        //
        // SAFETY: `Box::leak` hands ownership to the test binary's
        // process-lifetime arena, so the pointer + length below
        // satisfy `from_raw_parts_mut`'s "valid, exclusively-owned,
        // `'static`" contract for the rest of the run.
        let leaked: &'static mut [u8] = Box::leak(vec![0u8, 1, 2, 3].into_boxed_slice());
        let ptr = leaked.as_mut_ptr();
        let len = leaked.len();
        let mut pd: super::ParcelData<u8> =
            unsafe { super::ParcelData::from_raw_parts_mut(ptr, len) };

        let slice = pd.as_mut_slice();
        assert_eq!(slice, &mut [0u8, 1, 2, 3][..]);
        slice[0] = 99;
        slice[3] = 200;

        // Round-trip through the immutable view: writes survived,
        // proving the returned `&mut [T]` actually aliases the
        // underlying storage (not a copy).
        assert_eq!(pd.as_slice(), &[99u8, 1, 2, 200][..]);
    }

    #[test]
    fn write_array_zeroes_trailing_pad() {
        // A byte array whose length is not a multiple of 4 leaves 1-3
        // trailing pad bytes inside the (4-byte aligned) parcel slot. That
        // pad is part of the transmitted payload (it is counted in
        // `data_size`), so it must be zeroed — matching AOSP — otherwise
        // `reserve`+`set_len` would mark uninitialized/leftover memory as
        // initialized (UB) and leak it to the peer.
        let mut parcel = Parcel::new();
        // Poison the first 8 bytes with 0xFF so a missing zero-fill is
        // observable as leftover bytes rather than incidental zeros.
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
        // `i32::MAX × usize::MAX` overflows on every target — covers
        // the 32-bit DoS surface the helper exists to seal. (The
        // direct `Parcel::read_array` callers can't actually hit
        // this on 64-bit since `size_of::<D>()` for any real Rust
        // type is bounded by `isize::MAX`, but the helper itself is
        // generic over `elem_size: usize`.)
        assert_eq!(
            super::checked_array_layout(i32::MAX, usize::MAX),
            Err(StatusCode::BadValue)
        );
    }

    #[test]
    fn checked_array_layout_rejects_pad_overflow() {
        // `size + 3` itself overflows `usize` when `size > usize::MAX - 3`.
        // Hand-craft an `elem_size` that makes the multiplication land
        // exactly at `usize::MAX` so `pad_size` is the failing arm.
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
    fn checked_array_layout_64bit_no_op_for_i32_max() {
        // The whole point: on a 64-bit `usize`, no realistic
        // `(i32::MAX, size_of::<D>())` can overflow. Regression
        // guard so the helper never starts gating valid 64-bit
        // inputs (which would silently break every kernel/RPC array
        // path).
        let (size, padded) = super::checked_array_layout(i32::MAX, 4).unwrap();
        assert_eq!(size, (i32::MAX as usize) * 4);
        assert_eq!(padded, super::pad_size(size));
    }

    #[test]
    fn read_array_rejects_hostile_len_gracefully() {
        // A parcel whose data only encodes the length — no actual
        // array body — with a `len` far larger than `data_avail()`.
        // Pre-hardening this could panic in `Vec::with_capacity` on
        // 32-bit; post-hardening it returns `Err(_)` on every target.
        let mut parcel = Parcel::new();
        parcel.write::<i32>(&1_000_000_000).unwrap();
        parcel.set_data_position(0);
        let r = parcel.read_array::<i32>();
        assert!(r.is_err(), "expected Err, got {r:?}");
    }

    #[test]
    fn read_array_char_rejects_hostile_len_gracefully() {
        // Same shape as `read_array_rejects_hostile_len_gracefully`
        // but exercises the char-array variant — both call into
        // `checked_array_layout`.
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

    // #[test]
    // fn test_dyn_ibinder() -> Result<()> {
    //     let proxy: Arc<Box<dyn IBinder>> = Arc::new(proxy::Proxy::new_unknown(0));
    //     let raw = Arc::into_raw(proxy.clone());

    //     let mut parcel = Parcel::new();

    //     {
    //         parcel.write(&raw)?;
    //     }
    //     parcel.set_data_position(0);

    //     let cloned = proxy.clone();
    //     {
    //         let restored = parcel.read::<*const dyn IBinder>()?;

    //         assert_eq!(raw, restored);
    //         assert_eq!(Arc::strong_count(&cloned), Arc::strong_count(&unsafe {Arc::from_raw(restored)}));
    //     }

    //     Ok(())
    // }

    #[test]
    fn test_errors() -> Result<()> {
        Ok(())
    }

    // E8: typed scalar helpers must round-trip and stay wire-identical to the
    // generic read::<T>/write::<T> path they wrap.
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

    // Regression test for issue #97 (BC_FREE_BUFFER no match).
    //
    // When an IPC reply has data_size == 0 (e.g. a successful `void` AIDL
    // method), the binder driver still allocates a buffer and returns its
    // user-space address in `binder_transaction_data.data.ptr.buffer`. The
    // receiver must echo that exact address back via `BC_FREE_BUFFER`. If
    // `from_raw_parts_mut` collapsed the zero-length case to `&mut []` it
    // would discard the kernel-supplied pointer and replace it with the
    // empty-slice dangling pointer (0x1 for u8), causing the kernel to log
    // `BC_FREE_BUFFER no match for buffer at offset ...001` on every empty
    // reply. This test asserts the original pointer survives both the
    // construction call and a Drop that funnels it back to free_buffer.
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
            // SAFETY: `original` points to a valid allocation; `len == 0` exercises
            // the regression path. `objects` is null with object_count == 0,
            // exercising the preserved null guard.
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
        // Preserves the null-pointer guard from commit bae39ec: a null `data`
        // with `len == 0` is allowed (used elsewhere) and must not invoke
        // `slice::from_raw_parts_mut` with a null pointer.
        fn noop(
            _: Option<&Parcel>,
            _: crate::sys::binder::binder_uintptr_t,
            _: usize,
            _: crate::sys::binder::binder_uintptr_t,
            _: usize,
        ) -> Result<()> {
            Ok(())
        }

        // SAFETY: both pointers are null with length 0 — the documented
        // empty-parcel case for `from_ipc_parts`. No reads or writes
        // happen against the null pointers in the rest of this test.
        let parcel = unsafe {
            Parcel::from_ipc_parts(std::ptr::null_mut(), 0, std::ptr::null_mut(), 0, noop)
        };
        // Empty slice fallback — pointer is the dangling NonNull but no UB.
        assert_eq!(parcel.data_size(), 0);
        drop(parcel);
    }

    // Hardening regression: `set_data_size` must reject a length larger
    // than the backing buffer's capacity instead of entering the
    // `Vec::set_len` UB of claiming uninitialized capacity. A broken
    // driver/buffer contract is the untrusted-input source here.
    #[test]
    fn set_data_size_rejects_over_capacity() {
        let mut parcel = Parcel::new();
        let cap = parcel.capacity();

        // Exactly at capacity is the boundary and must succeed.
        assert!(parcel.set_data_size(cap).is_ok());
        // One past capacity must be refused with BadValue, not panic/UB.
        assert_eq!(parcel.set_data_size(cap + 1), Err(StatusCode::BadValue));
        // Zero is always valid.
        assert!(parcel.set_data_size(0).is_ok());
    }

    // Hardening regression: `data_avail` must saturate when the cursor
    // has been moved past the end (`set_data_position` is unbounded).
    // The pre-fix `len - pos` underflowed and panicked in debug builds
    // on attacker-influenced positions.
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

    /// Stable-AIDL forward-compat read path: a reader expecting more fields
    /// than a shorter (older-peer) parcelable carries must leave the trailing
    /// fields at their default — driven by `has_more_data()` respecting the
    /// `sized_read` block boundary — and a reader expecting fewer fields than
    /// a longer (newer-peer) parcelable must skip the extra bytes cleanly.
    #[test]
    fn sized_read_field_truncation_via_has_more_data() {
        // A "V1" writer emits a length-prefixed parcelable of two i32s.
        let mut wv1 = Parcel::new();
        wv1.sized_write(|p| {
            p.write(&11i32)?;
            p.write(&22i32)
        })
        .expect("v1 write");
        // Trailing sentinel after the parcelable (reply-trailer analogue) so
        // an over-read past the block boundary would be detectable.
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

        // Reverse direction: a "V3" writer emits three i32s; a "V1" reader
        // expecting two must skip the extra field and land on the sentinel.
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

    /// A self-referential parcelable (AIDL `RecursiveList`) recurses through
    /// `sized_read` on read, so a hostile deeply-nested payload must be
    /// rejected with `BadValue` at [`MAX_NESTED_READ_DEPTH`] rather than
    /// recursing until the worker-thread stack overflows (a hard abort).
    /// Also proves the depth counter is decremented on both the success and
    /// error paths, so it never leaks across successive reads.
    #[test]
    fn sized_read_depth_is_bounded() {
        // Mirrors a generated `RecursiveList` write: each node is a sized
        // block holding a marker plus (optionally) the next node.
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

        // A legitimate shallow nesting still reads cleanly, twice — the second
        // read only succeeds if the counter was restored on the way out of the
        // first (and on the error unwind above).
        let mut ok = Parcel::new();
        write_nested(&mut ok, 8).expect("write shallow");
        for _ in 0..2 {
            ok.set_data_position(0);
            read_nested(&mut ok).expect("shallow read");
        }
    }

    /// The RPC object-position table is collected AOSP-faithfully: the
    /// recorded offset is the position of the object's leading int32
    /// (AOSP `dataPos = mDataPos` *before* `writeInt32(TYPE_*)`), the
    /// table stays **sorted** (AOSP `mObjectPositions.insert(upper_bound(...),
    /// dataPos)`) even when objects are recorded out of order, it is
    /// hard-gated on `is_for_rpc` (kernel diff 0), and it
    /// survives a v2 codec encode→decode with the AOSP `bodySize =
    /// fixed + parcelDataSize + 4·N` framing. Single / multiple /
    /// mixed (binder-shaped + FD-shaped) objects.
    #[cfg(feature = "rpc")]
    #[test]
    fn rpc_object_position_table_is_aosp_faithful_and_sorted() {
        use crate::rpc::wire::{WireCodec, WireMessage, WireTransaction};
        use crate::rpc::wire_android13::Android13PlusCodec;

        // ---- kernel parcel: recording is a hard no-op ----
        let mut kparcel = Parcel::new();
        kparcel.write(&7i32).unwrap();
        kparcel.rpc_record_object_position(0); // !is_for_rpc ⇒ ignored
        assert!(
            kparcel.rpc_object_positions().is_empty(),
            "kernel parcel must never grow an object table"
        );

        // ---- RPC parcel: AOSP-faithful flatten sequence ----
        let mut p = Parcel::new();
        p.set_for_rpc(true);
        p.set_rpc_record_fd_positions(true);

        // Interface-token-like prefix, then a mix of objects and
        // scalars. Each "object" mirrors the android-16 wire body:
        //   binder: [i32 present=1][8B RpcWireAddress][i32 stability]
        //   fd    : [i32 present=1][i32 ancillary-index]
        // The position recorded is the offset of the leading int32,
        // captured *before* it is written (AOSP `Parcel::flattenBinder`
        // / `writeFileDescriptor`).
        p.write(&0xDEAD_BEEFu32).unwrap(); // token-ish
        p.write(&"iface".to_owned()).unwrap(); // a String arg

        let mut expect: Vec<u32> = Vec::new();

        // binder #1
        let pos = p.data_position();
        expect.push(pos as u32);
        p.write(&1i32).unwrap(); // present/TYPE_BINDER
        p.write_aligned_data(&[0u8; 8]); // 8B RpcWireAddress
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
        p.write_aligned_data(&[0u8; 8]);
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

        // AOSP `mObjectPositions.insert(upper_bound(...), dataPos)`:
        // a late out-of-order record still lands sorted.
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

        // The v2 strict-receive `binary_search` primitive
        // (`Parcel::unflattenBinder`): a recorded position passes, an
        // unrecorded one fails ⇒ `read_binder` returns BAD_VALUE.
        for &good in &[0u32, 8, 24, 40] {
            assert!(q.rpc_object_position_present(good as usize), "pos {good}");
        }
        for bad in [4usize, 12, 41, 9999] {
            assert!(
                !q.rpc_object_position_present(bad),
                "unrecorded pos {bad} must fail the v2 binary_search"
            );
        }

        // ---- v2 codec framing: positions survive encode→decode and
        //      bodySize = 40 + parcelDataSize + 4·N (AOSP RpcState) ----
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
