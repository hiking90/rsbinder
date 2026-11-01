// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Death notification bookkeeping behind `IBinder::link_to_death`: the
//! recipient list each proxy keeps, kernel or RPC (`DeathRecipients`), and the
//! per-handle kernel registration (`DeathRegistry`).
//!
//! # Recipient list
//!
//! `DeathRecipients` is the list and its `obituary_sent` latch, with the rules
//! the `proxy` module doc "Death notifications" states: the latch is read and
//! set only under the list's lock, the obituary detaches the list under the
//! lock and runs the callbacks after it, each callback (and the drop of the
//! recipient it upgraded) is caught. A kernel proxy passes its registry calls
//! as the closures that run under the lock, since the registration order
//! depends on them; an RPC proxy passes none. The lock recovers from poison
//! (`proxy` module doc).
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
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use crate::binder::{DeathRecipient, SIBinder, WIBinder};
use crate::error::{panic_message, Result, StatusCode};
use crate::freeze::ClearDone;

/// A death notification that is unregistered when this value drops.
///
/// [`DeathLink::new`] links `recipient` to `binder` with
/// [`IBinder::link_to_death`](crate::IBinder::link_to_death) and keeps a
/// strong reference to both, so the caller need not keep the recipient alive
/// for the notification to fire. Dropping the link calls
/// [`unlink_to_death`](crate::IBinder::unlink_to_death) and then drops the
/// binder reference.
///
/// The drop never panics past itself: the unlink and the binder's drop run
/// under `catch_unwind`, so a panic there (a poisoned lock inside rsbinder, or
/// the last reference to a proxy whose own drop panics) is logged at error
/// level instead of unwinding, which would abort the process if the link were
/// dropped during another unwind. An unlink that returns an error is logged at
/// debug level: [`StatusCode::DeadObject`] is what an unlink returns after
/// the binder's death was delivered.
///
/// A recipient linked to the same binder twice is notified twice; each link
/// removes one registration when it drops (see `unlink_to_death`).
#[must_use = "dropping the link unregisters the death notification"]
pub struct DeathLink {
    /// `None` only inside `Drop`, which drops it under the catch.
    binder: Option<SIBinder>,
    recipient: Arc<dyn DeathRecipient>,
}

impl DeathLink {
    /// Link `recipient` to `binder`; on failure, what `link_to_death` returned, such as
    /// [`StatusCode::DeadObject`] for a binder whose death was already delivered, or
    /// [`StatusCode::InvalidOperation`] for a local binder.
    pub fn new(binder: &SIBinder, recipient: Arc<dyn DeathRecipient>) -> Result<Self> {
        binder.link_to_death(Arc::downgrade(&recipient))?;
        Ok(Self {
            binder: Some(binder.clone()),
            recipient,
        })
    }
}

impl Drop for DeathLink {
    fn drop(&mut self) {
        let binder = self.binder.take();
        let recipient = Arc::downgrade(&self.recipient);
        // Borrowed here: a binder dropped by this unwind could panic during cleanup and abort.
        let unlinked = catch_unwind(AssertUnwindSafe(|| {
            binder.as_ref().map(|b| b.unlink_to_death(recipient))
        }));
        match unlinked {
            Ok(Some(Err(err))) => log::debug!("death link: unlink_to_death returned {err:?}"),
            Ok(_) => {}
            Err(payload) => log::error!(
                "death link: unlink_to_death panicked: {}",
                panic_message(&payload)
            ),
        }
        if let Err(payload) = catch_unwind(AssertUnwindSafe(move || drop(binder))) {
            log::error!(
                "death link: the binder's drop panicked: {}",
                panic_message(&payload)
            );
        }
    }
}

impl fmt::Debug for DeathLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeathLink")
            .field("binder", &self.binder)
            .finish_non_exhaustive()
    }
}

/// A proxy's death recipients and its `obituary_sent` latch; see the module doc.
pub(crate) struct DeathRecipients {
    list: Mutex<Vec<Weak<dyn DeathRecipient>>>,
    /// Written once, `Release`, under `list`; `is_dead` reads it without the lock.
    obituary_sent: AtomicBool,
}

impl DeathRecipients {
    pub(crate) fn new() -> Self {
        Self {
            list: Mutex::new(Vec::new()),
            obituary_sent: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Weak<dyn DeathRecipient>>> {
        self.list.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `DeadObject` after the obituary, `BadValue` for a dead `recipient`; `on_first` runs
    /// under the lock when the list was empty, and its error leaves the list as it was.
    pub(crate) fn link(
        &self,
        recipient: Weak<dyn DeathRecipient>,
        on_first: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let mut list = self.lock();
        if self.obituary_sent.load(Ordering::Relaxed) {
            return Err(StatusCode::DeadObject);
        }
        // A count, as an `upgrade` could run the recipient's `Drop` under the lock.
        if recipient.strong_count() == 0 {
            return Err(StatusCode::BadValue);
        }
        if list.is_empty() {
            on_first()?;
        }
        list.push(recipient);
        Ok(())
    }

    /// `DeadObject` after the obituary; removes the first match only (AOSP `removeAt(i)`),
    /// `NameNotFound` on none; `on_last` runs under the lock when the list became empty.
    pub(crate) fn unlink(
        &self,
        recipient: Weak<dyn DeathRecipient>,
        on_last: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let mut list = self.lock();
        if self.obituary_sent.load(Ordering::Relaxed) {
            return Err(StatusCode::DeadObject);
        }
        let Some(i) = list.iter().position(|r| Weak::ptr_eq(r, &recipient)) else {
            return Err(StatusCode::NameNotFound);
        };
        list.remove(i);
        if list.is_empty() {
            on_last()?;
        }
        Ok(())
    }

    /// The list, detached and latched; `None` once sent. `on_nonempty` runs first under
    /// the lock, its panic caught: the obituary comes once and must still go out.
    pub(crate) fn take(&self, on_nonempty: impl FnOnce()) -> Option<Vec<Weak<dyn DeathRecipient>>> {
        let mut list = self.lock();
        if self.obituary_sent.load(Ordering::Relaxed) {
            return None;
        }
        if !list.is_empty() {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(on_nonempty)) {
                log::error!(
                    "the obituary's unlink panicked ({}); delivering the obituary anyway",
                    panic_message(&payload)
                );
            }
        }
        let snapshot = std::mem::take(&mut *list);
        // `Release` for the lock-free `is_dead` readers.
        self.obituary_sent.store(true, Ordering::Release);
        Some(snapshot)
    }

    /// `binder_died` on each live recipient with no lock held, its panic and the drop of the
    /// recipient it upgraded caught; `target` names the proxy in the log.
    pub(crate) fn dispatch(
        snapshot: &[Weak<dyn DeathRecipient>],
        who: &WIBinder,
        target: impl fmt::Display,
    ) {
        for weak in snapshot {
            let caught = catch_unwind(AssertUnwindSafe(|| {
                if let Some(recipient) = weak.upgrade() {
                    recipient.binder_died(who);
                }
            }));
            if let Err(payload) = caught {
                let msg = panic_message(&payload);
                log::error!("DeathRecipient panicked during binder_died for {target}: {msg}");
            }
        }
    }

    /// Whether the obituary went out; `Acquire`, for the readers that take no lock.
    pub(crate) fn is_dead(&self) -> bool {
        self.obituary_sent.load(Ordering::Acquire)
    }

    /// Whether a recipient is linked, for a proxy's `Drop`, which needs no lock.
    pub(crate) fn has_recipients(&mut self) -> bool {
        !self
            .list
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    /// A list whose obituary went out already when `obituary_sent`.
    #[cfg(test)]
    pub(crate) fn sent(obituary_sent: bool) -> Self {
        let recipients = Self::new();
        recipients
            .obituary_sent
            .store(obituary_sent, Ordering::Relaxed);
        recipients
    }

    #[cfg(test)]
    pub(crate) fn list(&self) -> MutexGuard<'_, Vec<Weak<dyn DeathRecipient>>> {
        self.lock()
    }
}

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

    mod recipients {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        use super::super::*;
        use crate::binder::{Interface, Remotable, TransactionCode};
        use crate::{Binder, Parcel, SIBinder};

        struct Local;
        impl Interface for Local {}
        impl Remotable for Local {
            fn descriptor() -> &'static str {
                "rsbinder.test.ILocal"
            }
            fn on_transact(
                &self,
                _: TransactionCode,
                _: &mut Parcel,
                _: &mut Parcel,
            ) -> Result<()> {
                Err(StatusCode::UnknownTransaction)
            }
            fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
                Ok(())
            }
        }

        fn who() -> (SIBinder, WIBinder) {
            let strong = Interface::as_binder(&Binder::new(Local));
            let weak = SIBinder::downgrade(&strong);
            (strong, weak)
        }

        /// Counts its notifications.
        struct Counting(Arc<AtomicUsize>);
        impl DeathRecipient for Counting {
            fn binder_died(&self, _: &WIBinder) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        fn counting() -> (Arc<dyn DeathRecipient>, Arc<AtomicUsize>) {
            let count = Arc::new(AtomicUsize::new(0));
            (Arc::new(Counting(count.clone())), count)
        }

        /// A panic under the lock poisons it for real; the list still links and notifies.
        #[test]
        fn a_poisoned_list_still_links_and_notifies() {
            let list = DeathRecipients::new();
            let (first, _) = counting();
            let panicked = catch_unwind(AssertUnwindSafe(|| {
                let _ = list.link(Arc::downgrade(&first), || panic!("the registry panics"));
            }));
            assert!(panicked.is_err());
            assert!(list.list.is_poisoned());

            let (recipient, count) = counting();
            list.link(Arc::downgrade(&recipient), || Ok(()))
                .expect("link");
            let snapshot = list.take(|| {}).expect("first obituary");
            let (_strong, who) = who();
            DeathRecipients::dispatch(&snapshot, &who, "test");
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }

        /// The obituary's unlink panicking still drains and latches the list.
        #[test]
        fn a_panicking_unlink_still_gives_the_obituary() {
            let list = DeathRecipients::new();
            let (recipient, _) = counting();
            list.link(Arc::downgrade(&recipient), || Ok(()))
                .expect("link");
            let snapshot = list.take(|| panic!("the unlink panics"));
            assert_eq!(snapshot.map(|s| s.len()), Some(1));
            assert!(list.is_dead());
            assert!(list.take(|| {}).is_none(), "once");
        }

        /// Drops its own last strong reference in `binder_died`, so its `Drop` runs in dispatch.
        struct DropsItself(std::sync::Mutex<Option<Arc<DropsItself>>>);
        impl DeathRecipient for DropsItself {
            fn binder_died(&self, _: &WIBinder) {
                drop(self.0.lock().expect("lock").take());
            }
        }
        impl Drop for DropsItself {
            fn drop(&mut self) {
                panic!("the recipient's drop panics");
            }
        }

        /// A recipient's `Drop` panicking in dispatch leaves the next one notified.
        #[test]
        fn a_panicking_recipient_drop_starves_no_other() {
            let first = Arc::new(DropsItself(std::sync::Mutex::new(None)));
            *first.0.lock().expect("lock") = Some(first.clone());
            let weak_first: Weak<dyn DeathRecipient> = Arc::downgrade(&first) as _;
            drop(first);
            let (second, count) = counting();
            let snapshot = vec![weak_first, Arc::downgrade(&second)];
            let (_strong, who) = who();
            DeathRecipients::dispatch(&snapshot, &who, "test");
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }

    mod link {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        use super::super::*;
        use crate::binder::{IBinder, Transactable};

        #[derive(Clone, Copy, PartialEq)]
        enum Unlink {
            Ok,
            Dead,
            Panic,
        }

        /// Records its link and the unlinks of that same recipient; `drop_panics` on its drop.
        struct Mock {
            unlink: Unlink,
            linked: std::sync::Mutex<Option<Weak<dyn DeathRecipient>>>,
            unlinked: Arc<AtomicUsize>,
            drop_panics: bool,
        }
        impl IBinder for Mock {
            fn link_to_death(&self, recipient: Weak<dyn DeathRecipient>) -> Result<()> {
                *self.linked.lock().expect("lock") = Some(recipient);
                Ok(())
            }
            fn unlink_to_death(&self, recipient: Weak<dyn DeathRecipient>) -> Result<()> {
                let linked = self.linked.lock().expect("lock").clone().expect("linked");
                assert!(Weak::ptr_eq(&linked, &recipient), "the linked recipient");
                self.unlinked.fetch_add(1, Ordering::SeqCst);
                match self.unlink {
                    Unlink::Ok => Ok(()),
                    Unlink::Dead => Err(StatusCode::DeadObject),
                    Unlink::Panic => panic!("the unlink panics"),
                }
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
                "rsbinder.test.IMock"
            }
            fn is_remote(&self) -> bool {
                true
            }
        }
        impl Drop for Mock {
            fn drop(&mut self) {
                if self.drop_panics {
                    panic!("the binder's drop panics");
                }
            }
        }

        struct Quiet;
        impl DeathRecipient for Quiet {
            fn binder_died(&self, _: &WIBinder) {}
        }

        fn link(unlink: Unlink, drop_panics: bool) -> (DeathLink, Arc<AtomicUsize>) {
            let unlinked = Arc::new(AtomicUsize::new(0));
            let binder = SIBinder::from_arc(Arc::new(Mock {
                unlink,
                linked: std::sync::Mutex::new(None),
                unlinked: unlinked.clone(),
                drop_panics,
            }));
            let link = DeathLink::new(&binder, Arc::new(Quiet)).expect("link");
            (link, unlinked)
        }

        #[test]
        fn a_drop_unlinks_its_recipient_once() {
            for unlink in [Unlink::Ok, Unlink::Dead] {
                let (link, unlinked) = link(unlink, false);
                drop(link);
                assert_eq!(unlinked.load(Ordering::SeqCst), 1);
            }
        }

        #[test]
        fn a_panicking_unlink_stays_inside_the_drop() {
            let (link, unlinked) = link(Unlink::Panic, false);
            assert!(catch_unwind(AssertUnwindSafe(|| drop(link))).is_ok());
            assert_eq!(unlinked.load(Ordering::SeqCst), 1);
        }

        /// The link held the binder's last reference, whose drop panics.
        #[test]
        fn a_panicking_binder_drop_stays_inside_the_drop() {
            let (link, _) = link(Unlink::Ok, true);
            assert!(catch_unwind(AssertUnwindSafe(|| drop(link))).is_ok());
        }

        /// Dropped by another unwind: a panic escaping this drop would abort the test binary.
        #[test]
        fn a_drop_during_an_unwind_does_not_abort() {
            let (link, unlinked) = link(Unlink::Panic, true);
            let outer = catch_unwind(AssertUnwindSafe(move || {
                let _link = link;
                panic!("the outer panic");
            }));
            assert!(outer.is_err());
            assert_eq!(unlinked.load(Ordering::SeqCst), 1);
        }
    }
}
