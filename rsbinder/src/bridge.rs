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
/// [`purge_dead`]: Self::purge_dead
pub struct Rewrap<I: FromIBinder + ?Sized> {
    // `Box<dyn Fn>` rather than a fn pointer so a capturing closure is also
    // accepted; `new` takes `impl Fn`, so `BnFoo::new_binder` passes as-is.
    #[allow(clippy::type_complexity)]
    make_local: Box<dyn Fn(Strong<I>) -> Strong<I> + Send + Sync>,
    // `Arc`, not a plain field, so a `Reaper` can hold a `Weak` to just the
    // map — keeping the death recipient free of `I` and out of any cycle.
    entries: Arc<Mutex<HashMap<usize, Entry>>>,
}

struct Entry {
    remote: WIBinder,
    local: WIBinder,
    /// The death link holds only a weak reference to its recipient, so the
    /// table has to own the `Arc` or the notification would never fire.
    reaper: Arc<Reaper>,
}

impl Entry {
    /// The live `(remote, local)` pair, or `None` if either half is gone.
    fn live(&self) -> Option<(SIBinder, SIBinder)> {
        Some((self.remote.upgrade().ok()?, self.local.upgrade().ok()?))
    }

    /// Undo the death link: dropping the entry only makes it inert, and a
    /// proxy's recipient list never shrinks on its own. Call with the table
    /// lock released — this reaches into the proxy.
    fn unlink(&self) {
        if let Ok(remote) = self.remote.upgrade() {
            let _ = remote.unlink_to_death_arc(&self.reaper);
        }
    }
}

/// The one place an entry leaves the table outside `Drop` and
/// `Reaper::binder_died`: it takes every entry whose remote or wrapper is
/// gone and, when `insert` is given, installs it — returning the swept
/// entries *and* whatever the insert displaced. Dropping an `Entry` in place
/// only makes it inert (`Entry::unlink`), so the caller must unlink every
/// entry returned here, once it has dropped the table lock.
fn take_removed(entries: &mut HashMap<usize, Entry>, insert: Option<(usize, Entry)>) -> Vec<Entry> {
    let keys: Vec<usize> = entries
        .iter()
        .filter(|(_, e)| e.live().is_none())
        .map(|(k, _)| *k)
        .collect();
    let mut removed: Vec<Entry> = keys.iter().filter_map(|k| entries.remove(k)).collect();
    if let Some((key, entry)) = insert {
        // A key whose entry is still live is re-filled rather than swept —
        // the displaced entry leaves the table like any other removal.
        removed.extend(entries.insert(key, entry));
    }
    removed
}

/// Unlink every entry that left the table, finishing the list even when one
/// entry's proxy panics.
///
/// Abandoning the rest is the damage worth avoiding: nothing prunes a dead
/// `Weak` from a proxy's recipient list, and `link_to_death` queues
/// `BC_REQUEST_DEATH_NOTIFICATION` only while that list is empty — so one
/// skipped unlink silently costs an unrelated later caller its death
/// notification on that proxy. A panic here is reachable without a bug in
/// this file: `ProxyHandle::link_to_death` holds its `recipients` write guard
/// across `talk_with_driver`, which panics if the driver under-consumes, and
/// that poisons the guard so every later unlink on that proxy panics too.
///
/// `resume` re-raises the first panic once the list is finished, so a
/// non-`Drop` caller still fails; `Drop` passes `false`, since unwinding out
/// of a drop during another unwind aborts.
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

/// Removes one entry when its remote dies. Holds a `Weak` to the table so a
/// live death link cannot keep the gateway's table alive.
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
        // Evict only the binder that died: the slot may have been re-keyed
        // since this link was made. Its remote is still alive here — both
        // obituary drivers hold a strong reference across the callback — so
        // liveness cannot stand in for identity.
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

        // Mint outside the lock: `make_local` is user code and
        // `link_to_death` reaches the driver / the RPC session, and a
        // re-entrant `wrap` from inside `make_local` would deadlock
        // (`std::sync::Mutex` is not reentrant).
        let local = (self.make_local)(remote.clone());
        let reaper = Arc::new(Reaper {
            table: Arc::downgrade(&self.entries),
            key,
        });
        // Best effort: a local binder has no death to report
        // (`InvalidOperation`), and a proxy whose peer is already gone
        // returns `DeadObject`. Either way a *later* sweep — the next `wrap`
        // miss, or `purge_dead` — collects the entry once either half is
        // gone; this call's sweep runs before the entry is inserted.
        let _ = remote_binder.link_to_death_arc(&reaper);

        let mut entries = self.entries.lock().expect("rewrap table poisoned");
        // Another thread may have raced us to the same remote. Its wrapper
        // is as good as ours and may already be upstream, so it wins —
        // unless it no longer casts to `I`, in which case ours displaces it
        // below and the displaced entry is unlinked with the others.
        if let Some((r, l)) = entries.get(&key).and_then(Entry::live) {
            if r == remote_binder {
                if let Ok(existing) = <I as FromIBinder>::try_from(l) {
                    drop(entries);
                    let _ = remote_binder.unlink_to_death_arc(&reaper);
                    return existing;
                }
            }
        }
        // Swept on the miss path only: minting a binder and a death link
        // already costs far more than scanning a table this size, and it
        // bounds the table without a timer.
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
        // Unlink outside the lock: it takes the proxy's recipient lock and can
        // reach the driver, and this `Mutex` is not reentrant.
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
        // Dropping the table removes every entry, and the links outlive it
        // otherwise: the `Arc<Reaper>`s go, but each proxy keeps a dead
        // `Weak` in its recipient list, which on the kernel arm blocks every
        // later `BC_REQUEST`/`BC_CLEAR` transition.
        let dead: Vec<Entry> = {
            // Never panic in a `Drop`: drain a poisoned table instead.
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries.drain().map(|(_, e)| e).collect()
        };
        // `false`: a panic must not leave this `Drop`, or an unwind already in
        // progress would abort the process.
        unlink_all(&dead, false);
    }
}

// No `Default`: the only factory that needs no arguments is `|p| p`, which
// hands the remote straight back and is then refused at the boundary — a
// silently wrong table is worse than none.

impl<I: FromIBinder + ?Sized> std::fmt::Debug for Rewrap<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rewrap").field("len", &self.len()).finish()
    }
}

// Identity key: the `Arc`'s data address, thinned so two vtables for the same
// allocation cannot disagree. Stable while `Entry.remote`'s `Weak` reserves
// the allocation.
fn key_of(binder: &SIBinder) -> usize {
    Arc::as_ptr(binder.as_arc()) as *const () as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::{IBinder, Stability};
    use std::mem::ManuallyDrop;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A binder whose `unlink_to_death` either counts the call or panics —
    /// standing in for a proxy whose `recipients` lock has been poisoned by an
    /// earlier driver panic, which is how this is reachable in production.
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
        // The caller keeps the strong `SIBinder` alive so `unlink`'s
        // `remote.upgrade()` succeeds.
        (b, e)
    }

    /// One entry panicking must not cost the rest their unlink. Nothing prunes
    /// a dead `Weak` from a proxy's recipient list, so a skipped unlink
    /// silently blocks a later `link_to_death` from subscribing.
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

    /// `Drop` swallows the panic instead: unwinding out of a drop that is
    /// itself running during an unwind aborts the process.
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
