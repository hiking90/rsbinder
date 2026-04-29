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

//! Thread-local binder state management.
//!
//! This module manages the per-thread state for binder operations, including
//! transaction context, reference counting, and communication with the binder driver.
//! Each thread participating in binder IPC maintains its own state through this module.
//!
//! # Borrow-Discipline Invariant (R1)
//!
//! `THREAD_STATE` and `BINDER_DEREFS` are `RefCell`s. Their `borrow*()` guards
//! must NOT be held across calls that may re-borrow either cell. The set of
//! calls that may re-borrow includes:
//!
//!   - User-code entry points: `Transactable::transact`, `Inner<T>::drop`
//!     (invoked via `deref_native_kernel`), and
//!     `DeathRecipient::binder_died`.
//!   - Other functions in this module that take a borrow at some point:
//!     `transact`, `wait_for_response`, `flush_commands`, `flash_if_needed`,
//!     `inc_strong_handle`, `dec_strong_handle`, `inc_weak_handle`,
//!     `dec_weak_handle`, `free_buffer`, and `process_pending_derefs`.
//!
//! Violation manifests as a `RefCell` panic ("already borrowed" /
//! "already mutably borrowed"). Binder's nested-IPC protocol means user
//! callbacks routinely make outgoing binder calls from inside an incoming
//! `BR_TRANSACTION`, so this is not a theoretical concern.
//!
//! ## Patterns to satisfy R1
//!
//! - **P2 — Minimal scope**: scope each borrow as tightly as "read value →
//!   process → write value". Drop the borrow before calling out, then
//!   re-acquire a fresh borrow afterwards. Don't span the entire function
//!   body inside one `borrow_mut()`.
//! - **P3 — Stack-save**: when a user callback is about to be invoked, save
//!   and restore logical state (e.g. `transaction`) via local variables on
//!   the call stack, not via persisted `RefCell` borrows. Persisting state
//!   in a `RefCell` borrow across a callback risks dragging the borrow into
//!   re-entrant code.
//!
//! See `b17d522` for the regression that motivated the explicit P2 split in
//! `process_pending_derefs`.

use log::error;
use std::backtrace::Backtrace;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::fmt::Debug;
use std::fs::File;
use std::sync::{atomic::Ordering, Arc};

use crate::{binder::*, error::*, parcel::*, process_state::*, sys::*};

// See module doc — R1: borrows of these `RefCell`s must not be held across
// calls that may re-borrow either cell (user callbacks, other binder entry
// points). Use the P2 (minimal scope) and P3 (stack-save) patterns.
thread_local! {
    static THREAD_STATE: RefCell<ThreadState> = RefCell::new(ThreadState::new());
    static BINDER_DEREFS: RefCell<BinderDerefs> = RefCell::new(BinderDerefs::new());
}

const RETURN_STRINGS: [&str; 21] = [
    "BR_ERROR",
    "BR_OK",
    "BR_TRANSACTION",
    "BR_REPLY",
    "BR_ACQUIRE_RESULT",
    "BR_DEAD_REPLY",
    "BR_TRANSACTION_COMPLETE",
    "BR_INCREFS",
    "BR_ACQUIRE",
    "BR_RELEASE",
    "BR_DECREFS",
    "BR_ATTEMPT_ACQUIRE",
    "BR_NOOP",
    "BR_SPAWN_LOOPER",
    "BR_FINISHED",
    "BR_DEAD_BINDER",
    "BR_CLEAR_DEATH_NOTIFICATION_DONE",
    "BR_FAILED_REPLY",
    "BR_FROZEN_REPLY",
    "BR_ONEWAY_SPAM_SUSPECT",
    "BR_TRANSACTION_SEC_CTX",
];

fn return_to_str(cmd: std::os::raw::c_uint) -> &'static str {
    if cmd == binder::BR_TRANSACTION_SEC_CTX {
        "BR_TRANSACTION_SEC_CTX"
    } else {
        let idx: usize = (cmd & binder::_IOC_NRMASK) as _;

        if idx < RETURN_STRINGS.len() {
            RETURN_STRINGS[idx]
        } else {
            "Unknown BR_ return"
        }
    }
}

const COMMAND_STRINGS: [&str; 19] = [
    "BC_TRANSACTION",
    "BC_REPLY",
    "BC_ACQUIRE_RESULT",
    "BC_FREE_BUFFER",
    "BC_INCREFS",
    "BC_ACQUIRE",
    "BC_RELEASE",
    "BC_DECREFS",
    "BC_INCREFS_DONE",
    "BC_ACQUIRE_DONE",
    "BC_ATTEMPT_ACQUIRE",
    "BC_REGISTER_LOOPER",
    "BC_ENTER_LOOPER",
    "BC_EXIT_LOOPER",
    "BC_REQUEST_DEATH_NOTIFICATION",
    "BC_CLEAR_DEATH_NOTIFICATION",
    "BC_DEAD_BINDER_DONE",
    "BC_TRANSACTION_SG",
    "BC_REPLY_SG",
];

fn command_to_str(cmd: std::os::raw::c_uint) -> &'static str {
    let idx: usize = (cmd & 0xFF) as _;

    if idx < COMMAND_STRINGS.len() {
        COMMAND_STRINGS[idx]
    } else {
        "Unknown BC_ command"
    }
}

const WORK_SOURCE_PROPAGATED_BIT_INDEX: i64 = 32;
pub(crate) const UNSET_WORK_SOURCE: i32 = -1;

#[derive(Debug, Clone, Copy)]
struct TransactionState {
    calling_pid: binder::pid_t,
    calling_sid: *const u8,
    calling_uid: binder::uid_t,
    // strict_mode_policy: i32,
    last_transaction_binder_flags: u32,
    work_source: binder::uid_t,
    propagate_work_source: bool,
}

impl TransactionState {
    fn from_transaction_data(data: &binder::binder_transaction_data_secctx) -> Self {
        TransactionState {
            calling_pid: data.transaction_data.sender_pid,
            calling_sid: data.secctx as _,
            calling_uid: data.transaction_data.sender_euid,
            // strict_mode_policy: 0,
            last_transaction_binder_flags: data.transaction_data.flags,
            work_source: 0,
            propagate_work_source: false,
        }
    }
}

// Storage for inbound BR_RELEASE / BR_DECREFS payloads — process-monotonic
// u64 ids delivered by the kernel for native binders we previously
// published. Processed lazily on the next driver round-trip via
// `process_pending_derefs`.
//
// Under the new id-encoding model (replacing the fat-pointer scheme that
// could dangle when `Inner<T>` was dropped between BR_RELEASE and
// BR_DECREFS), the pending entries are pure ids — `deref_native_kernel`
// looks them up in `ProcessState::published_natives` and drives
// `kernel_refs--` / entry-removal-on-zero. No SIBinder reconstruction
// from a raw pointer, no method dispatch on a possibly-freed `Inner<T>`.
//
// Outbound proxy ref-count (BC_ACQUIRE / BC_RELEASE / BC_INCREFS /
// BC_DECREFS) is not routed through this state — proxies own kernel
// strong refs 1-per-Arc (acquire in `ProxyHandle::new_acquired`, release
// in `ProxyHandle::Drop`) and the cache pin owns the kernel weak ref.
struct BinderDerefs {
    pending_strong_derefs: VecDeque<u64>,
    pending_weak_derefs: VecDeque<u64>,
}

impl BinderDerefs {
    fn new() -> Self {
        BinderDerefs {
            pending_strong_derefs: VecDeque::new(),
            pending_weak_derefs: VecDeque::new(),
        }
    }
}

/// Drain `BINDER_DEREFS` of pending BR_RELEASE / BR_DECREFS ids.
///
/// Each `deref_native_kernel(id)` call may, when an entry's counters
/// both reach zero, trigger `remove_entry_if_zero` which drops the
/// canonical `Arc<dyn IBinder>` and synchronously fires
/// `Inner<T>::drop` on the user's `Remotable` instance. A user
/// destructor that initiates an outgoing synchronous IPC ends up in
/// `wait_for_response` → `talk_with_driver` → `execute_command`,
/// whose BR_RELEASE / BR_DECREFS arms try to push back into
/// `BINDER_DEREFS` via a fresh `borrow_mut()`.
///
/// To make that re-entrancy safe, this function never holds the
/// `BINDER_DEREFS` `RefCell` borrow across a `deref_native_kernel`
/// call. It alternates between:
///
///   1. Acquire the borrow, take the entire weak queue (or pop one
///      strong id), release the borrow.
///   2. Dispatch outside the borrow.
///
/// Pushes from re-entrant BR handlers go into a fresh `borrow_mut`,
/// and the outer loop picks them up on the next iteration.
///
/// Order matches Android's libbinder: drain ALL weak derefs before
/// dispatching the next strong, so a strong-deref destructor that
/// queues a weak-deref gets drained ahead of the next pending
/// strong. FIFO within each queue (`VecDeque::pop_front` /
/// `mem::take` preserves insertion order).
///
/// # Borrow discipline (R1)
///
/// Must be called with NO `THREAD_STATE` or `BINDER_DEREFS` borrow held
/// — `deref_native_kernel` may invoke user `Inner<T>::drop`, which can
/// re-enter the binder stack. The acquire/release alternation inside this
/// function maintains R1; do not hoist the borrow out of the loop.
fn process_pending_derefs() -> Result<()> {
    loop {
        // Inner loop: drain weak fully. Re-take after each batch
        // because dispatch may push more weak entries (from
        // `Inner<T>::drop` running user destructor code that
        // synchronously triggers another BR_DECREFS).
        loop {
            let batch: VecDeque<u64> =
                BINDER_DEREFS.with(|d| std::mem::take(&mut d.borrow_mut().pending_weak_derefs));
            if batch.is_empty() {
                break;
            }
            for id in batch {
                if ProcessState::as_self().deref_native_kernel(id).is_none() {
                    log::trace!("BR_DECREFS for unknown native id {id}");
                }
            }
        }

        // Pop exactly one strong id under a fresh borrow, then
        // dispatch outside the borrow. If none, both queues are
        // empty (the inner loop just confirmed weak is empty), so
        // we're done. Re-checking weak after dispatch is handled by
        // looping back to the inner loop above.
        let id = BINDER_DEREFS.with(|d| d.borrow_mut().pending_strong_derefs.pop_front());
        match id {
            Some(id) => {
                if ProcessState::as_self().deref_native_kernel(id).is_none() {
                    log::trace!("BR_RELEASE for unknown native id {id}");
                }
            }
            None => return Ok(()),
        }
    }
}

pub(crate) struct ThreadState {
    in_parcel: Parcel,
    out_parcel: Parcel,
    transaction: Option<TransactionState>,
    strict_mode_policy: i32,
    is_looper: bool,
    is_flushing: bool,
    call_restriction: CallRestriction,
    driver: Arc<File>,
}

impl ThreadState {
    fn new() -> Self {
        ThreadState {
            in_parcel: Parcel::new(),
            out_parcel: Parcel::new(),
            transaction: None,
            strict_mode_policy: 0,
            is_looper: false,
            is_flushing: false,
            call_restriction: ProcessState::as_self().call_restriction(),
            driver: ProcessState::as_self().driver(),
        }
    }

    pub(crate) fn set_strict_mode_policy(&mut self, policy: i32) {
        self.strict_mode_policy = policy;
    }

    pub(crate) fn _strict_mode_policy(&self) -> i32 {
        self.strict_mode_policy
    }

    pub(crate) fn last_transaction_binder_flags(&self) -> u32 {
        match self.transaction {
            Some(tr) => tr.last_transaction_binder_flags,
            None => 0,
        }
    }

    fn is_process_pending_derefs(&mut self) -> bool {
        self.in_parcel.data_position() >= self.in_parcel.data_size()
    }

    fn clear_propagate_work_source(&mut self) {
        if let Some(ref mut state) = self.transaction {
            state.propagate_work_source = false;
        }
    }

    fn clear_calling_work_source(&mut self) {
        self.set_calling_work_source_uid(UNSET_WORK_SOURCE as _);
    }

    fn set_calling_work_source_uid(&mut self, uid: binder::uid_t) -> i64 {
        let token = self.set_calling_work_source_uid_without_propagation(uid);
        if let Some(ref mut state) = self.transaction {
            state.propagate_work_source = true;
        }
        token
    }

    pub(crate) fn set_calling_work_source_uid_without_propagation(
        &mut self,
        uid: binder::uid_t,
    ) -> i64 {
        match self.transaction {
            Some(ref mut state) => {
                let propagated_bit =
                    (state.propagate_work_source as i64) << WORK_SOURCE_PROPAGATED_BIT_INDEX;
                let token = propagated_bit | (state.work_source as i64);
                state.work_source = uid;

                token
            }
            None => 0,
        }
    }

    fn write_transaction_data(
        &mut self,
        cmd: u32,
        mut flags: u32,
        handle: u32,
        code: u32,
        data: &Parcel,
        status: &i32,
    ) -> Result<()> {
        log::trace!(
            "write_transaction_data: {} {flags:X} {handle} {code}\n{:?}",
            command_to_str(cmd),
            data
        );
        // ptr is initialized by zero because ptr(64) and handle(32) size is different.
        let mut target = binder_transaction_data__bindgen_ty_1 { ptr: 0 };
        target.handle = handle;

        // let all_flags: u32 = FLAG_PRIVATE_VENDOR | FLAG_CLEAR_BUF | FLAG_ONEWAY;
        // if (flags & !all_flags) != 0 {
        //     log::error!("Unrecognized flags sent: {:X}", flags);
        // }
        let tr = if *status == StatusCode::Ok.into() {
            binder_transaction_data {
                target,
                cookie: 0,
                code,
                flags,
                sender_pid: 0,
                sender_euid: 0,
                data_size: data.data_size() as _,
                offsets_size: (data.objects.len() * std::mem::size_of::<binder_size_t>()) as _,
                data: binder_transaction_data__bindgen_ty_2 {
                    ptr: binder_transaction_data__bindgen_ty_2__bindgen_ty_1 {
                        buffer: data.as_ptr() as _,
                        offsets: data.objects.as_ptr() as _,
                    },
                },
            }
        } else {
            flags |= binder::transaction_flags_TF_STATUS_CODE;
            binder_transaction_data {
                target,
                cookie: 0,
                code,
                flags,
                sender_pid: 0,
                sender_euid: 0,
                data_size: std::mem::size_of::<i32>() as _,
                offsets_size: 0,
                data: binder_transaction_data__bindgen_ty_2 {
                    ptr: binder_transaction_data__bindgen_ty_2__bindgen_ty_1 {
                        buffer: status as *const i32 as _,
                        offsets: 0,
                    },
                },
            }
        };

        self.out_parcel.write::<u32>(&cmd)?;
        self.out_parcel.write_aligned(&tr);

        Ok(())
    }
}

pub(crate) fn set_call_restriction(call_restriction: CallRestriction) {
    THREAD_STATE.with(|thread_state| {
        thread_state.borrow_mut().call_restriction = call_restriction;
    })
}

pub(crate) fn call_restriction() -> CallRestriction {
    THREAD_STATE.with(|thread_state| thread_state.borrow().call_restriction)
}

pub(crate) fn strict_mode_policy() -> i32 {
    THREAD_STATE.with(|thread_state| thread_state.borrow().strict_mode_policy)
}

pub(crate) fn should_propagate_work_source() -> bool {
    THREAD_STATE.with(|thread_state| {
        thread_state
            .borrow()
            .transaction
            .is_some_and(|state| state.propagate_work_source)
    })
}

pub(crate) fn calling_work_source_uid() -> binder::uid_t {
    THREAD_STATE.with(|thread_state| {
        thread_state
            .borrow()
            .transaction
            .map_or(0, |state| state.work_source)
    })
}

pub(crate) fn _setup_polling() -> Result<()> {
    THREAD_STATE.with(|thread_state| -> Result<()> {
        thread_state
            .borrow_mut()
            .out_parcel
            .write::<u32>(&binder::BC_ENTER_LOOPER)
    })?;
    flush_commands()?;
    Ok(())
}

enum UntilResponse {
    Reply,
    TransactionComplete,
    /// Inbound `BR_ACQUIRE_RESULT` arm. Currently unreachable because
    /// `BC_ATTEMPT_ACQUIRE` is no longer issued by rsbinder — under the
    /// cache-pin model, regular `BC_ACQUIRE` always succeeds (the cache
    /// pin keeps the slot alive) and `Weak<I>::upgrade` covers the
    /// "atomically promote a weak ref" semantics. The variant is kept
    /// only so the kernel-direction match in `wait_for_response` stays
    /// exhaustive.
    #[allow(dead_code)]
    AcquireResult,
}

fn wait_for_response(until: UntilResponse) -> Result<Option<Parcel>> {
    THREAD_STATE.with(|thread_state| -> Result<Option<Parcel>> {
        loop {
            talk_with_driver(true)?;

            if thread_state.borrow().in_parcel.is_empty() {
                continue;
            }
            let cmd: u32 = thread_state.borrow_mut().in_parcel.read::<i32>()? as _;

            log::trace!("{:?}", return_to_str(cmd));

            match cmd {
                binder::BR_ONEWAY_SPAM_SUSPECT => {
                    log::error!("Process seems to be sending too many oneway calls.");
                    log::error!("{}", Backtrace::capture());

                    if let UntilResponse::TransactionComplete = until {
                        break;
                    }
                }
                binder::BR_TRANSACTION_COMPLETE => {
                    if let UntilResponse::TransactionComplete = until {
                        break;
                    }
                }
                binder::BR_DEAD_REPLY => {
                    return Err(StatusCode::DeadObject);
                }
                binder::BR_FAILED_REPLY => {
                    log::error!(
                        "Received FAILED_REPLY transaction reply for pid {}",
                        thread_state
                            .borrow()
                            .transaction
                            .map_or(0, |state| state.calling_pid)
                    );
                    return Err(StatusCode::FailedTransaction);
                }
                binder::BR_FROZEN_REPLY => {
                    log::error!(
                        "Received FROZEN_REPLY transaction reply for pid {}",
                        thread_state
                            .borrow()
                            .transaction
                            .map_or(0, |state| state.calling_pid)
                    );
                    return Err(StatusCode::FailedTransaction);
                }
                binder::BR_ACQUIRE_RESULT => {
                    let result = thread_state.borrow_mut().in_parcel.read::<i32>()?;
                    if let UntilResponse::AcquireResult = until {
                        let res = if result != 0 {
                            Ok(None)
                        } else {
                            Err(StatusCode::InvalidOperation)
                        };
                        return res;
                    } else if cfg!(debug_assertions) {
                        panic!("Unexpected BR_ACQUIRE_RESULT");
                    }
                }
                binder::BR_REPLY => {
                    let tr = thread_state
                        .borrow_mut()
                        .in_parcel
                        .read::<binder::binder_transaction_data>()?;
                    let (buffer, offsets) = unsafe { (tr.data.ptr.buffer, tr.data.ptr.offsets) };
                    if let UntilResponse::Reply = until {
                        if (tr.flags & transaction_flags_TF_STATUS_CODE) == 0 {
                            // SAFETY: buffer and offsets are valid pointers from binder driver
                            // transaction data, with sizes given by tr.data_size and tr.offsets_size
                            let reply = unsafe {
                                Parcel::from_ipc_parts(
                                    buffer as _,
                                    tr.data_size as _,
                                    offsets as _,
                                    (tr.offsets_size as usize)
                                        / std::mem::size_of::<binder::binder_size_t>(),
                                    free_buffer,
                                )
                            };
                            return Ok(Some(reply));
                        } else {
                            // SAFETY: Reading status code from binder transaction reply
                            // - We verify tr.data_size >= size_of::<i32>() before reading
                            // - buffer points to valid memory owned by binder driver
                            // - The data remains valid for the transaction lifetime
                            // - We convert to StatusCode immediately after reading
                            let status: StatusCode =
                                if tr.data_size >= std::mem::size_of::<i32>() as u64 {
                                    unsafe { (*(buffer as *const i32)).into() }
                                } else {
                                    log::error!(
                                        "Buffer too small for status code: {} < {}",
                                        tr.data_size,
                                        std::mem::size_of::<i32>()
                                    );
                                    StatusCode::BadValue
                                };
                            log::trace!("binder::BR_REPLY ({status})");
                            free_buffer(
                                None,
                                buffer,
                                tr.data_size as _,
                                offsets,
                                (tr.offsets_size as usize) / std::mem::size_of::<binder_size_t>(),
                            )?;

                            if status != StatusCode::Ok {
                                log::warn!("binder::BR_REPLY ({status})");
                                return Err(status);
                            }
                        }
                    } else {
                        free_buffer(
                            None,
                            buffer,
                            tr.data_size as _,
                            offsets,
                            (tr.offsets_size as usize) / std::mem::size_of::<binder_size_t>(),
                        )?;
                    }
                }
                _ => {
                    execute_command(cmd as _)?;
                }
            };
        }
        Ok(None)
    })
}

/// Drive the kernel handshake for `BR_DEAD_BINDER` so that a
/// user-side `send_obituary` failure cannot strand the kernel
/// `binder_ref` slot.
///
/// The kernel slot is leaked permanently if either:
///   1. `BC_DEAD_BINDER_DONE` is not written to `out_parcel`, or
///   2. `release_obituary_pin`'s `BC_DECREFS` is not flushed.
///
/// So phases 2 (`queue_done`) and 3 (`pin_release`) always run
/// regardless of phase 1's (`obituary`) outcome. The user-visible
/// obituary error takes priority over the pin-release error;
/// the pin error is also logged so it is not lost when both fail.
///
/// Residual edge: if `queue_done` itself fails (rare — `out_parcel`
/// is unhealthy under e.g. allocator OOM), phase 3 is skipped and the
/// pin leak still occurs. Documented; the next ioctl on this thread
/// will fail anyway.
fn drive_dead_binder_handshake<O, Q, P>(
    handle: binder::binder_uintptr_t,
    obituary: O,
    queue_done: Q,
    pin_release: P,
) -> Result<()>
where
    O: FnOnce() -> Result<()>,
    Q: FnOnce() -> Result<()>,
    P: FnOnce() -> Result<()>,
{
    // Phase 1: dispatch recipients. Capture the result; do NOT
    // short-circuit — kernel handshake must always complete.
    let obituary_result = obituary();

    // Phase 2: queue BC_DEAD_BINDER_DONE unconditionally. A failure
    // here means out_parcel is unhealthy; propagate immediately
    // because the next ioctl will surface it anyway. Phase 3 is
    // skipped in this rare path (acknowledged residual edge). Log
    // the obituary error first if it would otherwise be swallowed
    // by the queue error — the obituary diagnostic is most valuable
    // exactly when the kernel handshake is also breaking.
    if let Err(qe) = queue_done() {
        if let Err(oe) = &obituary_result {
            error!(
                "BR_DEAD_BINDER: queue BC_DEAD_BINDER_DONE failed ({qe:?}) \
                 swallowing obituary error {oe:?} for handle {handle:X}"
            );
        }
        return Err(qe);
    }

    // Phase 3: release the cache pin (BC_DECREFS via flush). Always
    // attempted, even if obituary errored. A pin-release error is
    // logged here so the diagnostic is not swallowed when the
    // obituary error is surfaced below.
    let pin_result = pin_release();
    if let Err(e) = &pin_result {
        error!(
            "release_obituary_pin failed for handle {handle:X}: {e:?}; \
             obituary_result: {obituary_result:?}"
        );
    }

    // Surface obituary error first (user-visible — death recipient
    // observed the failure). Fall back to pin error when obituary
    // succeeded but pin failed.
    obituary_result?;
    pin_result?;
    Ok(())
}

/// Invoke `Transactable::transact`, catching panics so a buggy
/// service handler cannot terminate the binder worker thread.
///
/// On panic, the partial reply (if any) is discarded and the caller
/// receives `StatusCode::Unknown`, so the existing `BR_TRANSACTION`
/// reply path synthesizes a deterministic error reply for the
/// calling client (rather than leaving the client hung waiting for
/// `BR_REPLY`). Mirrors the panic guard around `DeathRecipient`
/// callbacks in `ProxyHandle::dispatch_obituary_callbacks`. See the
/// `Transactable` trait doc for the full guarantee scope.
///
/// # Borrow discipline (R1)
///
/// Must be called with NO `THREAD_STATE` or `BINDER_DEREFS` borrow held —
/// `Transactable::transact` is user code that may issue nested binder
/// calls (re-entering this module). See module doc.
fn dispatch_transact_caught(
    transactable: &dyn Transactable,
    code: TransactionCode,
    reader: &mut Parcel,
    reply: &mut Parcel,
) -> Result<()> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        transactable.transact(code, reader, reply)
    }));
    match result {
        Ok(transact_result) => transact_result,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&'static str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<non-string panic payload>");
            error!("Transactable::transact panicked for code {code}: {msg}");
            // Discard any partially-written reply so the client does
            // not misparse half-formed data; the reply path below
            // turns `Err` into a clean error status.
            *reply = Parcel::new();
            Err(StatusCode::Unknown)
        }
    }
}

fn execute_command(cmd: i32) -> Result<()> {
    let cmd: std::os::raw::c_uint = cmd as _;

    THREAD_STATE.with(|thread_state| -> Result<()> {
        match cmd {
            binder::BR_ERROR => {
                let other: StatusCode = thread_state.borrow_mut().in_parcel.read::<i32>()?.into();
                log::error!("binder::BR_ERROR ({other})");
                return Err(other);
            }
            binder::BR_OK => {}

            binder::BR_TRANSACTION_SEC_CTX | binder::BR_TRANSACTION => {
                let tr_secctx = {
                    let mut thread_state = thread_state.borrow_mut();
                    if cmd == binder::BR_TRANSACTION_SEC_CTX {
                        thread_state
                            .in_parcel
                            .read::<binder::binder_transaction_data_secctx>()?
                    } else {
                        binder::binder_transaction_data_secctx {
                            transaction_data: thread_state
                                .in_parcel
                                .read::<binder::binder_transaction_data>()?,
                            secctx: 0,
                        }
                    }
                };

                let mut reader = unsafe {
                    let tr = &tr_secctx.transaction_data;

                    Parcel::from_ipc_parts(
                        tr.data.ptr.buffer as _,
                        tr.data_size as _,
                        tr.data.ptr.offsets as _,
                        (tr.offsets_size as usize) / std::mem::size_of::<binder::binder_size_t>(),
                        free_buffer,
                    )
                };

                // TODO: Skip now, because if below implmentation is mandatory.
                // const void* origServingStackPointer = mServingStackPointer;
                // mServingStackPointer = &origServingStackPointer; // anything on the stack

                let transaction_old = {
                    let mut thread_state = thread_state.borrow_mut();
                    let transaction_old = thread_state.transaction;

                    thread_state.clear_calling_work_source();
                    thread_state.clear_propagate_work_source();

                    thread_state.transaction =
                        Some(TransactionState::from_transaction_data(&tr_secctx));

                    transaction_old
                };

                let mut reply = Parcel::new();

                let result = {
                    let target_ptr = unsafe { tr_secctx.transaction_data.target.ptr };
                    if target_ptr != 0 {
                        // `target_ptr` is now the process-monotonic id
                        // assigned by `publish_native`. Resolve via the
                        // sidecar table (read-only, no count change) —
                        // the table guarantees `Inner<T>` is alive
                        // while the entry exists, so the SIBinder we
                        // construct here can safely drive the user's
                        // `Transactable` impl. SIBinder construction
                        // is contained within this scope and the
                        // RefCounter ops balance pairwise: `from_arc`
                        // calls `inc_strong` (+1), `attempt_increase`
                        // adds another (+1), `decrease()` cancels one
                        // (−1), and `strong`'s `Drop` cancels the
                        // last (−1) — net zero across the block.
                        let id = target_ptr;
                        match ProcessState::as_self().lookup_native(id) {
                            Some(arc) => {
                                let strong = SIBinder::from_arc(arc);
                                if strong.attempt_increase() {
                                    let result = dispatch_transact_caught(
                                        strong.as_transactable().expect("Transactable is None."),
                                        tr_secctx.transaction_data.code,
                                        &mut reader,
                                        &mut reply,
                                    );
                                    strong.decrease()?;
                                    result
                                } else {
                                    log::warn!("Failed strong.attempt_increase for native id {id}");
                                    Err(StatusCode::UnknownTransaction)
                                }
                            }
                            None => {
                                log::error!("BR_TRANSACTION for unknown native id {id}");
                                Err(StatusCode::DeadObject)
                            }
                        }
                    } else {
                        let context = ProcessState::as_self()
                            .context_manager()
                            .expect("Transactable is None.");
                        dispatch_transact_caught(
                            context.as_transactable().expect("Transactable is None."),
                            tr_secctx.transaction_data.code,
                            &mut reader,
                            &mut reply,
                        )
                    }
                };
                let flags = tr_secctx.transaction_data.flags;
                if (flags & transaction_flags_TF_ONE_WAY) == 0 {
                    let flags = flags & transaction_flags_TF_CLEAR_BUF;
                    let status: i32 = match result {
                        Ok(_) => StatusCode::Ok.into(),
                        Err(err) => err.into(),
                    };
                    thread_state.borrow_mut().write_transaction_data(
                        binder::BC_REPLY,
                        flags,
                        u32::MAX,
                        0,
                        &reply,
                        &status,
                    )?;
                    wait_for_response(UntilResponse::TransactionComplete)?;
                } else if let Err(err) = result {
                    let mut log = format!(
                        "oneway function results for code {} on binder at {:X}",
                        tr_secctx.transaction_data.code,
                        unsafe { tr_secctx.transaction_data.target.ptr }
                    );
                    log += &format!(" will be dropped but finished with status {err}");

                    if reply.data_size() != 0 {
                        log += &format!(" and reply parcel size {}", reply.data_size());
                    }
                    log::error!("{log}");
                }

                thread_state.borrow_mut().transaction = transaction_old;
            }

            binder::BR_INCREFS => {
                let mut state = thread_state.borrow_mut();
                let id = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                // The cookie half is unused under the new id encoding
                // but the kernel still emits the original `cookie`
                // (always 0 for our published natives). Echo it back
                // verbatim in BC_INCREFS_DONE.
                let cookie_echo = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                drop(state);

                // BR_INCREFS reflects the kernel acquiring a weak ref
                // to one of our published natives. Pure id
                // bookkeeping: bump `kernel_refs` in the table; the
                // RefCounter.weak alive-signal is held above zero by
                // `publish_native` for the entry's lifetime — no
                // per-event RefCounter touch needed (and no SIBinder
                // construction, no method dispatch on a possibly-freed
                // `Inner<T>`, closing the UAF that the old
                // fat-pointer encoding could expose).
                if ProcessState::as_self().ref_native_kernel(id).is_none() {
                    log::error!("BR_INCREFS for unknown native id {id}");
                    debug_assert!(false, "BR_INCREFS for unknown native id {id}");
                }

                let mut state = thread_state.borrow_mut();
                state.out_parcel.write::<u32>(&binder::BC_INCREFS_DONE)?;
                state.out_parcel.write::<binder::binder_uintptr_t>(&id)?;
                state
                    .out_parcel
                    .write::<binder::binder_uintptr_t>(&cookie_echo)?;
            }
            binder::BR_ACQUIRE => {
                let mut state = thread_state.borrow_mut();
                let id = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                let cookie_echo = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                drop(state);

                // Same shape as BR_INCREFS — bookkeeping only.
                // `RefCounter.strong` alive-signal is held by the
                // table's `binder_pin: SIBinder` for the entry's
                // lifetime.
                if ProcessState::as_self().ref_native_kernel(id).is_none() {
                    log::error!("BR_ACQUIRE for unknown native id {id}");
                    debug_assert!(false, "BR_ACQUIRE for unknown native id {id}");
                }

                let mut state = thread_state.borrow_mut();
                state.out_parcel.write::<u32>(&(binder::BC_ACQUIRE_DONE))?;
                state.out_parcel.write::<binder::binder_uintptr_t>(&id)?;
                state
                    .out_parcel
                    .write::<binder::binder_uintptr_t>(&cookie_echo)?;
            }
            binder::BR_RELEASE => {
                let mut state = thread_state.borrow_mut();
                let id = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                // cookie echo unused on the deferred-deref path.
                let _cookie_echo = state.in_parcel.read::<binder::binder_uintptr_t>()?;

                BINDER_DEREFS.with(|binder_derefs| {
                    let mut binder_derefs = binder_derefs.borrow_mut();
                    binder_derefs.pending_strong_derefs.push_back(id);
                });
            }
            binder::BR_DECREFS => {
                let mut state = thread_state.borrow_mut();
                let id = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                let _cookie_echo = state.in_parcel.read::<binder::binder_uintptr_t>()?;

                BINDER_DEREFS.with(|binder_derefs| {
                    let mut binder_derefs = binder_derefs.borrow_mut();
                    binder_derefs.pending_weak_derefs.push_back(id);
                });
            }
            binder::BR_ATTEMPT_ACQUIRE => {
                let mut state = thread_state.borrow_mut();
                let id = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                let _cookie_echo = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                drop(state);

                // Probe the table's binary alive-signal: entry exists
                // ⟹ promotion may proceed. If alive, bump
                // `kernel_refs` (the kernel will hold a new strong
                // ref on success). Unknown id is permitted here — it
                // can race against unpublish (kernel may probe a
                // binder that just lost its last ref); reply
                // `success=0` without `debug_assert`.
                let success = ProcessState::as_self().ref_native_kernel(id).is_some();

                let mut state = thread_state.borrow_mut();
                state.out_parcel.write::<u32>(&binder::BC_ACQUIRE_RESULT)?;
                state.out_parcel.write::<i32>(&(success as _))?;
            }
            binder::BR_NOOP => {}
            binder::BR_SPAWN_LOOPER => {
                ProcessState::as_self().spawn_pooled_thread(false);
            }
            binder::BR_FINISHED => {
                return Err(StatusCode::TimedOut);
            }
            binder::BR_DEAD_BINDER => {
                let handle = {
                    let mut state = thread_state.borrow_mut();
                    state.in_parcel.read::<binder::binder_uintptr_t>()?
                };

                log::trace!("BR_DEAD_BINDER: handle {handle:X}");

                drive_dead_binder_handshake(
                    handle,
                    || ProcessState::as_self().send_obituary_for_handle(handle as _),
                    || {
                        let mut state = thread_state.borrow_mut();
                        state
                            .out_parcel
                            .write::<u32>(&(binder::BC_DEAD_BINDER_DONE))?;
                        state
                            .out_parcel
                            .write::<binder::binder_uintptr_t>(&handle)?;
                        Ok(())
                    },
                    || ProcessState::as_self().release_obituary_pin(handle as _),
                )?;
            }
            binder::BR_CLEAR_DEATH_NOTIFICATION_DONE => {
                let mut state = thread_state.borrow_mut();
                state.in_parcel.read::<binder::binder_uintptr_t>()?;
            }
            _ => {
                log::error!("*** BAD COMMAND {cmd} received from Binder driver\n");
                return Err(StatusCode::Unknown);
            }
        };

        Ok(())
    })
}

/// Drive one round-trip with the binder kernel driver.
///
/// # Borrow discipline (H1, hygiene only)
///
/// The `BINDER_WRITE_READ` ioctl does not re-enter Rust on the same
/// thread (the kernel only schedules our incoming queue and returns),
/// so holding a `THREAD_STATE` borrow across the syscall does not
/// violate R1 today. The current code does hold an immutable
/// `thread_state.borrow()` across the `write_read` call to read
/// `driver` — correct under the no-re-entry property.
///
/// H1 is the hygiene note that this boundary should ideally be
/// tightened: future EINTR / signal-safety / cancellation handling
/// changes, or any logic that gains a same-thread Rust callback here,
/// would break the no-re-entry assumption and quietly turn this into
/// an R1 violation. A defensive refactor would clone the `Arc<File>`
/// out under a short borrow and pass it by value across the syscall.
/// Not done today (no concrete risk); recorded so a future change
/// knows to revisit.
fn talk_with_driver(do_receive: bool) -> Result<()> {
    THREAD_STATE.with(|thread_state| -> Result<()> {
        let mut bwr = {
            let mut thread_state = thread_state.borrow_mut();
            let need_read = thread_state.in_parcel.is_empty();
            let out_avail = if !do_receive || need_read {
                thread_state.out_parcel.data_size()
            } else {
                0
            };

            let read_size = if do_receive && need_read {
                thread_state.in_parcel.capacity()
            } else {
                0
            };

            binder::binder_write_read {
                write_size: out_avail as _,
                write_consumed: 0,
                write_buffer: thread_state.out_parcel.as_mut_ptr() as _,
                read_size: read_size as _,
                read_consumed: 0,
                read_buffer: thread_state.in_parcel.as_mut_ptr() as _,
            }
        };

        if bwr.write_size == 0 && bwr.read_size == 0 {
            return Ok(());
        }

        if bwr.write_size != 0 {
            log::trace!(
                "Sending command to driver:\n{:?}",
                thread_state.borrow().out_parcel
            );
            log::trace!(
                "Size of receive buffer: {}, need_read: {}, do_receive: {}",
                bwr.read_size,
                thread_state.borrow().in_parcel.is_empty(),
                do_receive
            );
        }
        // unsafe {
        //     loop {
        //         let res = binder::write_read(thread_state.borrow().driver.as_raw_fd(), &mut bwr);
        //         match res {
        //             Ok(_) => break,
        //             Err(errno) if errno != nix::errno::Errno::EINTR => {
        //                 log::error!("binder::write_read() error : {}", errno);
        //                 return Err(StatusCode::Errno(errno as _));
        //             },
        //             _ => {}
        //         }
        //     }
        // }

        loop {
            let res = binder::write_read(&thread_state.borrow().driver, &mut bwr);
            match res {
                Ok(_) => break,
                Err(errno) if errno != rustix::io::Errno::INTR => {
                    log::error!("binder::write_read() error : {errno}");
                    return Err(StatusCode::Errno(errno.raw_os_error()));
                }
                _ => {}
            }
        }

        log::trace!(
            "write consumed: {} of {}, read consumed: {} of {}",
            bwr.write_consumed,
            bwr.write_size,
            bwr.read_consumed,
            bwr.read_size
        );

        // Process write and read results in a single borrow_mut scope
        {
            let mut thread_state = thread_state.borrow_mut();

            if bwr.write_consumed > 0 {
                if bwr.write_consumed < thread_state.out_parcel.data_size() as _ {
                    panic!(
                        "Driver did not consume write buffer. consumed: {} of {}",
                        bwr.write_consumed,
                        thread_state.out_parcel.data_size()
                    );
                }
                thread_state.out_parcel.set_data_size(0);
            }

            if bwr.read_consumed > 0 {
                thread_state.in_parcel.set_data_size(bwr.read_consumed as _);
                thread_state.in_parcel.set_data_position(0);

                log::trace!(
                    "Received commands from driver:\n{:?}",
                    thread_state.in_parcel
                );
            }
        } // thread_state is dropped here

        Ok(())
    })
}

fn get_and_execute_command() -> Result<()> {
    talk_with_driver(true)?;

    let cmd = THREAD_STATE.with(|thread_state| -> Result<i32> {
        thread_state.borrow_mut().in_parcel.read::<i32>()
    })?;
    execute_command(cmd)?;

    Ok(())
}

pub(crate) fn flush_commands() -> Result<()> {
    talk_with_driver(false)?;

    THREAD_STATE.with(|thread_state| -> Result<()> {
        if thread_state.borrow().out_parcel.data_size() > 0 {
            talk_with_driver(false)?;
        }

        if thread_state.borrow().out_parcel.data_size() > 0 {
            log::warn!("self.out_parcel.len() > 0 after flash_commands()");
        }

        Ok(())
    })
}

pub(crate) fn inc_strong_handle(handle: u32) -> Result<()> {
    log::trace!("inc_strong_handle: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write::<u32>(&(binder::BC_ACQUIRE))?;
            state.out_parcel.write::<u32>(&(handle))?;
        }

        flash_if_needed()?;

        Ok(())
    })
}

pub(crate) fn dec_strong_handle(handle: u32) -> Result<()> {
    log::trace!("dec_strong_handle: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write::<u32>(&(binder::BC_RELEASE))?;
            state.out_parcel.write::<u32>(&(handle))?;
        }

        flash_if_needed()?;

        Ok(())
    })
}

pub(crate) fn inc_weak_handle(handle: u32) -> Result<()> {
    log::trace!("inc_weak_handle: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write::<u32>(&(binder::BC_INCREFS))?;
            state.out_parcel.write::<u32>(&(handle))?;
        }

        flash_if_needed()?;

        Ok(())
    })
}

pub(crate) fn dec_weak_handle(handle: u32) -> Result<()> {
    log::trace!("dec_weak_handle: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write::<u32>(&(binder::BC_DECREFS))?;
            state.out_parcel.write::<u32>(&(handle))?;
        }

        flash_if_needed()?;

        Ok(())
    })
}

pub(crate) fn flash_if_needed() -> Result<bool> {
    THREAD_STATE.with(|thread_state| -> Result<bool> {
        {
            let thread_state = thread_state.borrow();
            if thread_state.is_looper || thread_state.is_flushing {
                return Ok(false);
            }
        }

        thread_state.borrow_mut().is_flushing = true;
        flush_commands()?;
        thread_state.borrow_mut().is_flushing = false;

        Ok(true)
    })
}

pub(crate) fn _handle_commands() -> Result<()> {
    while {
        get_and_execute_command()?;

        THREAD_STATE.with(|thread_state| -> bool { !thread_state.borrow().in_parcel.is_empty() })
    } {
        flush_commands()?;
    }
    Ok(())
}

pub fn check_interface(reader: &mut Parcel, descriptor: &str) -> Result<bool> {
    let mut strict_policy: i32 = reader.read()?;

    THREAD_STATE.with(|thread_state| -> Result<()> {
        let mut thread_state = thread_state.borrow_mut();

        if (thread_state.last_transaction_binder_flags() & FLAG_ONEWAY) != 0 {
            strict_policy = 0;
        }
        thread_state.set_strict_mode_policy(strict_policy);
        reader.update_work_source_request_header_pos();

        let work_source: i32 = reader.read()?;
        thread_state.set_calling_work_source_uid_without_propagation(work_source as _);

        Ok(())
    })?;

    if crate::sdk_at_least(30) {
        let header: u32 = reader.read()?;
        if header != INTERFACE_HEADER {
            log::error!("Expecting header {INTERFACE_HEADER:#x} but found {header:#x}.");
            return Ok(false);
        }
    }

    let parcel_interface: String = reader.read()?;
    if parcel_interface.eq(descriptor) {
        Ok(true)
    } else {
        log::error!("check_interface() expected '{descriptor}' but read '{parcel_interface}'");
        Ok(false)
    }
}

pub(crate) fn transact(
    handle: u32,
    code: u32,
    data: &Parcel,
    mut flags: u32,
) -> Result<Option<Parcel>> {
    let mut reply: Option<Parcel> = None;

    flags |= transaction_flags_TF_ACCEPT_FDS;

    let call_restriction = THREAD_STATE.with(|thread_state| -> Result<CallRestriction> {
        let mut thread_state = thread_state.borrow_mut();
        thread_state.write_transaction_data(
            binder::BC_TRANSACTION,
            flags,
            handle,
            code,
            data,
            &0,
        )?;
        Ok(thread_state.call_restriction)
    })?;

    if (flags & transaction_flags_TF_ONE_WAY) == 0 {
        match call_restriction {
            CallRestriction::ErrorIfNotOneway => {
                error!("Process making non-oneway call (code: {code}) but is restricted.")
            }
            CallRestriction::FatalIfNotOneway => {
                panic!("Process may not make non-oneway calls (code: {code}).");
            }
            _ => (),
        }

        reply = wait_for_response(UntilResponse::Reply)?;
    } else {
        wait_for_response(UntilResponse::TransactionComplete)?;
    }

    Ok(reply)
}

fn free_buffer(
    parcel: Option<&Parcel>,
    data: binder_uintptr_t,
    _: usize,
    _: binder_uintptr_t,
    _: usize,
) -> Result<()> {
    if let Some(parcel) = parcel {
        parcel.close_file_descriptors()
    }

    THREAD_STATE.with(|thread_state| -> Result<()> {
        let mut thread_state = thread_state.borrow_mut();
        thread_state
            .out_parcel
            .write::<u32>(&binder::BC_FREE_BUFFER)?;
        thread_state.out_parcel.write::<binder_uintptr_t>(&data)?;
        Ok(())
    })?;

    flash_if_needed()?;

    Ok(())
}

pub(crate) fn query_interface(handle: u32) -> Result<String> {
    #[cfg(all(target_os = "android", feature = "android_10"))]
    if handle == 0 && !crate::sdk_at_least(30) {
        return Ok(crate::hub::android_10::SERVICE_MANAGER_DESCRIPTOR.to_owned());
    }

    let data = Parcel::new();
    let reply = transact(handle, INTERFACE_TRANSACTION, &data, 0)?;
    let interface: String = reply
        .expect("INTERFACE_TRANSACTION should have reply parcel")
        .read()?;

    Ok(interface)
}

pub(crate) fn ping_binder(handle: u32) -> Result<()> {
    let data = Parcel::new();
    let _reply = transact(handle, PING_TRANSACTION, &data, 0)?;
    Ok(())
}

pub(crate) fn join_thread_pool(is_main: bool) -> Result<()> {
    THREAD_STATE.with(|thread_state| -> Result<()> {
        log::debug!(
            "**** THREAD {:?} (PID {}) IS JOINING THE THREAD POOL",
            std::thread::current().id(),
            std::process::id()
        );

        ProcessState::as_self()
            .current_threads
            .fetch_add(1, Ordering::SeqCst);

        let looper = if is_main {
            binder::BC_ENTER_LOOPER
        } else {
            binder::BC_REGISTER_LOOPER
        };

        {
            let mut thread_state = thread_state.borrow_mut();
            thread_state.out_parcel.write::<u32>(&looper)?;
            thread_state.is_looper = true;
        }

        let result;

        loop {
            if thread_state.borrow_mut().is_process_pending_derefs() {
                process_pending_derefs()?;
            }
            if let Err(e) = get_and_execute_command() {
                match e {
                    StatusCode::TimedOut if !is_main => {
                        result = e;
                        break;
                    }
                    StatusCode::Errno(errno)
                        if errno == (rustix::io::Errno::CONNREFUSED.raw_os_error()) =>
                    {
                        result = e;
                        break;
                    }
                    _ => {
                        panic!("get_and_execute_command() returned unexpected error {e}, aborting");
                    }
                }
            }
        }
        log::debug!(
            "**** THREAD {:?} (PID {}) IS LEAVING THE THREAD POOL err={}\n",
            std::thread::current().id(),
            std::process::id(),
            result
        );

        {
            let mut thread_state = thread_state.borrow_mut();

            thread_state
                .out_parcel
                .write::<u32>(&binder::BC_EXIT_LOOPER)?;
            thread_state.is_looper = false;
        }

        talk_with_driver(false)?;
        ProcessState::as_self()
            .current_threads
            .fetch_sub(1, Ordering::SeqCst);
        Ok(())
    })
}

pub(crate) fn request_death_notification(handle: u32) -> Result<()> {
    log::trace!("request_death_notification: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state
                .out_parcel
                .write::<u32>(&(binder::BC_REQUEST_DEATH_NOTIFICATION))?;
            state.out_parcel.write::<u32>(&(handle))?;
            // Android binder calls writePointer(proxy) here, but we just write handle.
            state
                .out_parcel
                .write::<binder::binder_uintptr_t>(&(handle as _))?;
        }

        Ok(())
    })
}

pub(crate) fn clear_death_notification(handle: u32) -> Result<()> {
    log::trace!("clear_death_notification: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state
                .out_parcel
                .write::<u32>(&(binder::BC_CLEAR_DEATH_NOTIFICATION))?;
            state.out_parcel.write::<u32>(&(handle))?;
            // Android binder calls writePointer(proxy) here, but we just write handle.
            state
                .out_parcel
                .write::<binder::binder_uintptr_t>(&(handle as _))?;
        }

        Ok(())
    })
}

#[derive(Debug)]
pub struct CallingContext {
    pub pid: binder::pid_t,
    pub uid: binder::uid_t,
    pub sid: Option<CString>,
}

impl std::default::Default for CallingContext {
    fn default() -> CallingContext {
        THREAD_STATE.with(|thread_state| -> CallingContext {
            let thread_state = thread_state.borrow();
            match thread_state.transaction.as_ref() {
                Some(transaction) => {
                    let calling_sid = if !transaction.calling_sid.is_null() {
                        // SAFETY: The calling_sid pointer is provided by the binder driver
                        // and is guaranteed to be a valid null-terminated C string during
                        // the transaction lifetime. We check for null before dereferencing.
                        // The pointer is cast from *const u8 to *const i8 as required by CStr::from_ptr.
                        unsafe { Some(CStr::from_ptr(transaction.calling_sid as _).to_owned()) }
                    } else {
                        None
                    };
                    CallingContext {
                        pid: transaction.calling_pid,
                        uid: transaction.calling_uid,
                        sid: calling_sid,
                    }
                }
                None => {
                    log::debug!("CallingContext::new() called outside of transaction");
                    CallingContext {
                        pid: rustix::process::getpid().as_raw_nonzero().get() as _,
                        uid: rustix::process::getuid().as_raw(),
                        sid: None,
                    }
                }
            }
        })
    }
}

pub fn is_handling_transaction() -> bool {
    THREAD_STATE.with(|thread_state| thread_state.borrow().transaction.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_return_to_str() {
        assert_eq!(return_to_str(binder::BR_OK), "BR_OK");
        assert_eq!(return_to_str(binder::BR_TRANSACTION), "BR_TRANSACTION");
        assert_eq!(return_to_str(binder::BR_REPLY), "BR_REPLY");
        assert_eq!(return_to_str(binder::BR_ACQUIRE), "BR_ACQUIRE");
        assert_eq!(return_to_str(binder::BR_INCREFS), "BR_INCREFS");
        assert_eq!(
            return_to_str(binder::BR_ACQUIRE_RESULT),
            "BR_ACQUIRE_RESULT"
        );
        assert_eq!(return_to_str(binder::BR_DEAD_BINDER), "BR_DEAD_BINDER");
        assert_eq!(
            return_to_str(binder::BR_CLEAR_DEATH_NOTIFICATION_DONE),
            "BR_CLEAR_DEATH_NOTIFICATION_DONE"
        );
        assert_eq!(return_to_str(binder::BR_FAILED_REPLY), "BR_FAILED_REPLY");
        assert_eq!(return_to_str(binder::BR_DEAD_REPLY), "BR_DEAD_REPLY");
        assert_eq!(return_to_str(binder::BR_FINISHED), "BR_FINISHED");
        assert_eq!(return_to_str(binder::BR_SPAWN_LOOPER), "BR_SPAWN_LOOPER");
        assert_eq!(
            return_to_str(binder::BR_ATTEMPT_ACQUIRE),
            "BR_ATTEMPT_ACQUIRE"
        );
        assert_eq!(return_to_str(binder::BR_NOOP), "BR_NOOP");
        assert_eq!(return_to_str(binder::BR_SPAWN_LOOPER), "BR_SPAWN_LOOPER");
        assert_eq!(return_to_str(binder::BR_ERROR), "BR_ERROR");
        assert_eq!(return_to_str(binder::BR_DEAD_REPLY), "BR_DEAD_REPLY");
        assert_eq!(return_to_str(binder::BR_FAILED_REPLY), "BR_FAILED_REPLY");
        assert_eq!(return_to_str(binder::BR_FROZEN_REPLY), "BR_FROZEN_REPLY");
        assert_eq!(
            return_to_str(binder::BR_TRANSACTION_SEC_CTX),
            "BR_TRANSACTION_SEC_CTX"
        );
        assert_eq!(return_to_str(binder::BR_DECREFS), "BR_DECREFS");
        assert_eq!(
            return_to_str(binder::BR_TRANSACTION_COMPLETE),
            "BR_TRANSACTION_COMPLETE"
        );
        assert_eq!(
            return_to_str(binder::BR_ONEWAY_SPAM_SUSPECT),
            "BR_ONEWAY_SPAM_SUSPECT"
        );
    }

    #[test]
    fn test_command_to_str() {
        assert_eq!(command_to_str(binder::BC_TRANSACTION), "BC_TRANSACTION");
        assert_eq!(command_to_str(binder::BC_REPLY), "BC_REPLY");
        assert_eq!(
            command_to_str(binder::BC_ACQUIRE_RESULT),
            "BC_ACQUIRE_RESULT"
        );
        assert_eq!(command_to_str(binder::BC_FREE_BUFFER), "BC_FREE_BUFFER");
        assert_eq!(command_to_str(binder::BC_INCREFS), "BC_INCREFS");
        assert_eq!(command_to_str(binder::BC_ACQUIRE), "BC_ACQUIRE");
        assert_eq!(command_to_str(binder::BC_RELEASE), "BC_RELEASE");
        assert_eq!(command_to_str(binder::BC_DECREFS), "BC_DECREFS");
        assert_eq!(command_to_str(binder::BC_INCREFS_DONE), "BC_INCREFS_DONE");
        assert_eq!(command_to_str(binder::BC_ACQUIRE_DONE), "BC_ACQUIRE_DONE");
        assert_eq!(
            command_to_str(binder::BC_ATTEMPT_ACQUIRE),
            "BC_ATTEMPT_ACQUIRE"
        );
        assert_eq!(
            command_to_str(binder::BC_REGISTER_LOOPER),
            "BC_REGISTER_LOOPER"
        );
        assert_eq!(command_to_str(binder::BC_ENTER_LOOPER), "BC_ENTER_LOOPER");
        assert_eq!(command_to_str(binder::BC_EXIT_LOOPER), "BC_EXIT_LOOPER");
        assert_eq!(
            command_to_str(binder::BC_REQUEST_DEATH_NOTIFICATION),
            "BC_REQUEST_DEATH_NOTIFICATION"
        );
        assert_eq!(
            command_to_str(binder::BC_CLEAR_DEATH_NOTIFICATION),
            "BC_CLEAR_DEATH_NOTIFICATION"
        );
        assert_eq!(
            command_to_str(binder::BC_DEAD_BINDER_DONE),
            "BC_DEAD_BINDER_DONE"
        );
        assert_eq!(
            command_to_str(binder::BC_TRANSACTION_SG),
            "BC_TRANSACTION_SG"
        );
        assert_eq!(command_to_str(binder::BC_REPLY_SG), "BC_REPLY_SG");
    }

    /// A panicking `Transactable::transact` must not unwind through
    /// `dispatch_transact_caught` and must surface as
    /// `Err(StatusCode::Unknown)` so the existing `BR_TRANSACTION`
    /// reply path can synthesize a deterministic error reply for the
    /// client. The partial reply (if any) must also be reset so the
    /// client does not misparse half-formed bytes.
    #[test]
    fn test_dispatch_transact_caught_isolates_panic() {
        struct PanickingTransactable;
        impl Transactable for PanickingTransactable {
            fn transact(
                &self,
                _code: TransactionCode,
                _reader: &mut Parcel,
                reply: &mut Parcel,
            ) -> Result<()> {
                // Write a few bytes then panic, so the test verifies
                // the partial reply is discarded.
                reply.write::<i32>(&0x6EAD_BEEFi32).ok();
                panic!("simulated transactable panic");
            }
        }

        let mut reader = Parcel::new();
        let mut reply = Parcel::new();
        let result = dispatch_transact_caught(&PanickingTransactable, 1, &mut reader, &mut reply);

        assert!(
            matches!(result, Err(StatusCode::Unknown)),
            "expected Err(StatusCode::Unknown), got {result:?}"
        );
        assert_eq!(
            reply.data_size(),
            0,
            "partial reply must be discarded after a panic so the \
             client does not misparse half-formed data"
        );
    }

    /// A non-panicking `Transactable::transact` must propagate its
    /// `Result` unchanged through `dispatch_transact_caught` —
    /// regression check that the panic guard does not interfere with
    /// the normal path.
    #[test]
    fn test_dispatch_transact_caught_propagates_normal_result() {
        struct OkTransactable;
        impl Transactable for OkTransactable {
            fn transact(
                &self,
                _code: TransactionCode,
                _reader: &mut Parcel,
                reply: &mut Parcel,
            ) -> Result<()> {
                reply.write::<i32>(&42i32)?;
                Ok(())
            }
        }

        struct ErrTransactable;
        impl Transactable for ErrTransactable {
            fn transact(
                &self,
                _code: TransactionCode,
                _reader: &mut Parcel,
                _reply: &mut Parcel,
            ) -> Result<()> {
                Err(StatusCode::PermissionDenied)
            }
        }

        let mut reader = Parcel::new();
        let mut reply = Parcel::new();
        assert!(dispatch_transact_caught(&OkTransactable, 1, &mut reader, &mut reply).is_ok());
        assert_eq!(reply.data_size(), std::mem::size_of::<i32>());

        let mut reply = Parcel::new();
        let err = dispatch_transact_caught(&ErrTransactable, 1, &mut reader, &mut reply);
        assert!(matches!(err, Err(StatusCode::PermissionDenied)));
    }

    /// `drive_dead_binder_handshake` orchestration must guarantee:
    /// - Phases run in obituary → queue → pin order on success
    /// - An obituary error does NOT short-circuit queue or pin
    /// - A queue error skips pin (acknowledged residual edge)
    /// - When obituary errors and pin errors, obituary takes priority
    /// - The kernel handshake (queue + pin) runs whenever queue succeeds
    ///
    /// These properties protect against the kernel `binder_ref` slot
    /// leak that motivated the refactor — losing any of them
    /// re-introduces the leak shape that the previous `?`-based
    /// implementation exhibited.
    #[test]
    fn test_drive_dead_binder_handshake_orchestration() {
        use std::cell::RefCell;

        let order = RefCell::new(Vec::<&'static str>::new());
        let push = |label: &'static str| order.borrow_mut().push(label);

        // Case A: all phases succeed.
        let result = drive_dead_binder_handshake(
            42,
            || {
                push("obituary");
                Ok(())
            },
            || {
                push("queue");
                Ok(())
            },
            || {
                push("pin");
                Ok(())
            },
        );
        assert!(result.is_ok());
        assert_eq!(*order.borrow(), vec!["obituary", "queue", "pin"]);
        order.borrow_mut().clear();

        // Case B: obituary errors → queue and pin still run; obituary
        // error surfaces. This is the headline guarantee: previously
        // the kernel handshake was skipped on obituary error, leaking
        // the binder_ref slot.
        let result = drive_dead_binder_handshake(
            42,
            || {
                push("obituary");
                Err(StatusCode::DeadObject)
            },
            || {
                push("queue");
                Ok(())
            },
            || {
                push("pin");
                Ok(())
            },
        );
        assert!(matches!(result, Err(StatusCode::DeadObject)));
        assert_eq!(*order.borrow(), vec!["obituary", "queue", "pin"]);
        order.borrow_mut().clear();

        // Case C: queue write fails → pin is skipped (documented
        // residual edge), queue error surfaces.
        let result = drive_dead_binder_handshake(
            42,
            || {
                push("obituary");
                Ok(())
            },
            || {
                push("queue");
                Err(StatusCode::NoMemory)
            },
            || {
                push("pin");
                Ok(())
            },
        );
        assert!(matches!(result, Err(StatusCode::NoMemory)));
        assert_eq!(*order.borrow(), vec!["obituary", "queue"]);
        order.borrow_mut().clear();

        // Case D: obituary OK, pin errors → pin error surfaces.
        let result = drive_dead_binder_handshake(
            42,
            || {
                push("obituary");
                Ok(())
            },
            || {
                push("queue");
                Ok(())
            },
            || {
                push("pin");
                Err(StatusCode::DeadObject)
            },
        );
        assert!(matches!(result, Err(StatusCode::DeadObject)));
        assert_eq!(*order.borrow(), vec!["obituary", "queue", "pin"]);
        order.borrow_mut().clear();

        // Case E: obituary errors AND pin errors → obituary error
        // surfaces (priority); pin error is logged in the error path
        // (asserting log content is out of scope for a unit test).
        let result = drive_dead_binder_handshake(
            42,
            || {
                push("obituary");
                Err(StatusCode::PermissionDenied)
            },
            || {
                push("queue");
                Ok(())
            },
            || {
                push("pin");
                Err(StatusCode::DeadObject)
            },
        );
        assert!(
            matches!(result, Err(StatusCode::PermissionDenied)),
            "obituary error must take priority over pin error, got {result:?}"
        );
        assert_eq!(*order.borrow(), vec!["obituary", "queue", "pin"]);
    }

    /// Regression test for `b17d522`: `process_pending_derefs` must
    /// tolerate a re-entrant push to `BINDER_DEREFS` from a user
    /// `Inner<T>::drop` callback.
    ///
    /// Pre-`b17d522`, the function held the `BINDER_DEREFS` borrow
    /// across `deref_native_kernel`. When entry removal triggered an
    /// `Inner<T>::drop` whose user destructor synchronously caused
    /// another BR_RELEASE / BR_DECREFS to land (path: outgoing IPC →
    /// `wait_for_response` → `talk_with_driver` → `execute_command`
    /// queues into `BINDER_DEREFS`), the second `borrow_mut()` panicked.
    ///
    /// We simulate that re-entrancy in-process by pushing a second id
    /// into `BINDER_DEREFS.pending_weak_derefs` directly from a
    /// drop-fired sentinel — no kernel needed.
    ///
    /// **Linux + binderfs only** (uses `ProcessState::init_default`,
    /// which opens `/dev/binderfs/binder`). Same convention as the
    /// `process_state` M4 tests; surfaces under
    /// `.github/workflows/integration-test.yml`.
    #[test]
    fn test_process_pending_derefs_handles_reentrant_push_from_drop() {
        use std::sync::atomic::AtomicU64;
        use std::sync::{self, Mutex};
        let process = ProcessState::init_default();

        let drop_log: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let pusher_target: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

        // Sentinel B: drop pushes nothing, only records that it fired.
        struct DropFireSentinel {
            my_id: Arc<AtomicU64>,
            log: Arc<Mutex<Vec<u64>>>,
        }
        impl Drop for DropFireSentinel {
            fn drop(&mut self) {
                self.log
                    .lock()
                    .unwrap()
                    .push(self.my_id.load(Ordering::SeqCst));
            }
        }
        impl IBinder for DropFireSentinel {
            fn link_to_death(&self, _: sync::Weak<dyn DeathRecipient>) -> Result<()> {
                Err(StatusCode::InvalidOperation)
            }
            fn unlink_to_death(&self, _: sync::Weak<dyn DeathRecipient>) -> Result<()> {
                Err(StatusCode::InvalidOperation)
            }
            fn ping_binder(&self) -> Result<()> {
                Ok(())
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_transactable(&self) -> Option<&dyn Transactable> {
                None
            }
            fn descriptor(&self) -> &str {
                "rsbinder.test.DropFireSentinel"
            }
            fn is_remote(&self) -> bool {
                false
            }
            fn inc_strong(&self, _: &SIBinder) -> Result<()> {
                Ok(())
            }
            fn attempt_inc_strong(&self) -> bool {
                true
            }
            fn dec_strong(&self, _: Option<std::mem::ManuallyDrop<SIBinder>>) -> Result<()> {
                Ok(())
            }
            fn inc_weak(&self, _: &WIBinder) -> Result<()> {
                Ok(())
            }
            fn dec_weak(&self) -> Result<()> {
                Ok(())
            }
        }

        // Sentinel A: drop pushes B's id back into BINDER_DEREFS,
        // simulating an outgoing IPC's BR_DECREFS landing during the
        // drain.
        struct ReentrantPusher {
            my_id: Arc<AtomicU64>,
            target: Arc<AtomicU64>,
            log: Arc<Mutex<Vec<u64>>>,
        }
        impl Drop for ReentrantPusher {
            fn drop(&mut self) {
                self.log
                    .lock()
                    .unwrap()
                    .push(self.my_id.load(Ordering::SeqCst));
                let target_id = self.target.load(Ordering::SeqCst);
                if target_id != 0 {
                    BINDER_DEREFS.with(|d| {
                        d.borrow_mut().pending_weak_derefs.push_back(target_id);
                    });
                }
            }
        }
        impl IBinder for ReentrantPusher {
            fn link_to_death(&self, _: sync::Weak<dyn DeathRecipient>) -> Result<()> {
                Err(StatusCode::InvalidOperation)
            }
            fn unlink_to_death(&self, _: sync::Weak<dyn DeathRecipient>) -> Result<()> {
                Err(StatusCode::InvalidOperation)
            }
            fn ping_binder(&self) -> Result<()> {
                Ok(())
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_transactable(&self) -> Option<&dyn Transactable> {
                None
            }
            fn descriptor(&self) -> &str {
                "rsbinder.test.ReentrantPusher"
            }
            fn is_remote(&self) -> bool {
                false
            }
            fn inc_strong(&self, _: &SIBinder) -> Result<()> {
                Ok(())
            }
            fn attempt_inc_strong(&self) -> bool {
                true
            }
            fn dec_strong(&self, _: Option<std::mem::ManuallyDrop<SIBinder>>) -> Result<()> {
                Ok(())
            }
            fn inc_weak(&self, _: &WIBinder) -> Result<()> {
                Ok(())
            }
            fn dec_weak(&self) -> Result<()> {
                Ok(())
            }
        }

        // Publish B first so we know its id before constructing A.
        let id_b_holder = Arc::new(AtomicU64::new(0));
        let arc_b: Arc<dyn IBinder> = Arc::new(DropFireSentinel {
            my_id: Arc::clone(&id_b_holder),
            log: Arc::clone(&drop_log),
        });
        let id_b = process.publish_native(Arc::clone(&arc_b));
        id_b_holder.store(id_b, Ordering::SeqCst);
        // Bump kernel_refs so deref_native_kernel later drives it 1→0
        // and removes the entry. Drop the returned strong arc at
        // semicolon so the table's binder_pin is the only holder.
        process
            .ref_native_kernel(id_b)
            .expect("ref_native_kernel(id_b)");
        // Drop user-side clone: the table holds the only strong now.
        drop(arc_b);

        // Configure pusher target to id_b before publishing A.
        pusher_target.store(id_b, Ordering::SeqCst);

        let id_a_holder = Arc::new(AtomicU64::new(0));
        let arc_a: Arc<dyn IBinder> = Arc::new(ReentrantPusher {
            my_id: Arc::clone(&id_a_holder),
            target: Arc::clone(&pusher_target),
            log: Arc::clone(&drop_log),
        });
        let id_a = process.publish_native(Arc::clone(&arc_a));
        id_a_holder.store(id_a, Ordering::SeqCst);
        process
            .ref_native_kernel(id_a)
            .expect("ref_native_kernel(id_a)");
        drop(arc_a);

        // Push id_a as a strong deref. Do NOT push id_b — A's drop
        // will push it during the drain.
        BINDER_DEREFS.with(|d| {
            d.borrow_mut().pending_strong_derefs.push_back(id_a);
        });

        // Drive the drain. Pre-`b17d522` this would panic with
        // "already mutably borrowed: BorrowError" the moment A's drop
        // tried to re-borrow BINDER_DEREFS.
        process_pending_derefs().expect("process_pending_derefs must not panic or error");

        // Both natives must have dropped — re-entrant push picked up
        // by the outer loop.
        let log = drop_log.lock().unwrap();
        assert_eq!(
            log.len(),
            2,
            "both A and B must have dropped exactly once, got {log:?}"
        );
        assert!(log.contains(&id_a), "A's drop must fire, got {log:?}");
        assert!(log.contains(&id_b), "B's drop must fire, got {log:?}");

        // Both queues must be empty after drain.
        BINDER_DEREFS.with(|d| {
            let derefs = d.borrow();
            assert!(
                derefs.pending_weak_derefs.is_empty(),
                "weak queue must be empty after drain"
            );
            assert!(
                derefs.pending_strong_derefs.is_empty(),
                "strong queue must be empty after drain"
            );
        });

        // Both entries must be removed from the published_natives table.
        assert!(process.lookup_native(id_a).is_none());
        assert!(process.lookup_native(id_b).is_none());
    }
}
