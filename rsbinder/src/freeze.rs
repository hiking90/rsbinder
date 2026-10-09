// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Per-handle bookkeeping for freeze notifications (AOSP
//! `IBinder::addFrozenStateChangeCallback`).
//!
//! # Kernel rules this module encodes
//!
//! Both the C driver (`binder_request_freeze_notification`,
//! `binder_clear_freeze_notification`, `binder_freeze_notification_done`) and
//! the Rust driver (`drivers/android/binder/freeze.rs`) answer `-EINVAL` to:
//!
//! - a second `BC_REQUEST_FREEZE_NOTIFICATION` on a ref that has one;
//! - a `BC_CLEAR_FREEZE_NOTIFICATION` on a ref that has none;
//! - a `BC_FREEZE_NOTIFICATION_DONE` for a cookie with no undelivered
//!   acknowledgement.
//!
//! A refused command in the middle of a write buffer aborts the process, and
//! one at its start is resent by every later ioctl (`thread_state` module doc
//! "Refused commands"), so these are prevented, not handled:
//!
//! - **Request** is issued only for a handle with no slot, or when a clear
//!   that was asked to re-register completes. One slot per handle serializes
//!   it: a `ProxyHandle` lives per strong lifetime, and a dropping proxy and
//!   its replacement can coexist (`process_state` module doc "Proxy cache slow
//!   path", case (b)), so per-proxy state could order a new request before an
//!   old clear.
//! - **Clear** is issued only from `Registered`.
//! - **Done** is queued exactly once per `BR_FROZEN_BINDER`, whatever the
//!   slot says (`thread_state::drive_frozen_binder_handshake`).
//!
//! A registration keeps its `binder_ref` alive until
//! `BR_CLEAR_FREEZE_NOTIFICATION_DONE`: the kernel drops a registration with
//! its ref without a clear-done, after which the pending done is refused.
//! `process_state`'s pin ledger holds `BC_DECREFS` for in-flight clears.
//!
//! The cookie is the handle, as for death notifications: the ledger keeps the
//! ref, so the handle cannot name another node while a registration or clear
//! is outstanding, which the Rust driver's cookie-uniqueness check requires.
//!
//! # Re-registration
//!
//! A callback added while a clear is in flight sets `rerequest`; the request
//! goes out when the clear completes, as AOSP's
//! `RELEASE_LIBBINDER_DEFER_BC_REQUEST_FREEZE_NOTIFICATION` (`BpBinder.cpp`
//! `onFrozenStateChangeListenerRemoved`).
//!
//! # Delivery
//!
//! Callbacks run outside every lock, so two threads could otherwise call one
//! callback concurrently with two states (an add's initial state and a
//! `BR_FROZEN_BINDER`), and it could end on the older one. `delivering` lets
//! one thread at a time deliver a handle's batch; a thread that finds it set
//! leaves, and the delivering thread re-checks after its batch. Each entry
//! records the last state it was given, so it sees every state at most once
//! and ends on the latest. A nested `BR_FROZEN_BINDER` on the delivering
//! thread (a callback making a oneway call) takes the same exit.
//!
//! The delivering thread may be one inside `add` (handing over a cached
//! state); it then also delivers states that arrive meanwhile, to every
//! entry, because the thread that brought them has left. Restricting it to
//! the new entry would leave those states with no thread to deliver them.
//! A batch's token is its slot's `id`, so a batch that outlives its slot
//! (cleared and re-created meanwhile) cannot release the new slot's claim.
//!
//! The C driver may report one state twice (`binder_add_freeze_work` sets
//! `resend` on a round trip); `last` filters it, as AOSP's `onFrozenStateChanged`.
//!
//! Entries are tagged with their proxy's address: only the live proxy's
//! entries are delivered, and a dropping proxy removes only its own.

use std::collections::hash_map::Entry as MapEntry;
use std::collections::HashMap;
use std::sync::{self, Arc};

use crate::binder::{FrozenState, FrozenStateChangeCallback};
use crate::error::StatusCode;

/// A command the caller writes while still holding the registry lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FreezeCmd {
    Request,
    Clear,
}

/// What a `BR_CLEAR_FREEZE_NOTIFICATION_DONE` did to the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClearDone {
    /// The slot is gone; nothing is registered for the handle.
    Removed,
    /// Callbacks were added during the clear; the caller writes a request.
    Rerequest,
    /// No clear was in flight for the handle.
    Unexpected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kernel {
    /// The request was written; `last` is the latest state the kernel reported.
    Registered { last: Option<bool> },
    /// The clear was written; `rerequest` if callbacks were added since.
    PendingClear { rerequest: bool },
}

struct Entry {
    owner: usize,
    callback: sync::Weak<dyn FrozenStateChangeCallback>,
    delivered: Option<bool>,
}

struct Slot {
    /// Unique per slot ever created; the delivery claim's token (module doc "Delivery").
    id: u64,
    kernel: Kernel,
    entries: Vec<Entry>,
    delivering: bool,
}

impl Slot {
    /// After entries were removed: the clear to write, if none remain.
    fn after_removal(&mut self) -> Option<FreezeCmd> {
        if !self.entries.is_empty() {
            return None;
        }
        match &mut self.kernel {
            Kernel::Registered { .. } => {
                self.kernel = Kernel::PendingClear { rerequest: false };
                Some(FreezeCmd::Clear)
            }
            Kernel::PendingClear { rerequest } => {
                *rerequest = false;
                None
            }
        }
    }
}

/// The claim token for [`FreezeRegistry::end_batch`], one freeze state, and the callbacks
/// to give it to.
pub(crate) type Batch = (u64, FrozenState, Vec<Arc<dyn FrozenStateChangeCallback>>);

/// Slots keyed by handle (= kernel cookie). See the module doc.
#[derive(Default)]
pub(crate) struct FreezeRegistry {
    slots: HashMap<u32, Slot>,
    next_id: u64,
}

impl FreezeRegistry {
    /// Adds `callback` for `owner`; `Some(Request)` when the handle had no slot.
    pub(crate) fn add(
        &mut self,
        handle: u32,
        owner: usize,
        callback: sync::Weak<dyn FrozenStateChangeCallback>,
    ) -> Option<FreezeCmd> {
        let entry = Entry {
            owner,
            callback,
            delivered: None,
        };
        match self.slots.entry(handle) {
            MapEntry::Vacant(vacant) => {
                self.next_id += 1;
                vacant.insert(Slot {
                    id: self.next_id,
                    kernel: Kernel::Registered { last: None },
                    entries: vec![entry],
                    delivering: false,
                });
                Some(FreezeCmd::Request)
            }
            MapEntry::Occupied(occupied) => {
                let slot = occupied.into_mut();
                if let Kernel::PendingClear { rerequest } = &mut slot.kernel {
                    *rerequest = true;
                }
                slot.entries.push(entry);
                None
            }
        }
    }

    /// Forgets `handle` after its request could not be written.
    pub(crate) fn abort_request(&mut self, handle: u32) {
        self.slots.remove(&handle);
    }

    /// Removes `owner`'s first registration of `callback`; `Some(Clear)` if none remain.
    pub(crate) fn remove(
        &mut self,
        handle: u32,
        owner: usize,
        callback: &sync::Weak<dyn FrozenStateChangeCallback>,
    ) -> Result<Option<FreezeCmd>, StatusCode> {
        let slot = self
            .slots
            .get_mut(&handle)
            .ok_or(StatusCode::NameNotFound)?;
        let index = slot
            .entries
            .iter()
            .position(|e| e.owner == owner && sync::Weak::ptr_eq(&e.callback, callback))
            .ok_or(StatusCode::NameNotFound)?;
        slot.entries.remove(index);
        Ok(slot.after_removal())
    }

    /// Removes every entry of a dropping proxy; `Some(Clear)` if none remain.
    pub(crate) fn drop_owner(&mut self, handle: u32, owner: usize) -> Option<FreezeCmd> {
        let slot = self.slots.get_mut(&handle)?;
        slot.entries.retain(|e| e.owner != owner);
        slot.after_removal()
    }

    /// Records a `BR_FROZEN_BINDER`; `true` if the state is new and should be delivered.
    pub(crate) fn state_changed(&mut self, handle: u32, is_frozen: bool) -> bool {
        match self.slots.get_mut(&handle).map(|slot| &mut slot.kernel) {
            Some(Kernel::Registered { last }) if *last != Some(is_frozen) => {
                *last = Some(is_frozen);
                true
            }
            // Stale or repeated, or a clear is in flight (AOSP `isPendingClear`).
            _ => false,
        }
    }

    /// Records a `BR_CLEAR_FREEZE_NOTIFICATION_DONE`.
    pub(crate) fn clear_done(&mut self, handle: u32) -> ClearDone {
        let Some(slot) = self.slots.get_mut(&handle) else {
            return ClearDone::Unexpected;
        };
        match slot.kernel {
            Kernel::Registered { .. } => ClearDone::Unexpected,
            Kernel::PendingClear { rerequest } if rerequest && !slot.entries.is_empty() => {
                // The kernel resends the initial state, and AOSP re-delivers it to all.
                slot.kernel = Kernel::Registered { last: None };
                slot.entries.iter_mut().for_each(|e| e.delivered = None);
                ClearDone::Rerequest
            }
            Kernel::PendingClear { .. } => {
                self.slots.remove(&handle);
                ClearDone::Removed
            }
        }
    }

    /// Claims delivery of `owner`'s undelivered entries; `None` if another thread delivers.
    pub(crate) fn begin_batch(&mut self, handle: u32, owner: usize) -> Option<Batch> {
        let slot = self.slots.get_mut(&handle)?;
        if slot.delivering {
            return None;
        }
        let Kernel::Registered {
            last: Some(is_frozen),
        } = slot.kernel
        else {
            return None;
        };
        // AOSP prunes dead callbacks here too; an emptied list keeps the registration.
        slot.entries.retain(|e| e.callback.strong_count() > 0);
        let callbacks: Vec<_> = slot
            .entries
            .iter_mut()
            .filter(|e| e.owner == owner && e.delivered != Some(is_frozen))
            .filter_map(|e| {
                let callback = e.callback.upgrade()?;
                e.delivered = Some(is_frozen);
                Some(callback)
            })
            .collect();
        if callbacks.is_empty() {
            return None;
        }
        slot.delivering = true;
        Some((slot.id, FrozenState::from(is_frozen), callbacks))
    }

    /// Releases the claim [`begin_batch`](Self::begin_batch) took, if its slot still exists.
    pub(crate) fn end_batch(&mut self, handle: u32, token: u64) {
        if let Some(slot) = self.slots.get_mut(&handle).filter(|slot| slot.id == token) {
            slot.delivering = false;
        }
    }

    #[cfg(test)]
    fn kernel(&self, handle: u32) -> Option<Kernel> {
        self.slots.get(&handle).map(|slot| slot.kernel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::WIBinder;

    struct Noop;
    impl FrozenStateChangeCallback for Noop {
        fn on_state_changed(&self, _: &WIBinder, _: FrozenState) {}
    }

    fn callback() -> Arc<dyn FrozenStateChangeCallback> {
        Arc::new(Noop)
    }

    const H: u32 = 7;
    const A: usize = 0x1000;
    const B: usize = 0x2000;

    #[test]
    fn first_add_requests_and_later_adds_do_not() {
        let mut reg = FreezeRegistry::default();
        let (c1, c2) = (callback(), callback());
        assert_eq!(reg.add(H, A, Arc::downgrade(&c1)), Some(FreezeCmd::Request));
        assert_eq!(reg.add(H, A, Arc::downgrade(&c2)), None);
        // A second proxy of the same handle (case (b) overlap) must not request again.
        assert_eq!(reg.add(H, B, Arc::downgrade(&c1)), None);
        assert_eq!(reg.kernel(H), Some(Kernel::Registered { last: None }));
    }

    #[test]
    fn last_removal_clears_once() {
        let mut reg = FreezeRegistry::default();
        let (c1, c2) = (callback(), callback());
        reg.add(H, A, Arc::downgrade(&c1));
        reg.add(H, A, Arc::downgrade(&c2));
        assert_eq!(reg.remove(H, A, &Arc::downgrade(&c1)), Ok(None));
        assert_eq!(
            reg.remove(H, A, &Arc::downgrade(&c2)),
            Ok(Some(FreezeCmd::Clear))
        );
        assert_eq!(
            reg.kernel(H),
            Some(Kernel::PendingClear { rerequest: false })
        );
        // Nothing left to remove: no second clear.
        assert_eq!(
            reg.remove(H, A, &Arc::downgrade(&c2)),
            Err(StatusCode::NameNotFound)
        );
        assert_eq!(reg.drop_owner(H, A), None);
    }

    #[test]
    fn remove_matches_owner_and_first_entry_only() {
        let mut reg = FreezeRegistry::default();
        let c = callback();
        reg.add(H, A, Arc::downgrade(&c));
        reg.add(H, A, Arc::downgrade(&c));
        assert_eq!(
            reg.remove(H, B, &Arc::downgrade(&c)),
            Err(StatusCode::NameNotFound)
        );
        assert_eq!(reg.remove(H, A, &Arc::downgrade(&c)), Ok(None));
        assert_eq!(
            reg.remove(H, A, &Arc::downgrade(&c)),
            Ok(Some(FreezeCmd::Clear))
        );
        assert_eq!(
            reg.remove(99, A, &Arc::downgrade(&c)),
            Err(StatusCode::NameNotFound)
        );
    }

    #[test]
    fn dropping_old_proxy_keeps_new_proxys_registration() {
        let mut reg = FreezeRegistry::default();
        let (old, new) = (callback(), callback());
        reg.add(H, A, Arc::downgrade(&old));
        reg.add(H, B, Arc::downgrade(&new));
        assert_eq!(reg.drop_owner(H, A), None);
        assert_eq!(reg.drop_owner(H, B), Some(FreezeCmd::Clear));
    }

    #[test]
    fn drop_owner_clears_a_registration_emptied_by_pruning() {
        let mut reg = FreezeRegistry::default();
        let c = callback();
        reg.add(H, A, Arc::downgrade(&c));
        drop(c);
        assert!(reg.state_changed(H, true));
        assert!(reg.begin_batch(H, A).is_none());
        assert_eq!(reg.drop_owner(H, A), Some(FreezeCmd::Clear));
    }

    #[test]
    fn add_during_clear_defers_the_request() {
        let mut reg = FreezeRegistry::default();
        let c = callback();
        reg.add(H, A, Arc::downgrade(&c));
        assert_eq!(reg.drop_owner(H, A), Some(FreezeCmd::Clear));
        // No request while the clear is in flight (kernel would refuse a second one).
        assert_eq!(reg.add(H, B, Arc::downgrade(&c)), None);
        assert_eq!(reg.clear_done(H), ClearDone::Rerequest);
        assert_eq!(reg.kernel(H), Some(Kernel::Registered { last: None }));
    }

    #[test]
    fn add_then_remove_during_clear_does_not_rerequest() {
        let mut reg = FreezeRegistry::default();
        let c = callback();
        reg.add(H, A, Arc::downgrade(&c));
        reg.drop_owner(H, A);
        reg.add(H, B, Arc::downgrade(&c));
        assert_eq!(reg.remove(H, B, &Arc::downgrade(&c)), Ok(None));
        assert_eq!(reg.clear_done(H), ClearDone::Removed);
        assert_eq!(reg.kernel(H), None);
        // A fresh add after removal requests again.
        assert_eq!(reg.add(H, B, Arc::downgrade(&c)), Some(FreezeCmd::Request));
    }

    #[test]
    fn unexpected_clear_done_changes_nothing() {
        let mut reg = FreezeRegistry::default();
        assert_eq!(reg.clear_done(H), ClearDone::Unexpected);
        let c = callback();
        reg.add(H, A, Arc::downgrade(&c));
        assert_eq!(reg.clear_done(H), ClearDone::Unexpected);
        assert_eq!(reg.kernel(H), Some(Kernel::Registered { last: None }));
    }

    #[test]
    fn abort_request_forgets_the_slot() {
        let mut reg = FreezeRegistry::default();
        let c = callback();
        assert_eq!(reg.add(H, A, Arc::downgrade(&c)), Some(FreezeCmd::Request));
        reg.abort_request(H);
        assert_eq!(reg.kernel(H), None);
        assert_eq!(reg.add(H, A, Arc::downgrade(&c)), Some(FreezeCmd::Request));
    }

    #[test]
    fn repeated_and_pending_clear_states_are_filtered() {
        let mut reg = FreezeRegistry::default();
        let c = callback();
        assert!(!reg.state_changed(H, true), "no slot");
        reg.add(H, A, Arc::downgrade(&c));
        assert!(reg.state_changed(H, false));
        assert!(
            !reg.state_changed(H, false),
            "C driver resend of the same state"
        );
        assert!(reg.state_changed(H, true));
        reg.drop_owner(H, A);
        assert!(!reg.state_changed(H, false), "clear in flight");
    }

    #[test]
    fn batches_deliver_each_state_once_to_the_live_owner() {
        let mut reg = FreezeRegistry::default();
        let (mine, theirs) = (callback(), callback());
        reg.add(H, A, Arc::downgrade(&mine));
        reg.add(H, B, Arc::downgrade(&theirs));
        assert!(reg.begin_batch(H, A).is_none(), "no state yet");

        reg.state_changed(H, true);
        let (token, state, callbacks) = reg.begin_batch(H, A).expect("batch");
        assert_eq!(state, FrozenState::Frozen);
        assert_eq!(callbacks.len(), 1);
        assert!(Arc::ptr_eq(&callbacks[0], &mine));
        reg.end_batch(H, token);
        assert!(reg.begin_batch(H, A).is_none(), "already delivered");

        // A late add gets the cached state.
        let late = callback();
        reg.add(H, A, Arc::downgrade(&late));
        let (token, state, callbacks) = reg.begin_batch(H, A).expect("late batch");
        assert_eq!(state, FrozenState::Frozen);
        assert!(Arc::ptr_eq(&callbacks[0], &late));
        reg.end_batch(H, token);
    }

    #[test]
    fn a_second_deliverer_backs_off_until_the_first_ends() {
        let mut reg = FreezeRegistry::default();
        let c = callback();
        reg.add(H, A, Arc::downgrade(&c));
        reg.state_changed(H, true);
        let (token, _, _) = reg.begin_batch(H, A).expect("first");
        reg.state_changed(H, false);
        assert!(reg.begin_batch(H, A).is_none(), "in progress");
        reg.end_batch(H, token);
        let (_, state, _) = reg.begin_batch(H, A).expect("re-check after the batch");
        assert_eq!(state, FrozenState::Unfrozen);
    }

    /// A batch whose slot was cleared and re-created must not release the new slot's claim.
    #[test]
    fn a_stale_batch_cannot_end_a_newer_slots_batch() {
        let mut reg = FreezeRegistry::default();
        let c = callback();
        reg.add(H, A, Arc::downgrade(&c));
        reg.state_changed(H, true);
        let (stale, _, _) = reg.begin_batch(H, A).expect("old slot's batch");
        assert_eq!(reg.drop_owner(H, A), Some(FreezeCmd::Clear));
        assert_eq!(reg.clear_done(H), ClearDone::Removed);
        assert_eq!(reg.add(H, B, Arc::downgrade(&c)), Some(FreezeCmd::Request));
        reg.state_changed(H, false);
        let (fresh, _, _) = reg.begin_batch(H, B).expect("new slot's batch");
        assert_ne!(stale, fresh);

        reg.end_batch(H, stale);
        reg.state_changed(H, true);
        assert!(
            reg.begin_batch(H, B).is_none(),
            "the new slot's batch is still in progress"
        );
        reg.end_batch(H, fresh);
        assert!(reg.begin_batch(H, B).is_some());
    }

    #[test]
    fn rerequest_redelivers_the_new_initial_state() {
        let mut reg = FreezeRegistry::default();
        let c = callback();
        reg.add(H, A, Arc::downgrade(&c));
        reg.state_changed(H, true);
        let (token, _, _) = reg.begin_batch(H, A).expect("batch");
        reg.end_batch(H, token);
        reg.drop_owner(H, A);
        reg.add(H, B, Arc::downgrade(&c));
        assert_eq!(reg.clear_done(H), ClearDone::Rerequest);
        assert!(reg.state_changed(H, true));
        let (_, state, _) = reg.begin_batch(H, B).expect("re-delivered");
        assert_eq!(state, FrozenState::Frozen);
    }
}
