// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Per-handle bookkeeping for the kernel death registration behind
//! `IBinder::link_to_death`.
//!
//! # Why per handle
//!
//! The death cookie is the handle, and both the C driver (`binder_thread_write`)
//! and the Rust driver (`Process::request_death`, `clear_death`) ignore a
//! second `BC_REQUEST_DEATH_NOTIFICATION` on a ref that has one while honoring
//! a `BC_CLEAR_DEATH_NOTIFICATION` whose cookie matches. A `ProxyHandle` lives
//! per strong lifetime, and a dropping proxy and its replacement can coexist
//! (`process_state` module doc "Proxy cache slow path", case (b)); a dropping
//! proxy's clear can also sit queued on a looper until its handler returns.
//! With the registration owned per proxy, a new proxy's request could reach
//! the kernel before the old proxy's clear: the request is ignored, the clear
//! removes the registration, and the new recipients never hear of the death.
//! The new proxy's own clear then finds nothing ("not active"), so no
//! clear-done arrives and the pin ledger holds the handle's `BC_DECREFS` for
//! good. `tests/tests/death_reorder_kernel.rs` forces that order. AOSP's
//! `BpBinder` has the same shape (`onLastStrongRef` queues the clear,
//! `linkToDeath` flushes the request).
//!
//! # Rules
//!
//! - A slot lists the proxies (`owner` = address) whose recipient list is
//!   non-empty. The registration is requested when the first one appears and
//!   cleared when the last one leaves (unlink, drop, or obituary).
//! - A request is never written while a clear is in flight: `rerequest` defers
//!   it to the clear-done, as freeze notifications do (`freeze` module doc).
//! - The request is flushed under the registry lock, so no clear written
//!   after it (by another thread) can overtake it.

use std::collections::hash_map::Entry as MapEntry;
use std::collections::HashMap;

use crate::freeze::ClearDone;

/// A command the caller writes while still holding the registry lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeathCmd {
    Request,
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kernel {
    Registered,
    PendingClear { rerequest: bool },
}

struct Slot {
    kernel: Kernel,
    linked: Vec<usize>,
}

/// Slots keyed by handle (= kernel cookie). See the module doc.
#[derive(Default)]
pub(crate) struct DeathRegistry {
    slots: HashMap<u32, Slot>,
}

impl DeathRegistry {
    /// `owner`'s recipient list became non-empty; `Some(Request)` when the handle had no slot.
    pub(crate) fn link(&mut self, handle: u32, owner: usize) -> Option<DeathCmd> {
        match self.slots.entry(handle) {
            MapEntry::Vacant(vacant) => {
                vacant.insert(Slot {
                    kernel: Kernel::Registered,
                    linked: vec![owner],
                });
                Some(DeathCmd::Request)
            }
            MapEntry::Occupied(occupied) => {
                let slot = occupied.into_mut();
                if let Kernel::PendingClear { rerequest } = &mut slot.kernel {
                    *rerequest = true;
                }
                if !slot.linked.contains(&owner) {
                    slot.linked.push(owner);
                }
                None
            }
        }
    }

    /// Forgets `handle` after its request never reached the kernel.
    pub(crate) fn abort_request(&mut self, handle: u32) {
        self.slots.remove(&handle);
    }

    /// `owner`'s recipient list became empty; `Some(Clear)` when it was the last.
    pub(crate) fn unlink(&mut self, handle: u32, owner: usize) -> Option<DeathCmd> {
        let slot = self.slots.get_mut(&handle)?;
        slot.linked.retain(|linked| *linked != owner);
        if !slot.linked.is_empty() {
            return None;
        }
        match &mut slot.kernel {
            Kernel::Registered => {
                slot.kernel = Kernel::PendingClear { rerequest: false };
                Some(DeathCmd::Clear)
            }
            Kernel::PendingClear { rerequest } => {
                *rerequest = false;
                None
            }
        }
    }

    /// Records a `BR_CLEAR_DEATH_NOTIFICATION_DONE`.
    pub(crate) fn clear_done(&mut self, handle: u32) -> ClearDone {
        let Some(slot) = self.slots.get_mut(&handle) else {
            return ClearDone::Unexpected;
        };
        match slot.kernel {
            Kernel::Registered => ClearDone::Unexpected,
            Kernel::PendingClear { rerequest } if rerequest && !slot.linked.is_empty() => {
                slot.kernel = Kernel::Registered;
                ClearDone::Rerequest
            }
            Kernel::PendingClear { .. } => {
                self.slots.remove(&handle);
                ClearDone::Removed
            }
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

    const H: u32 = 9;
    const OLD: usize = 0x1000;
    const NEW: usize = 0x2000;

    #[test]
    fn one_request_and_one_clear_per_registration() {
        let mut reg = DeathRegistry::default();
        assert_eq!(reg.link(H, OLD), Some(DeathCmd::Request));
        assert_eq!(reg.link(H, OLD), None, "an owner links once");
        assert_eq!(reg.unlink(H, OLD), Some(DeathCmd::Clear));
        assert_eq!(reg.unlink(H, OLD), None, "nothing left to clear");
        assert_eq!(reg.clear_done(H), ClearDone::Removed);
        assert_eq!(reg.kernel(H), None);
    }

    /// The overlap the module doc describes, the new proxy linking first:
    /// the old proxy's drop must not clear the registration the new one uses.
    #[test]
    fn an_old_proxys_drop_keeps_a_newer_link() {
        let mut reg = DeathRegistry::default();
        assert_eq!(reg.link(H, OLD), Some(DeathCmd::Request));
        assert_eq!(reg.link(H, NEW), None);
        assert_eq!(reg.unlink(H, OLD), None);
        assert_eq!(reg.kernel(H), Some(Kernel::Registered));
        assert_eq!(reg.unlink(H, NEW), Some(DeathCmd::Clear));
    }

    /// The order `death_reorder_kernel` forces: the old clear is written first,
    /// the new link defers its request to the clear-done.
    #[test]
    fn a_link_during_a_clear_requests_after_the_done() {
        let mut reg = DeathRegistry::default();
        reg.link(H, OLD);
        assert_eq!(reg.unlink(H, OLD), Some(DeathCmd::Clear));
        assert_eq!(
            reg.link(H, NEW),
            None,
            "no request while the clear is in flight"
        );
        assert_eq!(reg.clear_done(H), ClearDone::Rerequest);
        assert_eq!(reg.kernel(H), Some(Kernel::Registered));
        assert_eq!(reg.unlink(H, NEW), Some(DeathCmd::Clear));
    }

    #[test]
    fn a_link_undone_during_the_clear_does_not_rerequest() {
        let mut reg = DeathRegistry::default();
        reg.link(H, OLD);
        reg.unlink(H, OLD);
        reg.link(H, NEW);
        assert_eq!(reg.unlink(H, NEW), None);
        assert_eq!(reg.clear_done(H), ClearDone::Removed);
        assert_eq!(reg.link(H, NEW), Some(DeathCmd::Request));
    }

    #[test]
    fn stray_events_change_nothing() {
        let mut reg = DeathRegistry::default();
        assert_eq!(reg.unlink(H, OLD), None);
        assert_eq!(reg.clear_done(H), ClearDone::Unexpected);
        reg.link(H, OLD);
        assert_eq!(reg.clear_done(H), ClearDone::Unexpected);
        assert_eq!(reg.kernel(H), Some(Kernel::Registered));
        reg.abort_request(H);
        assert_eq!(reg.kernel(H), None);
    }
}
