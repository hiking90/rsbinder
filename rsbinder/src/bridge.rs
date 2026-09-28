// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Helpers for a process that sits between two others.
//!
//! A **gateway** re-publishes a service it reached over one transport onto
//! another (see the book's [cross-transport chapter]). Plain data flows
//! through it untouched, but every *binder object* crossing it has to be
//! unwrapped and wrapped again: a proxy of one session cannot enter a parcel
//! of another, and rsbinder refuses it at write time.
//!
//! Wrapping is one call — `BnFoo::new_binder(proxy)`. Doing it *per
//! forwarded call* is the trap: each `new_binder` mints a new object, so the
//! upstream sees a different callback identity every time and any interface
//! that pairs registration with removal by identity breaks.
//! [`Rewrap`](crate::bridge::Rewrap) is the memo table that fixes it — same
//! remote in, same local out.
//!
//! [cross-transport chapter]: https://hiking90.github.io/rsbinder/cross-transport-services.html

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::binder::{DeathRecipient, FromIBinder, Interface, SIBinder, Strong, WIBinder};

/// One remote → one local wrapper, for as long as the pair is alive.
///
/// # Why it exists
///
/// A gateway forwarding `register(cb)` must hand its upstream a binder of
/// its own, not the caller's proxy. Written inline that is
/// `BnCallback::new_binder(cb.clone())`, which is correct per call and wrong
/// across calls: `register(cb)` then `unregister(cb)` arrive upstream as two
/// unrelated objects, so the removal matches nothing.
///
/// `Rewrap` keeps the mapping, so the second call re-uses the wrapper the
/// first one made:
///
/// ```ignore
/// struct Gateway {
///     upstream: Strong<dyn IFoo>,
///     callbacks: Rewrap<dyn ICallback>,
/// }
///
/// // once, at construction:
/// callbacks: Rewrap::new(BnCallback::new_binder),
///
/// fn register(&self, cb: &Strong<dyn ICallback>) -> BinderResult<()> {
///     self.upstream.register(&self.callbacks.wrap(cb))
/// }
/// fn unregister(&self, cb: &Strong<dyn ICallback>) -> BinderResult<()> {
///     self.upstream.unregister(&self.callbacks.wrap(cb))   // same object
/// }
/// ```
///
/// # Lifetimes
///
/// The table holds **weak** references to both halves, so it never keeps a
/// wrapper alive on its own: the wrapper lives exactly as long as the
/// upstream holds it. Once the upstream drops it — or the remote dies, or
/// its session ends — the entry stops matching and the next `wrap` mints a
/// fresh object, which is the correct answer at that point.
///
/// Dead entries are also removed eagerly, by a death notification on the
/// remote, and swept whenever the table is next written to; [`purge_dead`]
/// forces a sweep.
///
/// Identity is the remote's `Arc` address. A live entry holds its own weak
/// reference to that allocation, so the address cannot be recycled while the
/// entry stands; a hit is still confirmed against the stored binder.
///
/// `Rewrap` is transport-agnostic: the remote may be an RPC proxy or a
/// kernel one.
///
/// # Implementation notes
///
/// - `wrap` mints outside the table lock: `make_local` is user code,
///   `link_to_death` reaches the driver or the RPC session, and a re-entrant
///   `wrap` from inside `make_local` would deadlock on the non-reentrant
///   `std::sync::Mutex`. Unlinks likewise run after the lock is dropped.
/// - The death link is best effort: a local binder has no death to report
///   (`InvalidOperation`), and a proxy whose peer is already gone returns
///   `DeadObject`. Either way a later sweep (the next `wrap` miss, or
///   `purge_dead`) collects the entry once either half is gone; a call's own
///   sweep runs before its entry is inserted.
/// - When two threads race to the same remote, the wrapper already in the
///   table wins, since it may already be upstream — unless it fails to cast
///   to `I`, in which case the new one displaces it and the displaced entry
///   is unlinked with the swept ones.
/// - The table is swept on the `wrap` miss path only: minting a binder and a
///   death link costs far more than scanning the table, and the sweep bounds
///   the table without a timer.
/// - The death callback evicts an entry only if its remote is the binder that
///   died, since the key may have been re-used. Liveness cannot stand in for
///   identity: both obituary drivers hold a strong reference to the remote
///   across the callback. The callback does not unlink; the link dies with
///   the reporting proxy.
/// - Dropping the table unlinks every entry. Otherwise each proxy keeps a
///   dead `Weak` in its recipient list, which on the kernel arm blocks every
///   later `BC_REQUEST`/`BC_CLEAR` transition.
/// - An entry leaves the table outside `Drop` and the death callback only
///   through `take_removed`, which returns every swept entry and whatever an
///   insert displaced. Dropping an entry in place only makes it inert (a
///   proxy's recipient list never shrinks on its own), so the caller unlinks
///   each returned entry once it has dropped the table lock, since unlinking
///   reaches into the proxy.
/// - `unlink_all` finishes the list even when one entry's unlink panics.
///   Nothing prunes a dead `Weak` from a proxy's recipient list, and
///   `link_to_death` queues `BC_REQUEST_DEATH_NOTIFICATION` only while that
///   list is empty, so one skipped unlink costs an unrelated later caller its
///   death notification on that proxy. The panic is reachable without a bug
///   here: `ProxyHandle::link_to_death` holds its `recipients` write guard
///   across `talk_with_driver`, which panics if the driver under-consumes,
///   and that poisons the guard so every later unlink on that proxy panics
///   too. The first panic is re-raised once the list is finished, so a
///   non-`Drop` caller still fails; `Drop` swallows it, since unwinding out of
///   a drop during another unwind aborts.
/// - There is no `Default`: the only argument-free factory is `|p| p`, which
///   hands the remote straight back and is then refused at the transport
///   boundary — a silently wrong table is worse than none.
///
/// [`purge_dead`]: Self::purge_dead
pub struct Rewrap<I: FromIBinder + ?Sized> {
    // `Box<dyn Fn>`, not a fn pointer, so a capturing closure is accepted too.
    #[allow(clippy::type_complexity)]
    make_local: Box<dyn Fn(Strong<I>) -> Strong<I> + Send + Sync>,
    // `Arc` so a `Reaper` can hold a `Weak` to just the map: free of `I`, out of any cycle.
    entries: Arc<Mutex<HashMap<usize, Entry>>>,
}

struct Entry {
    remote: WIBinder,
    local: WIBinder,
    /// Owned here: the death link holds only a `Weak` to its recipient.
    reaper: Arc<Reaper>,
}

impl Entry {
    /// The live `(remote, local)` pair, or `None` if either half is gone.
    fn live(&self) -> Option<(SIBinder, SIBinder)> {
        Some((self.remote.upgrade().ok()?, self.local.upgrade().ok()?))
    }

    /// Undo the death link (dropping only makes the entry inert); call without the table lock.
    fn unlink(&self) {
        if let Ok(remote) = self.remote.upgrade() {
            let _ = remote.unlink_to_death_arc(&self.reaper);
        }
    }
}

/// Sweep dead entries and do `insert`; returns swept + displaced ones to unlink after unlocking.
fn take_removed(entries: &mut HashMap<usize, Entry>, insert: Option<(usize, Entry)>) -> Vec<Entry> {
    let keys: Vec<usize> = entries
        .iter()
        .filter(|(_, e)| e.live().is_none())
        .map(|(k, _)| *k)
        .collect();
    let mut removed: Vec<Entry> = keys.iter().filter_map(|k| entries.remove(k)).collect();
    if let Some((key, entry)) = insert {
        // A still-live entry under `key` is displaced and returned like a swept one.
        removed.extend(entries.insert(key, entry));
    }
    removed
}

/// Unlink every entry, past a panicking one; `resume` re-raises the first. See `Rewrap` impl notes.
fn unlink_all(dead: &[Entry], resume: bool) {
    let mut first: Option<Box<dyn std::any::Any + Send>> = None;
    for e in dead {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.unlink()))
        {
            log::error!("Rewrap: unlink panicked; a death link is left registered on the remote");
            if first.is_none() {
                first = Some(payload);
            }
        }
    }
    if let Some(payload) = first {
        if resume {
            std::panic::resume_unwind(payload);
        }
    }
}

/// Removes one entry when its remote dies; the `Weak` table keeps a death link from pinning it.
struct Reaper {
    table: std::sync::Weak<Mutex<HashMap<usize, Entry>>>,
    key: usize,
}

impl DeathRecipient for Reaper {
    fn binder_died(&self, who: &WIBinder) {
        let Some(entries) = self.table.upgrade() else {
            return;
        };
        let mut entries = entries.lock().expect("rewrap table poisoned");
        // Match identity, not liveness (key may be re-used); see `Rewrap` implementation notes.
        if entries.get(&self.key).is_some_and(|e| e.remote == *who) {
            entries.remove(&self.key);
        }
        // No unlink: the link dies with the proxy that is reporting it.
    }
}

impl<I: FromIBinder + ?Sized> Rewrap<I> {
    /// Build a table whose wrappers are made by `make_local`.
    ///
    /// `make_local` is normally `BnFoo::new_binder` itself — the generated
    /// delegating impl means the proxy satisfies `new_binder`'s bound.
    pub fn new(make_local: impl Fn(Strong<I>) -> Strong<I> + Send + Sync + 'static) -> Self {
        Self {
            make_local: Box::new(make_local),
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The local wrapper for `remote`, minting one on first sight.
    ///
    /// Calling this twice with the same live remote returns the *same*
    /// object, which is the point. It is infallible: anything that would
    /// make the cached answer wrong (a dead remote, a dropped wrapper, an
    /// interface cast that no longer holds) falls back to a fresh wrapper.
    pub fn wrap(&self, remote: &Strong<I>) -> Strong<I> {
        let remote_binder = remote.as_binder();
        let key = key_of(&remote_binder);

        if let Some(hit) = self.lookup(key, &remote_binder) {
            return hit;
        }

        // Mint outside the lock (user code, driver calls); see `Rewrap` implementation notes.
        let local = (self.make_local)(remote.clone());
        let reaper = Arc::new(Reaper {
            table: Arc::downgrade(&self.entries),
            key,
        });
        // Best effort: a later sweep collects a failed link; see `Rewrap` implementation notes.
        let _ = remote_binder.link_to_death_arc(&reaper);

        let mut entries = self.entries.lock().expect("rewrap table poisoned");
        // A racing thread's wrapper may already be upstream, so it wins if it casts to `I`.
        if let Some((r, l)) = entries.get(&key).and_then(Entry::live) {
            if r == remote_binder {
                if let Ok(existing) = <I as FromIBinder>::try_from(l) {
                    drop(entries);
                    let _ = remote_binder.unlink_to_death_arc(&reaper);
                    return existing;
                }
            }
        }
        // Swept on the miss path only; this bounds the table without a timer.
        let dead = take_removed(
            &mut entries,
            Some((
                key,
                Entry {
                    remote: SIBinder::downgrade(&remote_binder),
                    local: SIBinder::downgrade(&local.as_binder()),
                    reaper,
                },
            )),
        );
        drop(entries);
        // Unlink outside the lock: it takes the proxy's recipient lock and can reach the driver.
        unlink_all(&dead, true);
        local
    }

    /// Drop every entry whose remote or wrapper is gone, and report how many
    /// went. `wrap` sweeps too; this is for a gateway that wants to reclaim
    /// on its own schedule, and for tests.
    ///
    /// Not a pure table sweep: an entry collected because its *wrapper* is
    /// gone is also unlinked from its still-live remote, so a kernel proxy
    /// that loses its last recipient here emits `BC_CLEAR_DEATH_NOTIFICATION`
    /// and flushes it to the driver. An entry collected because the remote
    /// itself is gone costs nothing extra — there is no proxy left to unlink.
    pub fn purge_dead(&self) -> usize {
        let dead = {
            let mut entries = self.entries.lock().expect("rewrap table poisoned");
            take_removed(&mut entries, None)
        };
        unlink_all(&dead, true);
        dead.len()
    }

    /// How many pairs the table currently holds, dead ones included (see
    /// [`purge_dead`](Self::purge_dead)).
    pub fn len(&self) -> usize {
        self.entries.lock().expect("rewrap table poisoned").len()
    }

    /// Whether the table holds no pairs at all — dead ones included, like
    /// [`len`](Self::len).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lookup(&self, key: usize, remote_binder: &SIBinder) -> Option<Strong<I>> {
        let entries = self.entries.lock().expect("rewrap table poisoned");
        let (r, l) = entries.get(&key).and_then(Entry::live)?;
        // Confirm identity, in case the slot was re-keyed since the entry.
        (&r == remote_binder)
            .then(|| <I as FromIBinder>::try_from(l).ok())
            .flatten()
    }
}

impl<I: FromIBinder + ?Sized> Drop for Rewrap<I> {
    fn drop(&mut self) {
        // Unlink all, or each proxy keeps a dead `Weak`; see `Rewrap` implementation notes.
        let dead: Vec<Entry> = {
            // Never panic in a `Drop`: drain a poisoned table instead.
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries.drain().map(|(_, e)| e).collect()
        };
        // `false`: a panic leaving `Drop` during an unwind would abort the process.
        unlink_all(&dead, false);
    }
}

// No `Default`: see `Rewrap` implementation notes.

impl<I: FromIBinder + ?Sized> std::fmt::Debug for Rewrap<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rewrap").field("len", &self.len()).finish()
    }
}

// Thin data address (vtables may differ); `Entry.remote`'s `Weak` keeps it from being reused.
fn key_of(binder: &SIBinder) -> usize {
    Arc::as_ptr(binder.as_arc()) as *const () as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::{IBinder, Stability};
    use std::mem::ManuallyDrop;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `unlink_to_death` counts or panics, like a proxy with a poisoned `recipients` lock.
    struct Unlinkable {
        panics: bool,
        unlinked: Arc<AtomicUsize>,
    }

    impl IBinder for Unlinkable {
        fn link_to_death(&self, _: std::sync::Weak<dyn DeathRecipient>) -> crate::Result<()> {
            Ok(())
        }
        fn unlink_to_death(&self, _: std::sync::Weak<dyn DeathRecipient>) -> crate::Result<()> {
            assert!(!self.panics, "injected unlink panic");
            self.unlinked.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn ping_binder(&self) -> crate::Result<()> {
            Ok(())
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_transactable(&self) -> Option<&dyn crate::Transactable> {
            None
        }
        fn descriptor(&self) -> &str {
            "rsbinder.test.IUnlinkable"
        }
        fn is_remote(&self) -> bool {
            true
        }
        fn stability(&self) -> Stability {
            Stability::default()
        }
        fn inc_strong(&self, _: &SIBinder) -> crate::Result<()> {
            Ok(())
        }
        fn attempt_inc_strong(&self) -> bool {
            true
        }
        fn dec_strong(&self, _: Option<ManuallyDrop<SIBinder>>) -> crate::Result<()> {
            Ok(())
        }
        fn inc_weak(&self, _: &WIBinder) -> crate::Result<()> {
            Ok(())
        }
        fn dec_weak(&self) -> crate::Result<()> {
            Ok(())
        }
    }

    fn entry(panics: bool, unlinked: &Arc<AtomicUsize>) -> (SIBinder, Entry) {
        let b = SIBinder::new(Arc::new(Unlinkable {
            panics,
            unlinked: unlinked.clone(),
        }))
        .expect("SIBinder::new");
        let e = Entry {
            remote: SIBinder::downgrade(&b),
            local: SIBinder::downgrade(&b),
            reaper: Arc::new(Reaper {
                table: std::sync::Weak::new(),
                key: 0,
            }),
        };
        // The caller keeps `b` alive so `unlink`'s `remote.upgrade()` succeeds.
        (b, e)
    }

    /// One entry panicking must not cost the rest their unlink; `resume = true` re-raises it.
    #[test]
    fn unlink_all_finishes_the_list_after_a_panic() {
        let unlinked = Arc::new(AtomicUsize::new(0));
        let (_k0, e0) = entry(false, &unlinked);
        let (_k1, e1) = entry(true, &unlinked); // panics
        let (_k2, e2) = entry(false, &unlinked);
        let dead = vec![e0, e1, e2];

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let out =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unlink_all(&dead, true)));
        std::panic::set_hook(prev);

        assert!(out.is_err(), "resume=true must re-raise the entry's panic");
        assert_eq!(
            unlinked.load(Ordering::SeqCst),
            2,
            "the entries either side of the panicking one must still be unlinked"
        );
    }

    /// `Drop` swallows the panic: unwinding out of a drop during another unwind aborts.
    #[test]
    fn unlink_all_swallows_the_panic_for_drop() {
        let unlinked = Arc::new(AtomicUsize::new(0));
        let (_k0, e0) = entry(true, &unlinked); // panics first
        let (_k1, e1) = entry(false, &unlinked);
        let dead = vec![e0, e1];

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let out =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unlink_all(&dead, false)));
        std::panic::set_hook(prev);

        assert!(out.is_ok(), "resume=false must not propagate");
        assert_eq!(unlinked.load(Ordering::SeqCst), 1);
    }
}
