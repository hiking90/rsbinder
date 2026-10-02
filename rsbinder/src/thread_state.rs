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
//!     `transact`, `wait_for_response`, `flush_commands`, `flush_if_needed`,
//!     `inc_strong_handle`, `dec_strong_handle`, `inc_weak_handle`,
//!     `dec_weak_handle`, `free_buffer`, and `process_pending_derefs`.
//!
//! Violation manifests as a `RefCell` panic ("already borrowed" /
//! "already mutably borrowed"). Binder's nested-IPC protocol means user
//! callbacks routinely make outgoing binder calls from inside an incoming
//! `BR_TRANSACTION`, so this is not a theoretical concern.
//!
//! The RPC stack's thread-locals are bound by the same rule:
//! `rpc::session::DRIVING` (the nested-call recursion marker) and
//! `RPC_CALLING` (below) both live across user callbacks — `rpc_transact`,
//! `binder_died` — and every access copies its value out inside `with`,
//! holding no guard across the callout. `RPC_CALLING_CAPS` (below) lives
//! across the same callbacks but is outside R1 entirely: it is a `Cell`, so
//! `get`/`set` copy a `Copy` value out with no borrow guard to hold in the
//! first place.
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
//! `process_pending_derefs` applies P2 explicitly; see below.
//!
//! ## R1 at specific call sites
//!
//! - **`process_pending_derefs`** is called with no `THREAD_STATE` or
//!   `BINDER_DEREFS` borrow held. Each `deref_native_kernel(id)` may, when an
//!   entry's counters both reach zero, reach `remove_entry_if_zero`, which
//!   drops the canonical `Arc<dyn IBinder>` and runs the user's
//!   `Inner<T>::drop` synchronously. A user destructor that makes an outgoing
//!   synchronous call goes `wait_for_response` → `talk_with_driver` →
//!   `execute_command`, whose `BR_RELEASE` / `BR_DECREFS` arms push into
//!   `BINDER_DEREFS` through a fresh `borrow_mut()`. The function therefore
//!   alternates: take the borrow, take the whole weak queue (or pop one strong
//!   id), release the borrow, dispatch outside it. Re-entrant pushes land in
//!   a fresh borrow and the outer loop picks them up on its next iteration;
//!   do not hoist the borrow out of the loop. Order matches libbinder: all
//!   weak derefs drain before the next strong one is dispatched, so a weak
//!   deref queued by a strong-deref destructor runs ahead of the next pending
//!   strong; each queue is FIFO (`VecDeque::pop_front` / `mem::take`).
//! - **`dispatch_transact_caught`** is called with no `THREAD_STATE` or
//!   `BINDER_DEREFS` borrow held: `Transactable::transact` is user code that
//!   may make nested binder calls into this module.
//! - **`talk_with_driver`** holds an immutable `THREAD_STATE` borrow across the
//!   `BINDER_WRITE_READ` ioctl to read `driver`. This is sound only because
//!   the ioctl does not re-enter Rust on the same thread (the kernel queues
//!   incoming work and returns). EINTR / signal-safety / cancellation handling
//!   or any same-thread Rust callback added at this point breaks that
//!   property and turns the borrow into an R1 violation; the fix then is to
//!   clone the `Arc<File>` out under a short borrow and pass it by value
//!   across the syscall.
//!
//! # RPC calling context
//!
//! The kernel calling identity lives in `THREAD_STATE.transaction`, but
//! `ThreadState::new()` eagerly pulls `ProcessState::as_self()`, which
//! **panics in a pure-RPC process** (no kernel binder). The RPC dispatch path
//! therefore must not touch `THREAD_STATE`. Instead it stamps the caller into
//! the separate, `ProcessState`-independent `RPC_CALLING` thread-local for the
//! duration of one RPC `on_transact`, and the public calling-identity
//! accessors consult it *first*, so they work in a pure-RPC process and never
//! force the kernel thread-local (Plan 2-16 Phase B).
//!
//! The cell holds an `Arc<PeerIdentity>` — the full peer, so handler-side
//! authorization can see the uid *and* the certificate/vsock identity. It is
//! borrowed only momentarily (clone-out) and never across the user handler:
//! set → run handler → restore, so R1 holds by construction. The `Arc` keeps
//! the per-dispatch and oneway-drain installs cheap.
//!
//! `RPC_CALLING_CAPS` holds the dispatching session's `TransportCaps`,
//! installed by the same guard. It is a separate cell rather than a field
//! beside the peer for two reasons: `Caller::Rpc(PeerIdentity)` is a public
//! tuple variant, so widening it would break every `match` on it; and caps are
//! a `Copy` `u32` that a `Cell` returns without a borrow, so reading them
//! cannot hold anything across a user callback. The caps are a snapshot taken
//! when the transaction was dispatched: the session can gain or lose a
//! connection while the handler runs, and the handler still sees what the
//! call arrived over.
//!
//! `RpcCallingGuard::install` stamps both cells around one RPC dispatch and
//! restores their previous values on drop, so nested dispatches nest, unwind
//! included. `RpcCallingGuard::suspend` clears them wherever the kernel driver
//! makes this thread run user code. A thread parked in `wait_for_response`
//! executes whatever the driver returns, so an RPC handler that makes an
//! outgoing kernel call can have driver-initiated user code run inside it, and
//! that code's caller is not the RPC peer. With the cells left installed every
//! calling-identity accessor would answer for the suspended RPC peer
//! (`calling_caps` would report the RPC session's caps for a caller that has
//! all of them).
//!
//! # Calling-identity token
//!
//! `clear_calling_identity` returns the 64-bit token of AOSP
//! `packCallingIdentity` (`IPCThreadState.cpp:498-511`), bit for bit:
//!
//! ```text
//! 32b      |        1b         |         1b            |        30b
//! uid      | pid sign bit      | hasExplicitIdentity   | pid (rest)
//! ```
//!
//! `hasExplicitIdentity` sits in the second-highest bit of the low half so a
//! negative pid (top sign bit) still round-trips. Tokens never go on the wire;
//! the AOSP layout keeps `clear`/`restore` tokens comparable across
//! implementations and lets AOSP tests against the bit pattern pass. The pid
//! window is 30 bits plus sign: a value such as `i32::MIN`, which overlaps the
//! `hasExplicitIdentity` bit, is outside it.
//!
//! `TransactionState::has_explicit_identity` is AOSP `mHasExplicitIdentity`:
//! `true` after `clear_calling_identity()` has replaced the kernel-delivered
//! `calling_uid`/`calling_pid` with this process's own, and reset to `false`
//! on every incoming `BR_TRANSACTION` (`IPCThreadState.cpp:1141`,
//! `mHasExplicitIdentity = false`).
//!
//! # Thread exit
//!
//! `ThreadExitGuard` is AOSP `IPCThreadState::threadDestructor`: when a thread
//! that talked to the driver ends, it flushes what is still queued and sends
//! `BINDER_THREAD_EXIT`. Without it the kernel keeps the thread's
//! `binder_thread` until the fd closes, and a recycled tid inherits it. The
//! guard is armed from `talk_with_driver`, the one function every driver
//! round trip goes through, so every thread that touched the driver gets
//! exactly one.
//!
//! # Dispatch notes
//!
//! - **`TF_STATUS_CODE` reply carrying 0.** AOSP
//!   `IPCThreadState::waitForResponse` ends the status-code branch with an
//!   unconditional `goto finish` (`return err`, even for `NO_ERROR`), so such
//!   a reply yields a successful empty reply and never loops.
//!   `wait_for_response` returns an empty `Parcel` to match; falling through
//!   would re-enter its loop and block in `talk_with_driver` waiting for a
//!   command a conforming peer never sends, hanging (possibly the main
//!   thread) on a malformed or hostile reply.
//! - **Native target ref balance.** For a `BR_TRANSACTION` to a local binder,
//!   `target.ptr` is the process-monotonic id assigned by `publish_native`.
//!   It is resolved via the sidecar table (read-only, no count change); the
//!   table keeps `Inner<T>` alive while the entry exists, so the `SIBinder`
//!   built there can drive the user's `Transactable`. Its `RefCounter` ops
//!   balance within the block: `from_arc` calls `inc_strong` (+1),
//!   `attempt_increase` adds one (+1), `decrease()` cancels one (−1), and the
//!   `SIBinder`'s `Drop` cancels the last (−1).
//! - **Reply path containment.** The saved transaction state is restored on
//!   every exit (an `Err` or panic must not leak it into the next command),
//!   and a panic from the reply flush/await — `talk_with_driver` or a nested
//!   dispatch; OOM aborts and is not catchable — is caught rather than
//!   unwinding the worker loop (as `dispatch_transact_caught` does; a dropped
//!   reply reaches a sync caller as `BR_DEAD_REPLY`). The `unflushed_mark` is
//!   taken outside `catch_unwind` so the panic arm can rewind to it too. That
//!   arm rewinds only this transaction's bytes: truncating the whole
//!   `out_parcel` would also drop a `BC_FREE_BUFFER` queued earlier by a
//!   nested call — which `flush_if_needed` never flushes on a looper thread —
//!   and the kernel would never reclaim that transaction buffer. It does not
//!   `retry_flush` either: the panic may have come from `talk_with_driver`,
//!   and a re-entry would unwind outside the `catch_unwind`.
//! - **Call restriction before queuing.** `transact` enforces
//!   `CallRestriction` *before* queuing `BC_TRANSACTION`.
//!   `write_transaction_data` records raw pointers into the caller's `data`;
//!   if the `FatalIfNotOneway` `panic!` unwinds instead of aborting (it is
//!   reachable under `dispatch_transact_caught`'s `catch_unwind` for a nested
//!   sync call, and under tokio's `spawn_blocking` panic capture), a completed
//!   `BC_TRANSACTION` with dangling pointers would stay in `out_parcel` and be
//!   flushed later — a cross-process use-after-free. AOSP checks *after*
//!   queuing, but its `LOG_ALWAYS_FATAL` aborts, so its queued command is
//!   never flushed.
//! - **Rewinding unflushed commands.** When a transact or reply fails,
//!   `discard_unflushed_commands` rewinds `out_parcel` to the `queued_at` mark
//!   taken by `unflushed_mark`: the command there points into memory the
//!   caller is about to release. `out_flush_epoch` is bumped each time the
//!   driver consumes `out_parcel`, so the mark tells "my command is still
//!   queued" from "consumed, and the buffer refilled since"; only the former
//!   is rewound. With `retry_flush` (`BC_REPLY` only) one more flush is tried
//!   first, so a transient failure does not cost the peer its reply. A retried
//!   `BC_TRANSACTION` would instead leave a two-way call in flight whose
//!   `BR_REPLY` the next `transact` would take.
//! - **Handler panics.** `dispatch_transact_caught` catches a panic from
//!   `Transactable::transact`, discards the partial reply, and returns
//!   `StatusCode::Unknown`, so the `BR_TRANSACTION` reply path sends the
//!   client a deterministic error reply instead of leaving it waiting for
//!   `BR_REPLY`. This mirrors the guard around `DeathRecipient` callbacks in
//!   `ProxyHandle::dispatch_obituary_callbacks`; the `Transactable` trait doc
//!   states the full guarantee.
//! - **Dead-binder handshake.** For `BR_DEAD_BINDER`,
//!   `drive_dead_binder_handshake` runs three phases: `obituary` (the user
//!   callbacks), `queue_done` (write `BC_DEAD_BINDER_DONE`), and `pin_release`
//!   (flush `release_obituary_pin`'s `BC_DECREFS`). The kernel `binder_ref`
//!   slot leaks for good if either of the last two is lost, so both run
//!   whatever the obituary returned. The obituary error takes priority over
//!   the pin-release error, which is logged so it is not lost when both fail.
//!   A `queue_done` failure (only when `out_parcel` is unhealthy, e.g. OOM)
//!   skips `pin_release` and the slot leaks; the next ioctl on this thread
//!   fails anyway.
//! - **Refused commands.** A `BINDER_WRITE_READ` that fails or stops short
//!   reports `write_consumed`, the offset the driver gave up at, so the
//!   command starting there is the one it rejected. `describe_refused_command`
//!   names it because the usual causes are caller bugs the errno alone cannot
//!   tell apart: a refcount underflow from an over-released proxy
//!   (`BC_RELEASE` / `BC_DECREFS`), or a `BC_FREE_BUFFER` for a buffer already
//!   returned.

use log::error;
use std::backtrace::Backtrace;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::fmt::Debug;
use std::fs::File;
use std::sync::{atomic::Ordering, Arc};

use crate::{
    binder::*, command_stream::CommandStream, error::*, parcel::*, process_state::*, sys::*,
};

// R1 (module doc): no borrow of these cells across a call that may re-borrow them.
thread_local! {
    static THREAD_STATE: RefCell<ThreadState> = RefCell::new(ThreadState::new());
    static BINDER_DEREFS: RefCell<BinderDerefs> = RefCell::new(BinderDerefs::new());
    // Apart from `THREAD_STATE`: `ThreadState`'s own Drop could not reach `THREAD_STATE.with`.
    static THREAD_EXIT_GUARD: RefCell<Option<ThreadExitGuard>> = const { RefCell::new(None) };
}

/// AOSP `threadDestructor`: flush, then `BINDER_THREAD_EXIT`; see module doc "Thread exit".
struct ThreadExitGuard {
    driver: Arc<File>,
}

impl Drop for ThreadExitGuard {
    fn drop(&mut self) {
        // TLS destruction order is unspecified; `THREAD_STATE` may already be gone.
        if THREAD_STATE.try_with(|_| ()).is_ok() {
            if let Err(e) = flush_commands() {
                log::warn!("flush on binder thread exit failed: {e}");
            }
        }
        if let Err(e) = binder::thread_exit(&*self.driver, 0) {
            log::warn!("BINDER_THREAD_EXIT failed: {e}");
        }
    }
}

/// Arms [`ThreadExitGuard`] once per thread, from `talk_with_driver` (every round trip).
fn ensure_thread_exit_guard(driver: &Arc<File>) {
    let _ = THREAD_EXIT_GUARD.try_with(|g| {
        let mut g = g.borrow_mut();
        if g.is_none() {
            *g = Some(ThreadExitGuard {
                driver: Arc::clone(driver),
            });
        }
    });
}

// ---- RPC calling context: see module doc "RPC calling context" ----
#[cfg(feature = "rpc")]
thread_local! {
    static RPC_CALLING: std::cell::RefCell<Option<std::sync::Arc<crate::rpc::transport::PeerIdentity>>> =
        const { std::cell::RefCell::new(None) };
    /// Session caps beside `RPC_CALLING`, a `Cell` on purpose; see module doc (RPC context).
    static RPC_CALLING_CAPS: std::cell::Cell<Option<crate::TransportCaps>> =
        const { std::cell::Cell::new(None) };
}

/// Uid for uid-less peers: `(uid_t)-1` matches no uid ACL and is not root; Plan 2-16 §1.2.
#[cfg(feature = "rpc")]
pub(crate) const RPC_UNKNOWN_CALLING_UID: binder::uid_t = binder::uid_t::MAX;

/// Sets or clears both RPC calling cells for one scope, restoring both on drop (see module doc).
#[cfg(feature = "rpc")]
pub(crate) struct RpcCallingGuard {
    previous: Option<std::sync::Arc<crate::rpc::transport::PeerIdentity>>,
    previous_caps: Option<crate::TransportCaps>,
}

#[cfg(feature = "rpc")]
impl RpcCallingGuard {
    /// Installs the peer and the `caps` snapshotted at dispatch for one handler.
    pub(crate) fn install(
        peer: std::sync::Arc<crate::rpc::transport::PeerIdentity>,
        caps: crate::TransportCaps,
    ) -> Self {
        let previous = RPC_CALLING.with(|c| c.borrow_mut().replace(peer));
        let previous_caps = RPC_CALLING_CAPS.with(|c| c.replace(Some(caps)));
        RpcCallingGuard {
            previous,
            previous_caps,
        }
    }

    /// Clears the RPC context while the driver runs user code here; see module doc.
    pub(crate) fn suspend() -> Self {
        let previous = RPC_CALLING.with(|c| c.borrow_mut().take());
        let previous_caps = RPC_CALLING_CAPS.with(|c| c.take());
        RpcCallingGuard {
            previous,
            previous_caps,
        }
    }
}

#[cfg(feature = "rpc")]
impl Drop for RpcCallingGuard {
    fn drop(&mut self) {
        RPC_CALLING.with(|c| *c.borrow_mut() = self.previous.take());
        RPC_CALLING_CAPS.with(|c| c.set(self.previous_caps.take()));
    }
}

/// A local peer's kernel-vouched `(uid, pid)`, else `(RPC_UNKNOWN_CALLING_UID, -1)`.
#[cfg(feature = "rpc")]
pub(crate) fn peer_uid_pid(
    peer: &crate::rpc::transport::PeerIdentity,
) -> (binder::uid_t, binder::pid_t) {
    match peer {
        crate::rpc::transport::PeerIdentity::Local { uid, pid } => (*uid, *pid),
        _ => (RPC_UNKNOWN_CALLING_UID, -1),
    }
}

/// The RPC caller's `(uid, pid)` if dispatching RPC; never touches `THREAD_STATE`/`ProcessState`.
#[inline]
fn rpc_calling() -> Option<(binder::uid_t, binder::pid_t)> {
    #[cfg(feature = "rpc")]
    {
        RPC_CALLING.with(|c| c.borrow().as_deref().map(peer_uid_pid))
    }
    #[cfg(not(feature = "rpc"))]
    {
        None
    }
}

/// The caller of the in-flight binder transaction, tagged by transport so
/// handler-side authorization can branch **explicitly** — the kernel and
/// RPC trust boundaries are genuinely different and must not be papered
/// over (Plan 2-16). Returned by [`calling_caller`].
///
/// Authorization differs by arm:
/// - [`Caller::Kernel`] — Android permissions (`@EnforcePermission` /
///   [`crate::permission_controller::check_permission`]), `uid`/`pid`
///   ACLs, or the SELinux `sid`.
/// - `Caller::Rpc` (with the `rpc` feature) — the transport's own trust
///   boundary: a Unix `PeerIdentity::Local` uid ACL, a TLS
///   `Certificate` subject/fingerprint allowlist, etc.
///   `@EnforcePermission` is **denied** over RPC (Plan 2-16 Phase A).
///
/// What the transport can *do* is not part of this value: capabilities
/// belong to the dispatch (for RPC, to the session it arrived on), not to
/// the caller identity, and a `Caller` is `Clone` and outlives its
/// handler. Ask [`calling_caps`] inside the handler instead.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Caller {
    /// A kernel-binder caller. `sid` is the SELinux context when the
    /// target binder requested it (`BinderFeatures::set_requesting_sid`).
    Kernel {
        /// Caller effective uid (kernel `sender_euid`).
        uid: binder::uid_t,
        /// Caller pid (kernel `sender_pid`).
        pid: binder::pid_t,
        /// SELinux context, if delivered (see [`get_calling_sid`]).
        sid: Option<CString>,
    },
    /// An RPC caller, identified by its transport peer identity.
    #[cfg(feature = "rpc")]
    Rpc(crate::rpc::transport::PeerIdentity),
}

/// The caller of the current in-flight binder transaction, tagged by
/// transport ([`Caller`]), or `None` when this thread is not dispatching
/// one. Pure-RPC safe (never forces the kernel `THREAD_STATE` /
/// `ProcessState`).
///
/// This is the transport-aware basis for handler-side authorization: a
/// service `match`es on the [`Caller`] arm and applies the right check
/// (Android permission vs uid ACL vs TLS-cert allowlist). For a single
/// injectable policy across both transports, see
/// [`crate::permission_controller::PermissionAuthority`].
pub fn calling_caller() -> Option<Caller> {
    // RPC first: it never forces the ProcessState-coupled kernel thread-local.
    #[cfg(feature = "rpc")]
    {
        if let Some(peer) = RPC_CALLING.with(|c| c.borrow().as_deref().cloned()) {
            return Some(Caller::Rpc(peer));
        }
    }
    if !ProcessState::is_initialized() {
        return None;
    }
    THREAD_STATE.with(|thread_state| {
        let thread_state = thread_state.borrow();
        thread_state.transaction.as_ref().map(|tr| {
            let sid = if tr.calling_sid.is_null() {
                None
            } else {
                // SAFETY: non-null = live NUL-terminated kernel sid (nulled on free).
                Some(unsafe { CStr::from_ptr(tr.calling_sid as _).to_owned() })
            };
            Caller::Kernel {
                uid: tr.calling_uid,
                pid: tr.calling_pid,
                sid,
            }
        })
    })
}

/// What the in-flight transaction arrived over, as a
/// [`TransportCaps`](crate::TransportCaps) set, or `None` when this
/// thread is not dispatching one.
///
/// This is the server-side counterpart of
/// [`Client::caps`](crate::Client::caps): a handler that wants to call
/// back into its caller — hand it a pipe, park a callback binder, start
/// a stream — can ask here whether that is possible before promising it.
///
/// Kernel binder is always the full set. An RPC caller's value is the
/// snapshot taken when the transaction was dispatched, so it does not
/// change under a running handler even if the session gains or loses a
/// connection meanwhile.
///
/// The **innermost** dispatch answers. Binder's nested IPC can deliver a
/// re-entrant kernel transaction to a thread that is running an RPC
/// handler (the handler made an outgoing kernel call); inside that kernel
/// handler this reports the full kernel set, not the suspended RPC
/// session's caps, and the RPC value comes back when it returns.
///
/// Pure-RPC safe: the RPC arm never touches the kernel thread-local or
/// [`ProcessState`].
pub fn calling_caps() -> Option<crate::TransportCaps> {
    // RPC first, as in `calling_caller`; `suspend` clears it so the innermost dispatch answers.
    #[cfg(feature = "rpc")]
    {
        if let Some(caps) = RPC_CALLING_CAPS.with(|c| c.get()) {
            return Some(caps);
        }
    }
    if !ProcessState::is_initialized() {
        return None;
    }
    THREAD_STATE.with(|thread_state| {
        let thread_state = thread_state.borrow();
        thread_state
            .transaction
            .as_ref()
            .map(|_| crate::TransportCaps::KERNEL)
    })
}

// `return_to_str` special-cases BR_TRANSACTION_SEC_CTX (nr=2), so nr=20 is PENDING_FROZEN.
const RETURN_STRINGS: [&str; 23] = [
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
    "BR_TRANSACTION_PENDING_FROZEN",
    "BR_FROZEN_BINDER",
    "BR_CLEAR_FREEZE_NOTIFICATION_DONE",
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

// Freeze BC labels (nr=19/20/21) are debug-print only; nothing queues these commands.
const COMMAND_STRINGS: [&str; 22] = [
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
    "BC_REQUEST_FREEZE_NOTIFICATION",
    "BC_CLEAR_FREEZE_NOTIFICATION",
    "BC_FREEZE_NOTIFICATION_DONE",
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
    last_transaction_binder_flags: u32,
    /// AOSP `mHasExplicitIdentity`: set by `clear_calling_identity`, reset per `BR_TRANSACTION`.
    has_explicit_identity: bool,
}

impl TransactionState {
    fn from_transaction_data(data: &binder::binder_transaction_data_secctx) -> Self {
        TransactionState {
            calling_pid: data.transaction_data.sender_pid,
            calling_sid: data.secctx as _,
            calling_uid: data.transaction_data.sender_euid,
            last_transaction_binder_flags: data.transaction_data.flags,
            has_explicit_identity: false,
        }
    }
}

/// AOSP `packCallingIdentity` layout; see module doc "Calling-identity token".
fn pack_calling_identity(has_explicit: bool, uid: binder::uid_t, pid: binder::pid_t) -> i64 {
    let pid_low = pid as u32;
    let pid_low = if has_explicit {
        pid_low | (1 << 30)
    } else {
        pid_low & !(1 << 30)
    };
    let token = ((uid as u64) << 32) | (pid_low as u64);
    token as i64
}

fn unpack_has_explicit_identity(token: i64) -> bool {
    ((token as i32) & (1 << 30)) != 0
}

fn unpack_calling_uid(token: i64) -> binder::uid_t {
    (token >> 32) as binder::uid_t
}

fn unpack_calling_pid(token: i64) -> binder::pid_t {
    let encoded = token as i32;
    if (encoded & (1 << 31)) != 0 {
        // Negative PID — restore the sign bit overlap with bit 30.
        (encoded | (1 << 30)) as binder::pid_t
    } else {
        (encoded & !(1 << 30)) as binder::pid_t
    }
}

// Inbound BR_RELEASE/BR_DECREFS ids (not pointers) of our natives; `process_pending_derefs`.
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

/// Drains `BR_RELEASE`/`BR_DECREFS` ids; see module doc "R1 at specific call sites".
fn process_pending_derefs() -> Result<()> {
    loop {
        // Drain weak fully; re-take per batch, since a user `Inner<T>::drop` may queue more.
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

        // Pop one strong id, dispatch outside the borrow; none left means both queues are empty.
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
    in_parcel: CommandStream,
    out_parcel: CommandStream,
    transaction: Option<TransactionState>,
    strict_mode_policy: i32,
    is_looper: bool,
    is_flushing: bool,
    /// Bumped per driver consume of `out_parcel`: "still queued" vs "consumed and refilled".
    out_flush_epoch: u64,
    call_restriction: CallRestriction,
    driver: Arc<File>,
}

impl ThreadState {
    fn new() -> Self {
        ThreadState {
            in_parcel: CommandStream::new(),
            out_parcel: CommandStream::new(),
            transaction: None,
            strict_mode_policy: 0,
            is_looper: false,
            is_flushing: false,
            out_flush_epoch: 0,
            call_restriction: ProcessState::as_self().call_restriction(),
            driver: ProcessState::as_self().driver(),
        }
    }

    pub(crate) fn set_strict_mode_policy(&mut self, policy: i32) {
        self.strict_mode_policy = policy;
    }

    /// Next command's `(flush epoch, out_parcel length)`, for `discard_unflushed_commands`.
    fn unflushed_mark(&self) -> (u64, usize) {
        (self.out_flush_epoch, self.out_parcel.data_size())
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

        let tr = if *status == StatusCode::Ok.into() {
            binder_transaction_data {
                target,
                cookie: 0,
                code,
                flags,
                sender_pid: 0,
                sender_euid: 0,
                data_size: data.ipc_data_size() as _,
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

        let start = self.out_parcel.data_size();
        self.out_parcel.write_cmd::<u32>(&cmd)?;
        if let Err(e) = self.out_parcel.write_transaction(&tr) {
            // Roll back the orphan cmd word: a bare BC_* opcode would desync the driver.
            let _ = self.out_parcel.set_data_size(start);
            return Err(e);
        }

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

/// Set the per-thread strict-mode policy bits to attach to the next
/// outgoing transaction header.
///
/// The next `Parcel::write_interface_token` (and AIDL-generated client
/// stubs that call it) prepends the policy as a 32-bit prefix before the
/// interface header, matching AOSP `Parcel::writeInterfaceToken`
/// (`frameworks/native/libs/binder/Parcel.cpp`). The server-side
/// `check_interface` extracts the prefix back into the receiving thread's
/// state, where it is read by the framework `StrictMode` enforcement
/// code.
///
/// `policy == 0` (default) keeps the wire byte-identical to legacy
/// behavior. Non-zero values are framework-defined bit flags — opt in
/// only when interoperating with Android system_server's strict-mode
/// chain (disk-I/O / network-on-main-thread / etc. policies).
///
/// No-op in a process where kernel binder is not initialized (pure-RPC
/// process).
///
/// Mirrors AOSP `IPCThreadState::setStrictModePolicy(int32_t)`
/// (`IPCThreadState.cpp:575`).
pub fn set_strict_mode_policy(policy: i32) {
    // Pure-RPC process: `ThreadState::new` would panic pulling `ProcessState`.
    if !ProcessState::is_initialized() {
        return;
    }
    THREAD_STATE.with(|thread_state| {
        thread_state.borrow_mut().set_strict_mode_policy(policy);
    })
}

/// Read the per-thread strict-mode policy bits.
///
/// On a server thread that has just received a `BR_TRANSACTION`, returns
/// the policy that was attached to the inbound header by the caller's
/// `Parcel::write_interface_token` (extracted in `check_interface`).
/// On a client thread, returns whatever was last set by
/// [`set_strict_mode_policy`] (default `0`).
///
/// Returns `0` in a process where kernel binder is not initialized
/// (pure-RPC process).
///
/// Mirrors AOSP `IPCThreadState::getStrictModePolicy() const`
/// (`IPCThreadState.cpp:580`).
pub fn get_strict_mode_policy() -> i32 {
    if !ProcessState::is_initialized() {
        return 0;
    }
    THREAD_STATE.with(|thread_state| thread_state.borrow().strict_mode_policy)
}

/// AOSP `IPCThreadState::mWorkSource` + `mPropagateWorkSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkSource {
    uid: binder::uid_t,
    propagate: bool,
}

impl WorkSource {
    const UNSET: WorkSource = WorkSource {
        uid: UNSET_WORK_SOURCE as binder::uid_t,
        propagate: false,
    };

    // Zero-extended, unlike AOSP, so an unset uid keeps the propagate bit on restore.
    fn token(self) -> i64 {
        ((self.propagate as i64) << WORK_SOURCE_PROPAGATED_BIT_INDEX) | (self.uid as i64)
    }

    fn from_token(token: i64) -> Self {
        WorkSource {
            uid: token as u32 as binder::uid_t,
            propagate: (token >> WORK_SOURCE_PROPAGATED_BIT_INDEX) & 1 == 1,
        }
    }
}

thread_local! {
    // Not a `ThreadState` field: it must work without `ProcessState` (pure-RPC, idle clients).
    static WORK_SOURCE: Cell<WorkSource> = const { Cell::new(WorkSource::UNSET) };
}

fn work_source() -> WorkSource {
    WORK_SOURCE.with(Cell::get)
}

fn replace_work_source(ws: WorkSource) -> WorkSource {
    WORK_SOURCE.with(|c| c.replace(ws))
}

/// Unset work source for one inbound dispatch (AOSP `BR_TRANSACTION`); restored on drop.
pub(crate) struct WorkSourceDispatchGuard {
    saved: WorkSource,
}

impl WorkSourceDispatchGuard {
    pub(crate) fn enter() -> Self {
        WorkSourceDispatchGuard {
            saved: replace_work_source(WorkSource::UNSET),
        }
    }
}

impl Drop for WorkSourceDispatchGuard {
    fn drop(&mut self) {
        let _ = WORK_SOURCE.try_with(|c| c.set(self.saved));
    }
}

/// Set the uid of the work being done on behalf of the current caller and
/// mark it for propagation, so every outgoing kernel binder call made from
/// this thread carries it in the request header until it is cleared or
/// restored. Returns a token for [`restore_calling_work_source`].
///
/// The work source is per-thread state that exists outside transactions:
/// a client thread sets it before calling, and a server reads what the
/// caller sent with [`get_calling_work_source_uid`]. A value received in a
/// transaction is **not** propagated further unless the handler sets it
/// again. An inbound dispatch starts from the unset value and the thread's
/// previous value comes back when the handler returns.
///
/// Mirrors AOSP `IPCThreadState::setCallingWorkSourceUid`
/// (`IPCThreadState.cpp:589`) and Java `Binder.setCallingWorkSourceUid`.
///
/// # Transports
///
/// Only the kernel binder request header has a work-source field; the RPC
/// wire format (AOSP's included) has none. Inside an RPC handler,
/// [`get_calling_work_source_uid`] therefore reports the unset value, and a
/// value set there propagates only to kernel binder calls the handler
/// makes. Pure-RPC processes can call these functions freely.
pub fn set_calling_work_source_uid(uid: binder::uid_t) -> i64 {
    replace_work_source(WorkSource {
        uid,
        propagate: true,
    })
    .token()
}

/// Keeps the propagation flag; `check_interface` installs the header's value (AOSP internal).
pub(crate) fn set_calling_work_source_uid_without_propagation(uid: binder::uid_t) -> i64 {
    let current = work_source();
    replace_work_source(WorkSource { uid, ..current }).token()
}

/// The work source uid of the current thread: inside a kernel binder
/// handler, the value the caller sent (`u32::MAX` when it sent none);
/// elsewhere, whatever this thread last set. The unset value is AOSP
/// `kUnsetWorkSource` (`-1`) viewed as `uid_t`.
///
/// Mirrors AOSP `IPCThreadState::getCallingWorkSourceUid`
/// (`IPCThreadState.cpp:614`). See [`set_calling_work_source_uid`] for the
/// propagation rules and the RPC behavior.
pub fn get_calling_work_source_uid() -> binder::uid_t {
    work_source().uid
}

/// Set the work source to unset **and mark it for propagation**, so
/// outgoing kernel calls send the unset value rather than one inherited
/// from an earlier [`set_calling_work_source_uid`]. Returns a token for
/// [`restore_calling_work_source`].
///
/// Mirrors AOSP `IPCThreadState::clearCallingWorkSource`
/// (`IPCThreadState.cpp:619`), which is `setCallingWorkSourceUid(-1)`.
pub fn clear_calling_work_source() -> i64 {
    set_calling_work_source_uid(UNSET_WORK_SOURCE as binder::uid_t)
}

/// Restore the work source uid and propagation flag saved in a token from
/// [`set_calling_work_source_uid`] or [`clear_calling_work_source`].
///
/// Mirrors AOSP `IPCThreadState::restoreCallingWorkSource`
/// (`IPCThreadState.cpp:624`). The token is only meaningful to this
/// function; its bit layout is not a stable interface.
pub fn restore_calling_work_source(token: i64) {
    replace_work_source(WorkSource::from_token(token));
}

/// Stop propagating the work source on outgoing calls; the uid itself is
/// kept. Mirrors AOSP `IPCThreadState::clearPropagateWorkSource`
/// (`IPCThreadState.cpp:604`).
pub fn clear_propagate_work_source() {
    let current = work_source();
    replace_work_source(WorkSource {
        propagate: false,
        ..current
    });
}

/// Whether outgoing kernel binder calls from this thread carry the work
/// source uid (otherwise they send the unset value). Mirrors AOSP
/// `IPCThreadState::shouldPropagateWorkSource` (`IPCThreadState.cpp:609`).
pub fn should_propagate_work_source() -> bool {
    work_source().propagate
}

pub(crate) fn _setup_polling() -> Result<()> {
    THREAD_STATE.with(|thread_state| -> Result<()> {
        thread_state
            .borrow_mut()
            .out_parcel
            .write_cmd::<u32>(&binder::BC_ENTER_LOOPER)
    })?;
    flush_commands()?;
    Ok(())
}

enum UntilResponse {
    Reply,
    TransactionComplete,
    /// Unreachable (no `BC_ATTEMPT_ACQUIRE` under the cache pin); keeps the match exhaustive.
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
            let cmd: u32 = thread_state.borrow_mut().in_parcel.read_cmd::<i32>()? as _;

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
                binder::BR_TRANSACTION_PENDING_FROZEN => {
                    log::warn!("Sending oneway calls to frozen process.");
                    break;
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
                    let result = thread_state.borrow_mut().in_parcel.read_cmd::<i32>()?;
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
                    let tr = thread_state.borrow_mut().in_parcel.read_transaction()?;
                    // SAFETY: a kernel BR_REPLY populates the `data.ptr` union arm.
                    let (buffer, offsets) = unsafe { (tr.data.ptr.buffer, tr.data.ptr.offsets) };
                    if let UntilResponse::Reply = until {
                        if (tr.flags & transaction_flags_TF_STATUS_CODE) == 0 {
                            // SAFETY: sized driver buffer, unshared until `free_buffer`.
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
                            // SAFETY: guarded by the size check; buffer live until `free_buffer`.
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
                            // Status 0 returns too, as AOSP: module doc "Dispatch notes".
                            return Ok(Some(Parcel::new()));
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

/// `BR_DEAD_BINDER` phases; only a `queue_done` error skips `pin_release`. See module doc.
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
    // Phase 1: dispatch recipients; never short-circuit, the handshake must complete.
    let obituary_result = obituary();

    // Phase 2: a failure skips phase 3 (see fn doc); log the obituary error it would hide.
    if let Err(qe) = queue_done() {
        if let Err(oe) = &obituary_result {
            error!(
                "BR_DEAD_BINDER: queue BC_DEAD_BINDER_DONE failed ({qe:?}) \
                 swallowing obituary error {oe:?} for handle {handle:X}"
            );
        }
        return Err(qe);
    }

    // Phase 3: always release the pin; log its error now, as the obituary error wins below.
    let pin_result = pin_release();
    if let Err(e) = &pin_result {
        error!(
            "release_obituary_pin failed for handle {handle:X}: {e:?}; \
             obituary_result: {obituary_result:?}"
        );
    }

    // The user-visible obituary error wins; the pin error surfaces only on its own.
    obituary_result?;
    pin_result?;
    Ok(())
}

/// `transact` with panics caught; call with no borrow held (R1). See module doc "Handler panics".
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
            // Discard the partial reply so the client never parses half-formed data.
            *reply = Parcel::new();
            Err(StatusCode::Unknown)
        }
    }
}

/// [`dispatch_transact_caught`] between the observer's calls, no `THREAD_STATE` borrow held.
fn dispatch_kernel_observed(
    binder: &SIBinder,
    transactable: &dyn Transactable,
    tr: &binder::binder_transaction_data,
    reader: &mut Parcel,
    reply: &mut Parcel,
) -> Result<()> {
    let code = tr.code;
    crate::observe::observed(
        // `descriptor()` is user code too, so it is looked up inside the caught closure.
        || crate::observe::TxnContext {
            descriptor: binder.descriptor(),
            code,
            method: transactable.transaction_name(code),
            is_oneway: tr.flags & transaction_flags_TF_ONE_WAY != 0,
            calling_uid: tr.sender_euid,
            calling_pid: tr.sender_pid,
            transport: crate::TransportCaps::KERNEL,
        },
        || dispatch_transact_caught(transactable, code, reader, reply),
    )
}

fn execute_command(cmd: i32) -> Result<()> {
    let cmd: std::os::raw::c_uint = cmd as _;

    THREAD_STATE.with(|thread_state| -> Result<()> {
        match cmd {
            binder::BR_ERROR => {
                let other: StatusCode = thread_state
                    .borrow_mut()
                    .in_parcel
                    .read_cmd::<i32>()?
                    .into();
                log::error!("binder::BR_ERROR ({other})");
                return Err(other);
            }
            binder::BR_OK => {}

            binder::BR_TRANSACTION_SEC_CTX | binder::BR_TRANSACTION => {
                let tr_secctx = {
                    let mut thread_state = thread_state.borrow_mut();
                    if cmd == binder::BR_TRANSACTION_SEC_CTX {
                        thread_state.in_parcel.read_transaction_secctx()?
                    } else {
                        binder::binder_transaction_data_secctx {
                            transaction_data: thread_state.in_parcel.read_transaction()?,
                            secctx: 0,
                        }
                    }
                };

                // SAFETY: kernel-filled `data.ptr` arm; sized, unshared until `free_buffer`.
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

                // TODO: AOSP `mServingStackPointer` is not tracked.

                let (transaction_old, strict_mode_policy_old) = {
                    let mut thread_state = thread_state.borrow_mut();
                    let transaction_old = thread_state.transaction;
                    let strict_mode_policy_old = thread_state.strict_mode_policy;

                    thread_state.transaction =
                        Some(TransactionState::from_transaction_data(&tr_secctx));

                    (transaction_old, strict_mode_policy_old)
                };
                // Unset until an AIDL stub's `check_interface` installs the caller's value.
                let _work_source = WorkSourceDispatchGuard::enter();

                // Nested IPC may enter from an RPC handler: suspend its calling context meanwhile.
                #[cfg(feature = "rpc")]
                let _rpc_suspended = RpcCallingGuard::suspend();

                let mut reply = Parcel::new();

                let result = {
                    // SAFETY: kernel txn to a local binder; `target.ptr` is the active arm.
                    let target_ptr = unsafe { tr_secctx.transaction_data.target.ptr };
                    if target_ptr != 0 {
                        // A `publish_native` id; ref balance: module doc "Dispatch notes".
                        let id = target_ptr;
                        match ProcessState::as_self().lookup_native(id) {
                            Some(arc) => {
                                let strong = SIBinder::from_arc(arc);
                                if strong.attempt_increase() {
                                    // May be `None` for a user `IBinder`: reject, no panic.
                                    let result = match strong.as_transactable() {
                                        Some(t) => dispatch_kernel_observed(
                                            &strong,
                                            t,
                                            &tr_secctx.transaction_data,
                                            &mut reader,
                                            &mut reply,
                                        ),
                                        None => {
                                            log::error!("native id {id} is not Transactable");
                                            Err(StatusCode::UnknownTransaction)
                                        }
                                    };
                                    // No `?`: the reply and state restore must still run.
                                    if let Err(e) = strong.decrease() {
                                        log::error!("dec_strong failed for native id {id}: {e:?}");
                                    }
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
                        match ProcessState::as_self().context_manager() {
                            Some(context) => match context.as_transactable() {
                                Some(t) => dispatch_kernel_observed(
                                    &context,
                                    t,
                                    &tr_secctx.transaction_data,
                                    &mut reader,
                                    &mut reply,
                                ),
                                None => {
                                    log::error!("context manager is not Transactable");
                                    Err(StatusCode::UnknownTransaction)
                                }
                            },
                            None => {
                                log::error!(
                                    "BR_TRANSACTION to handle 0 but no context manager set"
                                );
                                Err(StatusCode::DeadObject)
                            }
                        }
                    }
                };
                // Freed before the reply, as AOSP `executeCommand` does (b/238777741).
                drop(reader);
                // `calling_sid` points into the freed buffer; a death recipient must not read it.
                if let Some(tr) = thread_state.borrow_mut().transaction.as_mut() {
                    tr.calling_sid = std::ptr::null();
                }
                let flags = tr_secctx.transaction_data.flags;
                // Outside the catch so the panic arm can rewind; see module doc "Dispatch notes".
                let queued_at = thread_state.borrow().unflushed_mark();
                let reply_result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        if (flags & transaction_flags_TF_ONE_WAY) == 0 {
                            let flags = flags & transaction_flags_TF_CLEAR_BUF;
                            let status: i32 = match result {
                                Ok(_) => StatusCode::Ok.into(),
                                Err(err) => err.into(),
                            };
                            // BC_REPLY points at `reply`/`status`: a failed flush must rewind it.
                            thread_state.borrow_mut().write_transaction_data(
                                binder::BC_REPLY,
                                flags,
                                u32::MAX,
                                0,
                                &reply,
                                &status,
                            )?;
                            if let Err(e) = wait_for_response(UntilResponse::TransactionComplete) {
                                discard_unflushed_commands(thread_state, queued_at, true);
                                return Err(e);
                            }
                        } else if let Err(err) = result {
                            let mut log = format!(
                                "oneway function results for code {} on binder at {:X}",
                                tr_secctx.transaction_data.code,
                                // SAFETY: kernel txn; `target.ptr` is the active arm.
                                unsafe { tr_secctx.transaction_data.target.ptr }
                            );
                            log += &format!(" will be dropped but finished with status {err}");

                            if reply.data_size() != 0 {
                                log += &format!(" and reply parcel size {}", reply.data_size());
                            }
                            log::error!("{log}");
                        }
                        Ok(())
                    }));

                {
                    // Undo `check_interface`'s policy overwrite, as AOSP `executeCommand` does.
                    let mut thread_state = thread_state.borrow_mut();
                    thread_state.transaction = transaction_old;
                    thread_state.strict_mode_policy = strict_mode_policy_old;
                }

                match reply_result {
                    Ok(inner) => inner?,
                    Err(_payload) => {
                        // Rewind only this reply's bytes, no retry (module doc "Dispatch notes").
                        discard_unflushed_commands(thread_state, queued_at, false);
                        log::error!(
                            "reply path panicked for code {}; reply dropped",
                            tr_secctx.transaction_data.code
                        );
                        return Err(StatusCode::Unknown);
                    }
                }
            }

            binder::BR_INCREFS => {
                let mut state = thread_state.borrow_mut();
                let id = state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?;
                // Cookie (0 for our natives) is echoed verbatim in BC_INCREFS_DONE.
                let cookie_echo = state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?;
                drop(state);

                // Id bookkeeping only; the table holds RefCounter.weak up for the entry's life.
                if ProcessState::as_self().ref_native_kernel(id).is_none() {
                    log::error!("BR_INCREFS for unknown native id {id}");
                    debug_assert!(false, "BR_INCREFS for unknown native id {id}");
                }

                let mut state = thread_state.borrow_mut();
                state
                    .out_parcel
                    .write_cmd::<u32>(&binder::BC_INCREFS_DONE)?;
                state
                    .out_parcel
                    .write_cmd::<binder::binder_uintptr_t>(&id)?;
                state
                    .out_parcel
                    .write_cmd::<binder::binder_uintptr_t>(&cookie_echo)?;
            }
            binder::BR_ACQUIRE => {
                let mut state = thread_state.borrow_mut();
                let id = state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?;
                let cookie_echo = state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?;
                drop(state);

                // Bookkeeping only, as BR_INCREFS; `binder_pin` holds RefCounter.strong up.
                if ProcessState::as_self().ref_native_kernel(id).is_none() {
                    log::error!("BR_ACQUIRE for unknown native id {id}");
                    debug_assert!(false, "BR_ACQUIRE for unknown native id {id}");
                }

                let mut state = thread_state.borrow_mut();
                state
                    .out_parcel
                    .write_cmd::<u32>(&(binder::BC_ACQUIRE_DONE))?;
                state
                    .out_parcel
                    .write_cmd::<binder::binder_uintptr_t>(&id)?;
                state
                    .out_parcel
                    .write_cmd::<binder::binder_uintptr_t>(&cookie_echo)?;
            }
            binder::BR_RELEASE => {
                let mut state = thread_state.borrow_mut();
                let id = state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?;
                // cookie echo unused on the deferred-deref path.
                let _cookie_echo = state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?;

                BINDER_DEREFS.with(|binder_derefs| {
                    let mut binder_derefs = binder_derefs.borrow_mut();
                    binder_derefs.pending_strong_derefs.push_back(id);
                });
            }
            binder::BR_DECREFS => {
                let mut state = thread_state.borrow_mut();
                let id = state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?;
                let _cookie_echo = state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?;

                BINDER_DEREFS.with(|binder_derefs| {
                    let mut binder_derefs = binder_derefs.borrow_mut();
                    binder_derefs.pending_weak_derefs.push_back(id);
                });
            }
            binder::BR_ATTEMPT_ACQUIRE => {
                let mut state = thread_state.borrow_mut();
                let id = state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?;
                let _cookie_echo = state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?;
                drop(state);

                // Alive entry ⟹ promote; an unknown id may race unpublish, so no debug_assert.
                let success = ProcessState::as_self().ref_native_kernel(id).is_some();

                let mut state = thread_state.borrow_mut();
                state
                    .out_parcel
                    .write_cmd::<u32>(&binder::BC_ACQUIRE_RESULT)?;
                state.out_parcel.write_cmd::<i32>(&(success as _))?;
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
                    state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?
                };

                log::trace!("BR_DEAD_BINDER: handle {handle:X}");

                // `binder_died` is not an RPC dispatch; hide the suspended RPC peer from it.
                #[cfg(feature = "rpc")]
                let _rpc_suspended = RpcCallingGuard::suspend();

                drive_dead_binder_handshake(
                    handle,
                    || ProcessState::as_self().send_obituary_for_handle(handle as _),
                    || {
                        let mut state = thread_state.borrow_mut();
                        state
                            .out_parcel
                            .write_cmd::<u32>(&(binder::BC_DEAD_BINDER_DONE))?;
                        state
                            .out_parcel
                            .write_cmd::<binder::binder_uintptr_t>(&handle)?;
                        Ok(())
                    },
                    || ProcessState::as_self().release_obituary_pin(handle as _),
                )?;
            }
            binder::BR_CLEAR_DEATH_NOTIFICATION_DONE => {
                let mut state = thread_state.borrow_mut();
                state.in_parcel.read_cmd::<binder::binder_uintptr_t>()?;
            }
            _ => {
                log::error!("*** BAD COMMAND {cmd} received from Binder driver\n");
                return Err(StatusCode::Unknown);
            }
        };

        Ok(())
    })
}

/// Names the command at `stopped_at` (`write_consumed`); see module doc "Refused commands".
fn describe_refused_command(out: &mut CommandStream, stopped_at: usize) -> String {
    let total = out.data_size();
    if stopped_at >= total {
        return format!("consumed {stopped_at} of {total}");
    }
    let saved = out.data_position();
    out.set_data_position(stopped_at);
    let named = match out.read_cmd::<u32>() {
        Ok(cmd) => format!("{} ({cmd:#x})", command_to_str(cmd)),
        Err(e) => format!("<unreadable: {e}>"),
    };
    out.set_data_position(saved);
    format!("consumed {stopped_at} of {total}; driver stopped at {named}")
}

/// One driver round trip; see module doc "R1 at specific call sites" for its ioctl borrow.
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

        ensure_thread_exit_guard(&thread_state.borrow().driver);

        loop {
            // SAFETY: `bwr` points at `out_parcel` (`data_size`) and `in_parcel` (`capacity`).
            let res = unsafe { binder::write_read(&thread_state.borrow().driver, &mut bwr) };
            match res {
                Ok(_) => break,
                Err(errno) if errno != rustix::io::Errno::INTR => {
                    // A bad command fails the whole ioctl, so caller bugs surface here, not below.
                    let detail = describe_refused_command(
                        &mut thread_state.borrow_mut().out_parcel,
                        bwr.write_consumed as _,
                    );
                    log::error!("binder::write_read() error : {errno}; {detail}");
                    return Err(StatusCode::from(errno));
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
                    let detail = describe_refused_command(
                        &mut thread_state.out_parcel,
                        bwr.write_consumed as _,
                    );
                    // stderr, not `log`: a consumer with no logger would otherwise die mute.
                    eprintln!(
                        "rsbinder FATAL: driver did not consume the write buffer — {detail}\n\
                         The remainder was never seen by the kernel, so the reference\n\
                         counts and buffer ownership this process believes in are no\n\
                         longer the kernel's. Queued commands:\n{:?}",
                        thread_state.out_parcel
                    );
                    // Abort like AOSP: a caught panic would resume on the desynced stream.
                    std::process::abort();
                }
                thread_state.out_parcel.set_data_size(0)?;
                thread_state.out_flush_epoch += 1;
            }

            if bwr.read_consumed > 0 {
                // SAFETY: the driver just filled `0..read_consumed` through `read_buffer`.
                unsafe {
                    thread_state
                        .in_parcel
                        .set_data_size_driver_filled(bwr.read_consumed as _)?;
                }
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
        thread_state.borrow_mut().in_parcel.read_cmd::<i32>()
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
            log::warn!("self.out_parcel.len() > 0 after flush_commands()");
        }

        Ok(())
    })
}

pub(crate) fn inc_strong_handle(handle: u32) -> Result<()> {
    log::trace!("inc_strong_handle: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write_cmd::<u32>(&(binder::BC_ACQUIRE))?;
            state.out_parcel.write_cmd::<u32>(&(handle))?;
        }

        flush_if_needed()?;

        Ok(())
    })
}

// `THREAD_STATE` torn down at thread exit: write one command directly, then exit the thread again.
fn write_without_thread_state<T: NativeScalar>(cmd: u32, arg: T) -> Result<()> {
    let mut out = CommandStream::new();
    out.write_cmd::<u32>(&cmd)?;
    out.write_cmd::<T>(&arg)?;
    let driver = ProcessState::as_self().driver();
    let mut bwr = binder::binder_write_read {
        write_size: out.data_size() as _,
        write_consumed: 0,
        write_buffer: out.as_mut_ptr() as _,
        read_size: 0,
        read_consumed: 0,
        read_buffer: 0,
    };
    loop {
        // SAFETY: `write_buffer` is the live local `out` (`data_size` bytes); nothing is read.
        match unsafe { binder::write_read(&*driver, &mut bwr) } {
            Ok(()) => break,
            Err(errno) if errno == rustix::io::Errno::INTR => {}
            Err(errno) => {
                log::error!("binder::write_read() after thread-local teardown: {errno}");
                return Err(StatusCode::from(errno));
            }
        }
    }
    if let Err(e) = binder::thread_exit(&*driver, 0) {
        log::warn!("BINDER_THREAD_EXIT after thread-local teardown failed: {e}");
    }
    Ok(())
}

pub(crate) fn dec_strong_handle(handle: u32) -> Result<()> {
    log::trace!("dec_strong_handle: {handle}");
    let queued = THREAD_STATE.try_with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write_cmd::<u32>(&(binder::BC_RELEASE))?;
            state.out_parcel.write_cmd::<u32>(&(handle))?;
        }

        flush_if_needed()?;

        Ok(())
    });
    match queued {
        Ok(result) => result,
        Err(_) => write_without_thread_state::<u32>(binder::BC_RELEASE, handle),
    }
}

pub(crate) fn inc_weak_handle(handle: u32) -> Result<()> {
    log::trace!("inc_weak_handle: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write_cmd::<u32>(&(binder::BC_INCREFS))?;
            state.out_parcel.write_cmd::<u32>(&(handle))?;
        }

        flush_if_needed()?;

        Ok(())
    })
}

pub(crate) fn dec_weak_handle(handle: u32) -> Result<()> {
    log::trace!("dec_weak_handle: {handle}");
    let queued = THREAD_STATE.try_with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write_cmd::<u32>(&(binder::BC_DECREFS))?;
            state.out_parcel.write_cmd::<u32>(&(handle))?;
        }

        flush_if_needed()?;

        Ok(())
    });
    match queued {
        Ok(result) => result,
        Err(_) => write_without_thread_state::<u32>(binder::BC_DECREFS, handle),
    }
}

pub(crate) fn flush_if_needed() -> Result<bool> {
    THREAD_STATE.with(|thread_state| -> Result<bool> {
        {
            let thread_state = thread_state.borrow();
            if thread_state.is_looper || thread_state.is_flushing {
                return Ok(false);
            }
        }

        // Reset is_flushing on every exit, or later flushes wedge; no borrow held across (R1).
        struct FlushGuard;
        impl Drop for FlushGuard {
            fn drop(&mut self) {
                THREAD_STATE.with(|ts| ts.borrow_mut().is_flushing = false);
            }
        }

        thread_state.borrow_mut().is_flushing = true;
        let _guard = FlushGuard;
        flush_commands()?;

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

pub(crate) fn check_interface(reader: &mut Parcel, descriptor: &str) -> Result<bool> {
    let mut strict_policy: i32 = reader.read()?;

    THREAD_STATE.with(|thread_state| -> Result<()> {
        let mut thread_state = thread_state.borrow_mut();

        if (thread_state.last_transaction_binder_flags() & FLAG_ONEWAY) != 0 {
            strict_policy = 0;
        }
        thread_state.set_strict_mode_policy(strict_policy);
        Ok(())
    })?;

    reader.update_work_source_request_header_pos();
    let work_source: i32 = reader.read()?;
    set_calling_work_source_uid_without_propagation(work_source as _);

    if crate::sdk_at_least(30) {
        let header: u32 = reader.read()?;
        if header != INTERFACE_HEADER {
            log::error!("Expecting header {INTERFACE_HEADER:#x} but found {header:#x}.");
            return Ok(false);
        }
    }

    match crate::parcelable::read_string16_matches(reader, descriptor)? {
        Ok(()) => Ok(true),
        Err(parcel_interface) => {
            log::error!("check_interface() expected '{descriptor}' but read '{parcel_interface}'");
            Ok(false)
        }
    }
}

pub(crate) fn transact(
    handle: u32,
    code: u32,
    data: &Parcel,
    mut flags: u32,
) -> Result<Option<Parcel>> {
    flags |= transaction_flags_TF_ACCEPT_FDS;

    // Checked before queuing, unlike AOSP: see module doc "Dispatch notes".
    if (flags & transaction_flags_TF_ONE_WAY) == 0 {
        match call_restriction() {
            CallRestriction::ErrorIfNotOneway => {
                error!("Process making non-oneway call (code: {code}) but is restricted.")
            }
            CallRestriction::FatalIfNotOneway => {
                panic!("Process may not make non-oneway calls (code: {code}).");
            }
            _ => (),
        }
    }

    // The queued BC_TRANSACTION points into `data`: rewind it if the flush fails.
    let queued_at = THREAD_STATE.with(|thread_state| -> Result<(u64, usize)> {
        let mut thread_state = thread_state.borrow_mut();
        let queued_at = thread_state.unflushed_mark();
        thread_state.write_transaction_data(
            binder::BC_TRANSACTION,
            flags,
            handle,
            code,
            data,
            &0,
        )?;
        Ok(queued_at)
    })?;

    let waited = if (flags & transaction_flags_TF_ONE_WAY) == 0 {
        wait_for_response(UntilResponse::Reply)
    } else {
        wait_for_response(UntilResponse::TransactionComplete)
    };
    match waited {
        Ok(reply) => Ok(reply),
        Err(e) => {
            THREAD_STATE
                .with(|thread_state| discard_unflushed_commands(thread_state, queued_at, false));
            Err(e)
        }
    }
}

/// Rewinds `out_parcel` to `queued_at`; `retry_flush` is `BC_REPLY` only. See module doc.
fn discard_unflushed_commands(
    thread_state: &RefCell<ThreadState>,
    queued_at: (u64, usize),
    retry_flush: bool,
) {
    if retry_flush {
        if let Err(e) = talk_with_driver(false) {
            log::warn!("flush after failed reply also failed: {e}");
        }
    }
    let (epoch, queued_at) = queued_at;
    let mut ts = thread_state.borrow_mut();
    if ts.out_flush_epoch == epoch && ts.out_parcel.data_size() > queued_at {
        log::error!(
            "discarding {} unflushed out_parcel bytes that reference released memory",
            ts.out_parcel.data_size() - queued_at
        );
        let _ = ts.out_parcel.set_data_size(queued_at);
    }
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

    let queued = THREAD_STATE.try_with(|thread_state| -> Result<()> {
        let mut thread_state = thread_state.borrow_mut();
        thread_state
            .out_parcel
            .write_cmd::<u32>(&binder::BC_FREE_BUFFER)?;
        thread_state
            .out_parcel
            .write_cmd::<binder_uintptr_t>(&data)?;
        Ok(())
    });
    match queued {
        Ok(result) => result?,
        Err(_) => {
            return write_without_thread_state::<binder_uintptr_t>(binder::BC_FREE_BUFFER, data)
        }
    }

    flush_if_needed()?;

    Ok(())
}

pub(crate) fn query_interface(handle: u32) -> Result<String> {
    #[cfg(all(target_os = "android", feature = "android_10"))]
    if handle == 0 && !crate::sdk_at_least(30) {
        return Ok(crate::hub::android_10::SERVICE_MANAGER_DESCRIPTOR.to_owned());
    }

    let data = Parcel::new();
    let reply = transact(handle, INTERFACE_TRANSACTION, &data, 0)?;
    // `Ok(None)` is an error, not a panic; a null descriptor folds to empty (AOSP readString16).
    let interface: Option<String> = reply.ok_or(StatusCode::UnexpectedNull)?.read()?;

    Ok(interface.unwrap_or_default())
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

        // Decrement on every exit, `?` included, or `current_threads` stays inflated.
        struct ThreadCountGuard;
        impl Drop for ThreadCountGuard {
            fn drop(&mut self) {
                ProcessState::as_self()
                    .current_threads
                    .fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _thread_count_guard = ThreadCountGuard;

        let looper = if is_main {
            binder::BC_ENTER_LOOPER
        } else {
            binder::BC_REGISTER_LOOPER
        };

        {
            let mut thread_state = thread_state.borrow_mut();
            thread_state.out_parcel.write_cmd::<u32>(&looper)?;
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
                        if errno == -(rustix::io::Errno::CONNREFUSED.raw_os_error()) =>
                    {
                        result = e;
                        break;
                    }
                    _ => {
                        // Other errors leave too, not abort: the pool can spawn a replacement.
                        log::error!(
                            "get_and_execute_command() returned unexpected error {e}; \
                             leaving the thread pool"
                        );
                        result = e;
                        break;
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
            // Flag first: a stuck `is_looper` would stop `flush_if_needed` for good.
            thread_state.is_looper = false;
            thread_state
                .out_parcel
                .write_cmd::<u32>(&binder::BC_EXIT_LOOPER)?;
        }

        talk_with_driver(false)?;
        // `_thread_count_guard` decrements `current_threads` on drop.
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
                .write_cmd::<u32>(&(binder::BC_REQUEST_DEATH_NOTIFICATION))?;
            state.out_parcel.write_cmd::<u32>(&(handle))?;
            // Android binder calls writePointer(proxy) here, but we just write handle.
            state
                .out_parcel
                .write_cmd::<binder::binder_uintptr_t>(&(handle as _))?;
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
                .write_cmd::<u32>(&(binder::BC_CLEAR_DEATH_NOTIFICATION))?;
            state.out_parcel.write_cmd::<u32>(&(handle))?;
            // Android binder calls writePointer(proxy) here, but we just write handle.
            state
                .out_parcel
                .write_cmd::<binder::binder_uintptr_t>(&(handle as _))?;
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
        // An in-flight RPC transaction wins (pure-RPC safe); RPC carries no SELinux `sid`.
        if let Some((uid, pid)) = rpc_calling() {
            return CallingContext {
                pid,
                uid,
                sid: None,
            };
        }
        // Pure-RPC process: `THREAD_STATE` would panic; answer with the self identity.
        if !ProcessState::is_initialized() {
            return CallingContext {
                pid: rustix::process::getpid().as_raw_nonzero().get() as _,
                uid: rustix::process::getuid().as_raw(),
                sid: None,
            };
        }
        THREAD_STATE.with(|thread_state| -> CallingContext {
            let thread_state = thread_state.borrow();
            match thread_state.transaction.as_ref() {
                Some(transaction) => {
                    let calling_sid = if !transaction.calling_sid.is_null() {
                        // SAFETY: non-null = live NUL-terminated kernel sid (nulled on free).
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

pub(crate) fn is_handling_transaction() -> bool {
    // An in-flight RPC transaction counts, detected without forcing `THREAD_STATE`.
    if rpc_calling().is_some() {
        return true;
    }
    ProcessState::is_initialized()
        && THREAD_STATE.with(|thread_state| thread_state.borrow().transaction.is_some())
}

/// SELinux security context of the caller for the current in-flight
/// `BR_TRANSACTION`, when present.
///
/// `Some` is only ever returned while the current thread is dispatching a
/// transaction targeting a binder constructed with
/// `BinderFeatures { set_requesting_sid: true, .. }`. The kernel then
/// delivers the request via `BR_TRANSACTION_SEC_CTX` and rsbinder copies
/// the null-terminated SELinux context (e.g. `u:r:system_server:s0`) into
/// the returned `CString`.
///
/// Returns `None` when:
/// - The thread is not currently dispatching a binder transaction.
/// - The transaction was delivered as plain `BR_TRANSACTION` (caller's
///   binder did not request the security context).
/// - The transaction came over the RPC transport (RPC has its own
///   `PeerIdentity` model — see `rsbinder::rpc::PeerIdentity`).
///
/// Equivalent to AOSP `IPCThreadState::getCallingSid()` (libbinder
/// `frameworks/native/libs/binder/IPCThreadState.cpp`) and Android Rust
/// `libbinder_rs::ThreadState::with_calling_sid` (rsbinder returns an
/// owned `CString` instead of taking a `&CStr` callback — the kernel
/// pointer is lazily copied at every call, so leaking is not possible).
///
pub fn get_calling_sid() -> Option<CString> {
    // RPC has no SELinux context; return early without forcing `THREAD_STATE`.
    if rpc_calling().is_some() || !ProcessState::is_initialized() {
        return None;
    }
    THREAD_STATE.with(|thread_state| {
        let thread_state = thread_state.borrow();
        let transaction = thread_state.transaction.as_ref()?;
        if transaction.calling_sid.is_null() {
            return None;
        }
        // SAFETY: non-null = kernel NUL-terminated sid, valid until BC_FREE_BUFFER nulls it.
        Some(unsafe { CStr::from_ptr(transaction.calling_sid as _).to_owned() })
    })
}

/// PID of the caller for the current in-flight binder transaction.
///
/// Returns the sender PID delivered by the kernel via
/// `binder_transaction_data.sender_pid` when this thread is dispatching a
/// `BR_TRANSACTION` / `BR_TRANSACTION_SEC_CTX`, and this process's own
/// pid (`getpid(2)`) when not handling a transaction — AOSP
/// `IPCThreadState::getCallingPid()`, whose `clearCaller()` sets
/// `mCallingPid = getpid()` (`IPCThreadState.cpp`). A call that never
/// crossed a process boundary is this process calling itself.
///
/// Convenience wrapper around `CallingContext::default().pid` for the
/// common case where only the PID is needed — avoids the
/// `Option<CString>` allocation of the full context.
pub fn get_calling_pid() -> binder::pid_t {
    // An RPC caller's pid (`-1` without one) wins, read without forcing `THREAD_STATE`.
    if let Some((_, pid)) = rpc_calling() {
        return pid;
    }
    let own_pid = || rustix::process::getpid().as_raw_nonzero().get() as binder::pid_t;
    if !ProcessState::is_initialized() {
        return own_pid();
    }
    THREAD_STATE
        .with(|thread_state| {
            thread_state
                .borrow()
                .transaction
                .as_ref()
                .map(|tr| tr.calling_pid)
        })
        .unwrap_or_else(own_pid)
}

/// UID of the caller for the current in-flight binder transaction.
///
/// Returns the sender UID delivered by the kernel via
/// `binder_transaction_data.sender_euid` when this thread is dispatching
/// a `BR_TRANSACTION` / `BR_TRANSACTION_SEC_CTX`, and this process's own
/// uid (`getuid(2)`) when not handling a transaction — AOSP
/// `IPCThreadState::getCallingUid()` returns `getuid()` whenever no
/// caller is recorded. An in-process call, or a check made on a thread
/// the transaction did not arrive on, therefore reads as this process,
/// never as root.
///
/// Convenience wrapper around `CallingContext::default().uid` for the
/// common case where only the UID is needed.
///
/// Note: this is the *kernel-delivered* sender UID. To temporarily
/// replace it for a nested outbound call (the AOSP
/// `clearCallingIdentity` pattern), see [`clear_calling_identity`] and
/// [`restore_calling_identity`].
///
/// # RPC transports (Plan 2-16 Phase B)
///
/// Over a **Unix-domain RPC** connection this returns the kernel-vouched
/// peer uid (`SO_PEERCRED` on Linux/Android, `getpeereid` on macOS/BSD) —
/// so a hand-rolled uid ACL runs transport-agnostically on kernel binder
/// *and* Unix RPC. The granularity is **connection-level**, not
/// per-method (an RPC connection is opened once by one peer process).
///
/// Over RPC transports that carry **no** uid (`Vsock` cid, TLS
/// `Certificate`, `Anonymous`) this returns a **fail-closed sentinel**
/// (`u32::MAX`, never `0`/root and never a real privileged uid) — uid is
/// the wrong authorization basis there; use `PeerIdentity` /
/// `RpcServer::set_authorizer` instead. `@EnforcePermission` over RPC is
/// always denied regardless of uid (Plan 2-16 Phase A).
pub fn get_calling_uid() -> binder::uid_t {
    // An RPC uid wins (fail-closed sentinel on non-uid transports), pure-RPC safe.
    if let Some((uid, _)) = rpc_calling() {
        return uid;
    }
    if !ProcessState::is_initialized() {
        return rustix::process::getuid().as_raw();
    }
    THREAD_STATE
        .with(|thread_state| {
            thread_state
                .borrow()
                .transaction
                .as_ref()
                .map(|tr| tr.calling_uid)
        })
        .unwrap_or_else(|| rustix::process::getuid().as_raw())
}

/// Temporarily override the kernel-delivered calling identity with the
/// current process's own uid/pid, and return an opaque token that
/// [`restore_calling_identity`] uses to reverse the override.
///
/// Mirrors AOSP `IPCThreadState::clearCallingIdentity()`
/// (`frameworks/native/libs/binder/IPCThreadState.cpp:562`). Typical use
/// is server-side: a transaction handler that needs to make an outgoing
/// binder call to another service "as itself" rather than as the original
/// caller, to avoid leaking the caller's privileges into a downstream
/// permission check (or, conversely, to *deliberately* drop a privileged
/// caller's identity before delegating).
///
/// ```text
/// // Inside Transactable::transact:
/// let token = clear_calling_identity();
/// let result = downstream_service.do_something(...);
/// restore_calling_identity(token);
/// ```
///
/// The returned token packs `(has_explicit_identity, calling_uid,
/// calling_pid)` into 64 bits using the AOSP bit layout
/// (`packCallingIdentity` in IPCThreadState.cpp). The SELinux SID is
/// **not** preserved — matching the AOSP comment "ignore mCallingSid for
/// legacy reasons". After [`restore_calling_identity`], `get_calling_sid`
/// returns `None`.
///
/// # Behavior outside a transaction
///
/// Returns `0` and is a no-op when not currently dispatching a binder
/// transaction (no kernel-delivered identity exists to clear). This
/// differs from AOSP, which stores calling identity in flat
/// `IPCThreadState` fields that persist between transactions — but the
/// AOSP user-facing semantics (clear before downstream call, restore
/// after) match.
///
/// # Behavior during an RPC transaction
///
/// **Also a no-op, and the RPC peer stays visible.** While an RPC
/// calling context is installed this function returns `0` and changes
/// nothing; any kernel transaction below it on the stack belongs to an
/// outer frame. [`get_calling_uid`] / [`get_calling_pid`] keep answering
/// for the peer, so the AOSP idiom `let t = clear_calling_identity(); …;
/// restore_calling_identity(t);` drops nothing over RPC. The 64-bit
/// token cannot carry a `PeerIdentity` back. Authorize an RPC caller
/// through [`calling_caller`] or `RpcServer::set_authorizer` instead.
pub fn clear_calling_identity() -> i64 {
    // An RPC dispatch is innermost: the kernel transaction under it is an outer frame's.
    if rpc_calling().is_some() || !ProcessState::is_initialized() {
        return 0;
    }
    THREAD_STATE.with(|thread_state| {
        let mut thread_state = thread_state.borrow_mut();
        let Some(ref mut tr) = thread_state.transaction else {
            return 0;
        };
        let token = pack_calling_identity(tr.has_explicit_identity, tr.calling_uid, tr.calling_pid);
        // AOSP `clearCaller()`: own uid/pid, SID dropped ("expensive to lookup").
        tr.calling_uid = rustix::process::getuid().as_raw();
        tr.calling_pid = rustix::process::getpid().as_raw_nonzero().get() as _;
        tr.calling_sid = std::ptr::null();
        tr.has_explicit_identity = true;
        token
    })
}

/// Reverse a previous [`clear_calling_identity`] by unpacking the token
/// and writing the saved uid/pid/has_explicit back into the current
/// transaction's calling identity.
///
/// Mirrors AOSP `IPCThreadState::restoreCallingIdentity(int64_t)`
/// (`IPCThreadState.cpp:645`). The SELinux SID is left as `None` because
/// the token has no room to preserve it ("not enough data to restore" —
/// same as AOSP). Callers needing both UID/PID and SID restoration must
/// save the SID separately before clearing.
///
/// # When this is a no-op
///
/// Under the same two conditions in which [`clear_calling_identity`]
/// returns `0` — outside a kernel transaction, and during an RPC
/// dispatch. The token is discarded.
///
/// # Token mismatch
///
/// rsbinder does **not** validate that `token` came from a matching
/// `clear_calling_identity()` call on the same thread/transaction — same
/// trust model as AOSP. Passing a forged or stale token simply sets the
/// fields to whatever the token decodes to. The `RAII` guard pattern
/// (caller wraps `clear`/`restore` in a `Drop` impl) is recommended; see
/// AOSP `IPCThreadState::CallingIdentityScope` for the C++ analogue.
pub fn restore_calling_identity(token: i64) {
    // Same gate as `clear_calling_identity`: an RPC handler's token is `0`.
    if rpc_calling().is_some() || !ProcessState::is_initialized() {
        return;
    }
    THREAD_STATE.with(|thread_state| {
        let mut thread_state = thread_state.borrow_mut();
        let Some(ref mut tr) = thread_state.transaction else {
            return;
        };
        tr.calling_uid = unpack_calling_uid(token);
        tr.calling_pid = unpack_calling_pid(token);
        tr.has_explicit_identity = unpack_has_explicit_identity(token);
        tr.calling_sid = std::ptr::null();
    })
}

/// Linux scheduler policy of the current thread, as reported by the
/// kernel via `sched_getscheduler(0)`. Debug helper to confirm that
/// `FLAT_BINDER_FLAG_INHERIT_RT` actually lifted the worker thread into
/// SCHED_FIFO / SCHED_RR for the transaction.
///
/// Values match `<linux/sched.h>` / `libc::SCHED_*` constants:
///
///   * `0` = `SCHED_NORMAL` (a.k.a. `SCHED_OTHER`)
///   * `1` = `SCHED_FIFO`
///   * `2` = `SCHED_RR`
///   * `3` = `SCHED_BATCH`
///   * `5` = `SCHED_IDLE`
///   * `6` = `SCHED_DEADLINE`
///
/// Linux + Android only — on macOS this returns `Err(InvalidOperation)`
/// because `SCHED_*` policies are a Linux/Android concept. Inside an
/// `on_transact` body on Linux/Android, the returned value is the policy
/// the kernel installed for the binder worker thread before invoking
/// the handler.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn get_current_scheduler_policy() -> Result<i32> {
    // SAFETY: no pointer arguments; pid 0 (the calling thread) is always valid.
    let raw = unsafe { libc::sched_getscheduler(0) };
    if raw < 0 {
        // POSIX sets errno on -1; the fallback is EINVAL because `0` would read as success.
        let errno = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL);
        return Err(StatusCode::from(rustix::io::Errno::from_raw_os_error(
            errno,
        )));
    }
    Ok(raw)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn get_current_scheduler_policy() -> Result<i32> {
    Err(StatusCode::InvalidOperation)
}

/// Last transaction failure detail reported by the kernel binder driver
/// for the current thread, as returned by `BINDER_GET_EXTENDED_ERROR`
/// (Android 12 / Linux 5.14+).
///
/// Stable Rust mirror of the kernel's `struct binder_extended_error`
/// (`include/uapi/linux/android/binder.h:302-306` in AOSP /
/// `external/kernel-headers/.../binder.h`). See [`get_extended_error`]
/// for retrieval semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtendedError {
    /// Monotonically increasing per-thread counter. Each fresh failure
    /// observed by the driver bumps this; `0` means no failure has yet
    /// been recorded on this thread.
    pub id: u32,
    /// The BR_ return code that surfaced the failure
    /// (typically `BR_FAILED_REPLY`).
    pub command: u32,
    /// Negated errno or other driver-specific code (`0` when the driver
    /// has no additional detail).
    pub param: i32,
}

/// Retrieve the kernel-side detail of the most recent transaction
/// failure on the current thread.
///
/// AOSP equivalent: `IPCThreadState::getExtendedError()`
/// ([IPCThreadState.cpp](https://cs.android.com/android/platform/superproject/main/+/main:frameworks/native/libs/binder/IPCThreadState.cpp)).
/// The ioctl was added in Android 12 (S, SDK 31, Linux 5.14+); older
/// drivers respond `ENOTTY`, surfaced here as
/// `Err(StatusCode::InvalidOperation)` so callers can fall back to the
/// bare `StatusCode::FailedTransaction` signal from a failed `transact`.
///
/// Typical use pairs with a preceding `transact` that returned
/// `Err(StatusCode::FailedTransaction)`:
///
/// ```ignore
/// match svc.do_something(...) {
///     Err(StatusCode::FailedTransaction) => {
///         if let Ok(ee) = rsbinder::get_extended_error() {
///             eprintln!("driver detail: cmd={:#x} param={}", ee.command, ee.param);
///         }
///     }
///     _ => {}
/// }
/// ```
///
/// **Opt-in**: rsbinder does *not* call this automatically from the
/// `BR_FAILED_REPLY` arm. A no-op for callers that never invoke it.
pub fn get_extended_error() -> Result<ExtendedError> {
    // Pure-RPC process: an error, as the other accessors do, instead of an `as_self` panic.
    if !ProcessState::is_initialized() {
        return Err(StatusCode::InvalidOperation);
    }
    let mut ee = binder::binder_extended_error::default();
    let driver = ProcessState::as_self().driver();
    crate::sys::binder::get_extended_error(driver.as_ref(), &mut ee).map_err(|errno| {
        if errno == rustix::io::Errno::NOTTY {
            // Pre-Android-12 driver — feature unavailable.
            StatusCode::InvalidOperation
        } else {
            StatusCode::from(errno)
        }
    })?;
    Ok(ExtendedError {
        id: ee.id,
        command: ee.command,
        param: ee.param,
    })
}

/// Whether [`clear_calling_identity`] has been invoked on the current
/// in-flight transaction without a matching [`restore_calling_identity`].
///
/// Mirrors AOSP `IPCThreadState::hasExplicitIdentity()`
/// (`IPCThreadState.cpp:571`). Reset to `false` on every new incoming
/// `BR_TRANSACTION`, matching AOSP's per-transaction lifecycle
/// (`IPCThreadState.cpp:1141`).
///
/// Returns `false` when not currently dispatching a transaction, and also
/// while an RPC transaction is being dispatched — [`clear_calling_identity`]
/// is a no-op there, and the kernel transaction that may sit underneath a
/// nested RPC dispatch belongs to an outer frame.
pub fn has_explicit_identity() -> bool {
    if rpc_calling().is_some() || !ProcessState::is_initialized() {
        return false;
    }
    THREAD_STATE.with(|thread_state| {
        thread_state
            .borrow()
            .transaction
            .as_ref()
            .is_some_and(|tr| tr.has_explicit_identity)
    })
}

#[cfg(test)]
mod tests {
    //! Tests that touch `THREAD_STATE` are Linux + binderfs only: `THREAD_STATE.with(..)` runs
    //! `ThreadState::new`, which captures the global `ProcessState` driver, so they call
    //! `ProcessState::init_default` (opens `/dev/binderfs/binder`) and run under
    //! `serial(binder)`. They run in `.github/workflows/integration-test.yml`, except the
    //! `rpc`-gated ones (`nested_kernel_transaction_answers_for_the_kernel_caller`): that job
    //! builds with default features, so they run only by hand (`--features rpc`, binderfs).
    //!
    //! # Mutation gates
    //!
    //! - `rpc_calling_context_is_read_restored_and_failclosed` (Plan 2-16 Phase B/C): runs
    //!   without a kernel `ProcessState` (hermetic, also on macOS). The own-uid / `!handling` /
    //!   `None` answers outside the guard also prove the pure-RPC accessors do not panic while
    //!   `ProcessState` is uninitialized.
    //! - `test_drive_dead_binder_handshake_orchestration`: pins that phases run obituary →
    //!   queue → pin on success; an obituary error short-circuits neither queue nor pin; a
    //!   queue error skips pin; with obituary and pin both failing the obituary error wins; the
    //!   kernel handshake (queue + pin) runs whenever queue succeeds. Losing any of these (e.g. a
    //!   `?` early return on the obituary error) leaks the kernel `binder_ref` slot.
    //! - `test_process_pending_derefs_handles_reentrant_push_from_drop`: a `BINDER_DEREFS` borrow held
    //!   across `deref_native_kernel` panics on the second `borrow_mut()` when entry removal runs
    //!   an `Inner<T>::drop` whose destructor makes another `BR_RELEASE` / `BR_DECREFS` land
    //!   (outgoing IPC → `wait_for_response` → `talk_with_driver` → `execute_command`). The test
    //!   stands in for that by pushing a second id into `pending_weak_derefs` from a
    //!   drop-fired sentinel, with no kernel involved.
    //! - `nested_kernel_transaction_answers_for_the_kernel_caller`: the `BR_TRANSACTION` is fed
    //!   to `execute_command` itself, so deleting `RpcCallingGuard::suspend()` from the kernel
    //!   dispatch path fails the test. The forged transaction is `TF_ONE_WAY` (no `BC_REPLY`, no
    //!   driver round trip) with an empty buffer; `free_buffer` would queue a `BC_FREE_BUFFER`
    //!   for a pointer the driver does not own, so the thread is marked a looper for the call
    //!   (suppressing the flush) and those bytes are discarded afterwards.

    use super::*;

    fn own_uid() -> binder::uid_t {
        rustix::process::getuid().as_raw()
    }

    fn own_pid() -> binder::pid_t {
        rustix::process::getpid().as_raw_nonzero().get() as binder::pid_t
    }

    /// Accessors read the RPC context, restore and nest with the guard, and fail closed.
    #[cfg(feature = "rpc")]
    #[test]
    fn rpc_calling_context_is_read_restored_and_failclosed() {
        use crate::rpc::transport::PeerIdentity;
        use std::sync::Arc;

        // Outside any transaction (pure-RPC, no ProcessState): this process, as in AOSP.
        assert_eq!(get_calling_uid(), own_uid());
        assert_eq!(get_calling_pid(), own_pid());
        assert!(!is_handling_transaction());
        assert!(get_calling_sid().is_none());
        assert!(calling_caller().is_none());
        assert!(calling_caps().is_none());

        let unix_caps = crate::TransportCaps::FD_PASSING
            | crate::TransportCaps::TRUSTED_UID
            | crate::TransportCaps::SAME_HOST;
        {
            let _g = RpcCallingGuard::install(
                Arc::new(PeerIdentity::Local { uid: 1234, pid: 42 }),
                unix_caps,
            );
            assert_eq!(get_calling_uid(), 1234);
            assert_eq!(get_calling_pid(), 42);
            assert!(is_handling_transaction());
            // RPC carries no SELinux context.
            assert!(get_calling_sid().is_none());
            assert_eq!(CallingContext::default().uid, 1234);
            assert_eq!(CallingContext::default().pid, 42);
            // `calling_caller` exposes the full RPC peer (Phase C).
            match calling_caller() {
                Some(Caller::Rpc(PeerIdentity::Local { uid, pid })) => {
                    assert_eq!((uid, pid), (1234, 42));
                }
                other => panic!("expected Caller::Rpc(Local), got {other:?}"),
            }
            // The caps of the dispatching session.
            assert_eq!(calling_caps(), Some(unix_caps));
            // No callback connection here, so a feature needing one is refused locally.
            assert_eq!(
                calling_caps()
                    .unwrap()
                    .require(crate::TransportCaps::CALLBACKS, "a callback"),
                Err(crate::StatusCode::InvalidOperation)
            );

            // Nested vsock callback: the fail-closed sentinel, then the outer identity again.
            {
                let _g2 = RpcCallingGuard::install(
                    Arc::new(PeerIdentity::Vsock { cid: 7 }),
                    crate::TransportCaps::NONE,
                );
                assert_eq!(get_calling_uid(), RPC_UNKNOWN_CALLING_UID);
                assert_ne!(get_calling_uid(), 0, "sentinel must never read as root");
                assert_eq!(get_calling_pid(), -1);
                // The full peer is still exposed for cid-based decisions.
                assert!(matches!(
                    calling_caller(),
                    Some(Caller::Rpc(PeerIdentity::Vsock { cid: 7 }))
                ));
                assert_eq!(calling_caps(), Some(crate::TransportCaps::NONE));
            }
            assert_eq!(
                get_calling_uid(),
                1234,
                "outer identity restored after nesting"
            );
            assert_eq!(
                calling_caps(),
                Some(unix_caps),
                "outer caps restored after nesting"
            );
        }

        // Fully restored.
        assert_eq!(get_calling_uid(), own_uid());
        assert!(!is_handling_transaction());
        assert!(calling_caller().is_none());
        assert!(calling_caps().is_none());
    }

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
        // Freeze-observer BR strings.
        assert_eq!(
            return_to_str(binder::BR_TRANSACTION_PENDING_FROZEN),
            "BR_TRANSACTION_PENDING_FROZEN"
        );
        assert_eq!(return_to_str(binder::BR_FROZEN_BINDER), "BR_FROZEN_BINDER");
        assert_eq!(
            return_to_str(binder::BR_CLEAR_FREEZE_NOTIFICATION_DONE),
            "BR_CLEAR_FREEZE_NOTIFICATION_DONE"
        );
    }

    /// `_IO('r', 20)`, `_IOR('r', 21, binder_frozen_state_info)`, `_IOR('r', 22, uintptr)`.
    #[test]
    fn freeze_observer_br_constants_match_uapi() {
        // _IO('r', 20)
        assert_eq!(binder::BR_TRANSACTION_PENDING_FROZEN, 29204);
        // _IOR('r', 21, binder_frozen_state_info) — sizeof=16
        assert_eq!(binder::BR_FROZEN_BINDER, 2148561429);
        // _IOR('r', 22, binder_uintptr_t) — sizeof=8
        assert_eq!(binder::BR_CLEAR_FREEZE_NOTIFICATION_DONE, 2148037142);
    }

    /// `_IOW('c', 19 and 20, binder_handle_cookie)`, `_IOW('c', 21, binder_uintptr_t)`.
    #[test]
    fn freeze_observer_bc_constants_match_uapi() {
        // _IOW('c', 19, binder_handle_cookie) — sizeof packed = 12
        assert_eq!(binder::BC_REQUEST_FREEZE_NOTIFICATION, 1074553619);
        // _IOW('c', 20, binder_handle_cookie) — sizeof = 12
        assert_eq!(binder::BC_CLEAR_FREEZE_NOTIFICATION, 1074553620);
        // _IOW('c', 21, binder_uintptr_t) — sizeof = 8
        assert_eq!(binder::BC_FREEZE_NOTIFICATION_DONE, 1074291477);
    }

    /// u64 cookie + u32 is_frozen + u32 reserved = 16 bytes, align 8; no arm reads it yet.
    #[test]
    fn binder_frozen_state_info_layout() {
        use crate::sys::binder_frozen_state_info;
        assert_eq!(std::mem::size_of::<binder_frozen_state_info>(), 16);
        assert_eq!(std::mem::align_of::<binder_frozen_state_info>(), 8);
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
        // Freeze-observer BC strings.
        assert_eq!(
            command_to_str(binder::BC_REQUEST_FREEZE_NOTIFICATION),
            "BC_REQUEST_FREEZE_NOTIFICATION"
        );
        assert_eq!(
            command_to_str(binder::BC_CLEAR_FREEZE_NOTIFICATION),
            "BC_CLEAR_FREEZE_NOTIFICATION"
        );
        assert_eq!(
            command_to_str(binder::BC_FREEZE_NOTIFICATION_DONE),
            "BC_FREEZE_NOTIFICATION_DONE"
        );
    }

    /// A panic becomes `Err(Unknown)` with the partial reply reset (module doc "Handler panics").
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
                // Partial write then panic: the test checks the partial reply is discarded.
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

    /// A non-panicking `transact`'s `Result` passes through the panic guard unchanged.
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

    /// Phase order, no short-circuit and error priority; see `# Mutation gates`.
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

        // Case B: obituary errors → queue and pin still run, so the binder_ref slot never leaks.
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

        // Case C: queue write fails → pin skipped (documented edge); queue error surfaces.
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

        // Case E: obituary and pin both fail → obituary error surfaces; pin error only logged.
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

    /// A re-entrant push from a user `Inner<T>::drop` mid-drain; see `# Mutation gates`.
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_process_pending_derefs_handles_reentrant_push_from_drop() {
        use std::sync::atomic::AtomicU64;
        use std::sync::{self, Mutex};
        let process = ProcessState::init_default().expect("init_default");

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

        // Sentinel A: its drop pushes B's id into BINDER_DEREFS, like a mid-drain BR_DECREFS.
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
        // kernel_refs 1 so the deref removes the entry; binder_pin stays the only holder.
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

        // Push only id_a as a strong deref: A's drop pushes id_b during the drain.
        BINDER_DEREFS.with(|d| {
            d.borrow_mut().pending_strong_derefs.push_back(id_a);
        });

        // A's drop re-borrows BINDER_DEREFS mid-drain; a borrow held across it would panic.
        process_pending_derefs().expect("process_pending_derefs must not panic or error");

        // Both natives dropped: the outer loop picked up the re-entrant push.
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

    // ---- calling-identity + strict-mode API: synthetic `TransactionState`, no driver ----

    /// Installs a fake in-flight transaction; drop restores the previous one (usually `None`).
    #[cfg(target_os = "linux")]
    struct FakeTransactionGuard {
        previous: Option<TransactionState>,
    }

    #[cfg(target_os = "linux")]
    impl FakeTransactionGuard {
        fn install(
            calling_uid: binder::uid_t,
            calling_pid: binder::pid_t,
            calling_sid: *const u8,
        ) -> Self {
            let new_state = TransactionState {
                calling_pid,
                calling_sid,
                calling_uid,
                last_transaction_binder_flags: 0,
                has_explicit_identity: false,
            };
            let previous = THREAD_STATE.with(|ts| ts.borrow_mut().transaction.replace(new_state));
            FakeTransactionGuard { previous }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for FakeTransactionGuard {
        fn drop(&mut self) {
            THREAD_STATE.with(|ts| {
                ts.borrow_mut().transaction = self.previous.take();
            });
        }
    }

    /// Outside a transaction: own uid/pid, no SID, clear token 0, restore a no-op.
    #[test]
    #[cfg(target_os = "linux")]
    #[serial_test::serial(binder)]
    fn test_get_calling_outside_transaction_returns_defaults() {
        ProcessState::init_default().expect("init_default");
        // Clear a prior test's `THREAD_STATE.transaction`: `serial(binder)` keeps thread-locals.
        let _ = THREAD_STATE.with(|ts| ts.borrow_mut().transaction.take());
        assert!(!is_handling_transaction());
        // AOSP `IPCThreadState::getCallingUid/Pid`: this process, never root.
        assert_eq!(get_calling_uid(), own_uid());
        assert_eq!(get_calling_pid(), own_pid());
        assert!(get_calling_sid().is_none());
        assert!(!has_explicit_identity());
        // clear/restore are no-ops outside a transaction.
        assert_eq!(clear_calling_identity(), 0);
        restore_calling_identity(0xDEAD_BEEF_DEAD_BEEFu64 as i64); // must not panic
        assert_eq!(get_calling_uid(), own_uid());
    }

    /// Getters return the kernel-delivered values; the SID is a lazy `CString` copy of secctx.
    #[test]
    #[cfg(target_os = "linux")]
    #[serial_test::serial(binder)]
    fn test_get_calling_inside_transaction_extracts_fields() {
        ProcessState::init_default().expect("init_default");
        let sid_cstring = std::ffi::CString::new("u:r:system_server:s0").unwrap();
        let _guard = FakeTransactionGuard::install(1000, 9999, sid_cstring.as_ptr() as *const u8);

        assert!(is_handling_transaction());
        assert_eq!(get_calling_uid(), 1000);
        assert_eq!(get_calling_pid(), 9999);

        let sid = get_calling_sid().expect("SID present when secctx pointer non-null");
        assert_eq!(sid.to_str().unwrap(), "u:r:system_server:s0");

        // Each call copies into a fresh CString; the kernel mmap keeps owning the pointer.
        let sid2 = get_calling_sid().expect("second call also returns Some");
        assert_eq!(sid, sid2);
        assert!(!std::ptr::eq(sid.as_ptr(), sid2.as_ptr()));
    }

    /// Records the calling-identity answers inside the dispatch, read after `execute_command`.
    #[cfg(all(target_os = "linux", feature = "rpc"))]
    #[derive(Default)]
    struct RecordingNative {
        observed: std::sync::Mutex<Option<(Option<crate::TransportCaps>, Option<Caller>)>>,
    }

    #[cfg(all(target_os = "linux", feature = "rpc"))]
    impl Transactable for RecordingNative {
        fn transact(&self, _: TransactionCode, _: &mut Parcel, _: &mut Parcel) -> Result<()> {
            *self.observed.lock().unwrap() = Some((calling_caps(), calling_caller()));
            Ok(())
        }
    }

    #[cfg(all(target_os = "linux", feature = "rpc"))]
    impl IBinder for RecordingNative {
        fn link_to_death(&self, _: std::sync::Weak<dyn DeathRecipient>) -> Result<()> {
            Err(StatusCode::InvalidOperation)
        }
        fn unlink_to_death(&self, _: std::sync::Weak<dyn DeathRecipient>) -> Result<()> {
            Err(StatusCode::InvalidOperation)
        }
        fn ping_binder(&self) -> Result<()> {
            Ok(())
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_transactable(&self) -> Option<&dyn Transactable> {
            Some(self)
        }
        fn descriptor(&self) -> &str {
            "rsbinder.test.RecordingNative"
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

    /// A kernel call nested in an RPC handler sees the kernel caller; see `# Mutation gates`.
    #[test]
    #[cfg(all(target_os = "linux", feature = "rpc"))]
    #[serial_test::serial(binder)]
    fn nested_kernel_transaction_answers_for_the_kernel_caller() {
        use crate::rpc::transport::PeerIdentity;
        use std::sync::Arc;

        ProcessState::init_default().expect("init_default");
        let _ = THREAD_STATE.with(|ts| ts.borrow_mut().transaction.take());

        let recorder = Arc::new(RecordingNative::default());
        let id = ProcessState::as_self().publish_native(recorder.clone());

        let unix_caps = crate::TransportCaps::FD_PASSING
            | crate::TransportCaps::TRUSTED_UID
            | crate::TransportCaps::SAME_HOST;
        // An RPC handler's outgoing kernel call delivers the kernel transaction below into it.
        let _rpc = RpcCallingGuard::install(
            Arc::new(PeerIdentity::Local { uid: 1234, pid: 42 }),
            unix_caps,
        );
        assert_eq!(calling_caps(), Some(unix_caps));

        let mut buffer = [0u8; 8];
        let mut offsets = [0 as binder_size_t; 1];
        let tr = binder_transaction_data {
            target: binder_transaction_data__bindgen_ty_1 { ptr: id },
            cookie: 0,
            code: 1,
            flags: binder::transaction_flags_TF_ONE_WAY,
            sender_pid: 9999,
            sender_euid: 1000,
            data_size: 0,
            offsets_size: 0,
            data: binder_transaction_data__bindgen_ty_2 {
                ptr: binder_transaction_data__bindgen_ty_2__bindgen_ty_1 {
                    buffer: buffer.as_mut_ptr() as _,
                    offsets: offsets.as_mut_ptr() as _,
                },
            },
        };

        let (was_looper, mark) = THREAD_STATE.with(|ts| {
            let mut ts = ts.borrow_mut();
            let was_looper = ts.is_looper;
            ts.is_looper = true;
            let mark = ts.unflushed_mark();
            ts.in_parcel.set_data_size(0).expect("reset in_parcel");
            ts.in_parcel.set_data_position(0);
            ts.in_parcel
                .write_transaction(&tr)
                .expect("forge BR_TRANSACTION payload");
            ts.in_parcel.set_data_position(0);
            (was_looper, mark)
        });

        let dispatched = execute_command(binder::BR_TRANSACTION as i32);

        THREAD_STATE.with(|ts| {
            discard_unflushed_commands(ts, mark, false);
            let mut ts = ts.borrow_mut();
            ts.is_looper = was_looper;
            let _ = ts.in_parcel.set_data_size(0);
        });
        dispatched.expect("execute_command");

        let (caps, caller) = recorder
            .observed
            .lock()
            .unwrap()
            .take()
            .expect("the handler ran");
        assert_eq!(caps, Some(crate::TransportCaps::KERNEL));
        assert!(matches!(
            caller,
            Some(Caller::Kernel {
                uid: 1000,
                pid: 9999,
                ..
            })
        ));

        // The RPC handler resumes where it left off.
        assert_eq!(calling_caps(), Some(unix_caps));
        assert_eq!(get_calling_uid(), 1234);
    }

    /// Plain `BR_TRANSACTION` (not `_SEC_CTX`): no SID even while handling a transaction.
    #[test]
    #[cfg(target_os = "linux")]
    #[serial_test::serial(binder)]
    fn test_get_calling_sid_null_secctx_returns_none() {
        ProcessState::init_default().expect("init_default");
        let _guard = FakeTransactionGuard::install(1000, 9999, std::ptr::null());
        assert!(is_handling_transaction());
        assert_eq!(get_calling_uid(), 1000);
        assert!(get_calling_sid().is_none());
    }

    /// `(has_explicit, pid sign)` quadrants of AOSP `static_assert`s (IPCThreadState.cpp:530-560).
    #[test]
    fn test_calling_identity_token_pack_unpack_round_trip() {
        for &(has_explicit, uid, pid) in &[
            (true, 1000u32, 9999i32),
            (false, 1000u32, 9999i32),
            (true, 1000u32, -1i32),
            (false, 1000u32, -1i32),
            (true, 0u32, 0i32),
            (false, 0u32, 0i32),
        ] {
            let tok = pack_calling_identity(has_explicit, uid, pid);
            assert_eq!(
                unpack_has_explicit_identity(tok),
                has_explicit,
                "has_explicit ({has_explicit},{uid},{pid})"
            );
            assert_eq!(
                unpack_calling_uid(tok),
                uid,
                "uid ({has_explicit},{uid},{pid})"
            );
            assert_eq!(
                unpack_calling_pid(tok),
                pid,
                "pid ({has_explicit},{uid},{pid})"
            );
        }
    }

    /// Clear stamps own uid/pid and `has_explicit`; restore brings the originals back.
    #[test]
    #[cfg(target_os = "linux")]
    #[serial_test::serial(binder)]
    fn test_clear_and_restore_calling_identity_round_trip() {
        ProcessState::init_default().expect("init_default");
        let _guard = FakeTransactionGuard::install(1000, 9999, std::ptr::null());
        assert_eq!(get_calling_uid(), 1000);
        assert_eq!(get_calling_pid(), 9999);
        assert!(!has_explicit_identity());

        let token = clear_calling_identity();

        // After clear: calling identity is the current process.
        assert_eq!(get_calling_uid(), rustix::process::getuid().as_raw());
        assert_eq!(
            get_calling_pid(),
            rustix::process::getpid().as_raw_nonzero().get() as binder::pid_t
        );
        assert!(has_explicit_identity());
        assert!(get_calling_sid().is_none());

        restore_calling_identity(token);

        assert_eq!(get_calling_uid(), 1000);
        assert_eq!(get_calling_pid(), 9999);
        assert!(!has_explicit_identity());
    }

    /// Resets the policy at the end: later tests on this thread share the thread-local.
    #[test]
    #[cfg(target_os = "linux")]
    #[serial_test::serial(binder)]
    fn test_strict_mode_policy_round_trip() {
        ProcessState::init_default().expect("init_default");
        let saved = get_strict_mode_policy();
        set_strict_mode_policy(0x1234_5678);
        assert_eq!(get_strict_mode_policy(), 0x1234_5678);
        set_strict_mode_policy(saved);
        assert_eq!(get_strict_mode_policy(), saved);
    }

    const UNSET_UID: binder::uid_t = UNSET_WORK_SOURCE as binder::uid_t;

    /// Plan 10-9 AC-9.2; each `#[test]` runs on its own thread, so the thread-local starts unset.
    #[test]
    fn work_source_set_clear_restore_outside_a_transaction() {
        assert_eq!(get_calling_work_source_uid(), UNSET_UID);
        assert!(!should_propagate_work_source());

        let before_set = set_calling_work_source_uid(1234);
        assert_eq!(get_calling_work_source_uid(), 1234);
        assert!(should_propagate_work_source());

        let before_clear = clear_calling_work_source();
        assert_eq!(get_calling_work_source_uid(), UNSET_UID);
        assert!(
            should_propagate_work_source(),
            "clear sends the unset value on purpose, so it still propagates"
        );

        clear_propagate_work_source();
        assert!(!should_propagate_work_source());
        assert_eq!(get_calling_work_source_uid(), UNSET_UID);

        restore_calling_work_source(before_clear);
        assert_eq!(get_calling_work_source_uid(), 1234);
        assert!(should_propagate_work_source());

        restore_calling_work_source(before_set);
        assert_eq!(get_calling_work_source_uid(), UNSET_UID);
        assert!(
            !should_propagate_work_source(),
            "restoring an unset, non-propagating token must not turn propagation on"
        );
    }

    /// A received value reaches the handler but not the next hop unless the handler sets it again.
    #[test]
    fn work_source_received_value_does_not_propagate() {
        let token = set_calling_work_source_uid_without_propagation(77);
        assert_eq!(get_calling_work_source_uid(), 77);
        assert!(!should_propagate_work_source());
        restore_calling_work_source(token);
        assert_eq!(get_calling_work_source_uid(), UNSET_UID);
    }

    /// Nesting is how a server thread that is itself a client re-enters dispatch.
    #[test]
    fn work_source_dispatch_guard_resets_and_restores_when_nested() {
        set_calling_work_source_uid(10);
        {
            let _outer = WorkSourceDispatchGuard::enter();
            assert_eq!(get_calling_work_source_uid(), UNSET_UID);
            assert!(!should_propagate_work_source());
            set_calling_work_source_uid_without_propagation(20);
            set_calling_work_source_uid(30);
            {
                let _inner = WorkSourceDispatchGuard::enter();
                assert_eq!(get_calling_work_source_uid(), UNSET_UID);
                set_calling_work_source_uid(40);
            }
            assert_eq!(get_calling_work_source_uid(), 30);
            assert!(should_propagate_work_source());
        }
        assert_eq!(get_calling_work_source_uid(), 10);
        assert!(should_propagate_work_source());
    }

    #[test]
    fn work_source_dispatch_guard_restores_on_unwind() {
        set_calling_work_source_uid(10);
        let unwound = std::panic::catch_unwind(|| {
            let _guard = WorkSourceDispatchGuard::enter();
            set_calling_work_source_uid(99);
            panic!("handler panicked");
        });
        assert!(unwound.is_err());
        assert_eq!(get_calling_work_source_uid(), 10);
    }

    /// As AOSP `Parcel::writeInterfaceToken` (`Parcel.cpp:1136-1140`).
    #[test]
    fn work_source_is_written_to_the_request_header_only_when_propagating() {
        fn header_work_source() -> i32 {
            let mut p = Parcel::new();
            p.write_interface_token("x.y.IZ").unwrap();
            p.set_data_position(4);
            p.read::<i32>().unwrap()
        }
        assert_eq!(header_work_source(), UNSET_WORK_SOURCE);
        set_calling_work_source_uid(4321);
        assert_eq!(header_work_source(), 4321);
        clear_propagate_work_source();
        assert_eq!(header_work_source(), UNSET_WORK_SOURCE);
    }

    /// The driver reports an offset, not a command; the offset names the refused command.
    #[test]
    fn a_refused_command_is_named_from_the_offset_the_driver_stopped_at() {
        let mut out = CommandStream::new();
        out.write_cmd::<u32>(&binder::BC_INCREFS).unwrap();
        out.write_cmd::<u32>(&7u32).unwrap();
        let second = out.data_size();
        out.write_cmd::<u32>(&binder::BC_FREE_BUFFER).unwrap();
        out.write_cmd::<u32>(&0u32).unwrap();
        let total = out.data_size();

        out.set_data_position(4);
        let msg = describe_refused_command(&mut out, second);
        assert!(msg.contains("BC_FREE_BUFFER"), "{msg}");
        assert!(
            msg.contains(&format!("consumed {second} of {total}")),
            "{msg}"
        );
        assert_eq!(
            out.data_position(),
            4,
            "the probe must leave the read position where it found it"
        );

        assert_eq!(
            describe_refused_command(&mut out, total),
            format!("consumed {total} of {total}"),
            "a fully consumed buffer has no command to blame"
        );
    }
}
