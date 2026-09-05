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
/// callbacks: Rewrap::new(|p| BnCallback::new_binder(p)),
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
/// Identity is the remote's `Arc` address, *confirmed* by upgrading the
/// stored weak reference and comparing binders — an address alone would be
/// vulnerable to allocator reuse after a remote is dropped.
///
/// `Rewrap` is transport-agnostic: the remote may be an RPC proxy or a
/// kernel one.
///
/// [`purge_dead`]: Self::purge_dead
pub struct Rewrap<I: FromIBinder + ?Sized> {
    // Boxed rather than a fn pointer: `BnFoo::new_binder` is a generic fn
    // item, and coercing one to a pointer leans on inference at every call
    // site. A closure — `Rewrap::new(|p| BnFoo::new_binder(p))` — always
    // resolves.
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
    /// Dropping the entry lets the recipient die, which makes the link inert.
    _reaper: Arc<Reaper>,
}

impl Entry {
    /// The live `(remote, local)` pair, or `None` if either half is gone.
    fn live(&self) -> Option<(SIBinder, SIBinder)> {
        Some((self.remote.upgrade().ok()?, self.local.upgrade().ok()?))
    }
}

/// Removes one entry when its remote dies. Holds a `Weak` to the table so a
/// live death link cannot keep the gateway's table alive.
struct Reaper {
    table: std::sync::Weak<Mutex<HashMap<usize, Entry>>>,
    key: usize,
}

impl DeathRecipient for Reaper {
    fn binder_died(&self, _who: &WIBinder) {
        let Some(entries) = self.table.upgrade() else {
            return;
        };
        let mut entries = entries.lock().expect("rewrap table poisoned");
        // The key is an address, and an address gets reused. Only drop the
        // entry if it is still the dead one — otherwise this notification
        // would evict a live wrapper that happens to sit at the same
        // address, silently changing the identity the upstream sees.
        if entries.get(&self.key).is_some_and(|e| e.live().is_none()) {
            entries.remove(&self.key);
        }
    }
}

impl<I: FromIBinder + ?Sized> Rewrap<I> {
    /// Build a table whose wrappers are made by `make_local`.
    ///
    /// `make_local` is normally `|p| BnFoo::new_binder(p)` — the generated
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
        // `link_to_death` reaches the driver / the RPC session. Holding the
        // table lock across either invites a deadlock, and a re-entrant
        // `wrap` from inside `make_local` would deadlock outright
        // (`std::sync::Mutex` is not reentrant).
        let local = (self.make_local)(remote.clone());
        let reaper = Arc::new(Reaper {
            table: Arc::downgrade(&self.entries),
            key,
        });
        // Best effort: a local binder has no death to report
        // (`InvalidOperation`), and a proxy whose peer is already gone
        // returns `DeadObject`. Either way the sweep below still collects
        // the entry.
        let _ = remote_binder.link_to_death_arc(&reaper);

        let mut entries = self.entries.lock().expect("rewrap table poisoned");
        // Another thread may have raced us to the same remote. Its wrapper
        // is as good as ours and may already be upstream, so it wins; ours
        // is dropped, taking its death link with it.
        if let Some((r, l)) = entries.get(&key).and_then(Entry::live) {
            if r == remote_binder {
                if let Ok(existing) = <I as FromIBinder>::try_from(l) {
                    return existing;
                }
            }
        }
        // Swept on the miss path only: minting a binder and a death link
        // already costs far more than scanning a table this size, and it
        // bounds the table without a timer.
        entries.retain(|_, e| e.live().is_some());
        entries.insert(
            key,
            Entry {
                remote: SIBinder::downgrade(&remote_binder),
                local: SIBinder::downgrade(&local.as_binder()),
                _reaper: reaper,
            },
        );
        local
    }

    /// Drop every entry whose remote or wrapper is gone, and report how many
    /// went. `wrap` sweeps too; this is for a gateway that wants to reclaim
    /// on its own schedule, and for tests.
    pub fn purge_dead(&self) -> usize {
        let mut entries = self.entries.lock().expect("rewrap table poisoned");
        let before = entries.len();
        entries.retain(|_, e| e.live().is_some());
        before - entries.len()
    }

    /// How many pairs the table currently holds, dead ones included (see
    /// [`purge_dead`](Self::purge_dead)).
    pub fn len(&self) -> usize {
        self.entries.lock().expect("rewrap table poisoned").len()
    }

    /// Whether the table holds no pairs at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lookup(&self, key: usize, remote_binder: &SIBinder) -> Option<Strong<I>> {
        let entries = self.entries.lock().expect("rewrap table poisoned");
        let (r, l) = entries.get(&key).and_then(Entry::live)?;
        // Confirm identity: the address may have been reused by an unrelated
        // binder since the entry was made.
        (&r == remote_binder)
            .then(|| <I as FromIBinder>::try_from(l).ok())
            .flatten()
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

/// The identity key: the address of the binder's `Arc` allocation, thinned
/// so two trait-object vtables for the same allocation cannot disagree.
///
/// Stable for the binder's lifetime — both stacks dedup their proxies
/// (`ProcessState::handle_to_proxy` for kernel, `RpcState::remote_proxies`
/// for RPC), so the same remote resolves to the same `Arc` while any strong
/// reference to it is alive. It is only a *hint*: `wrap` confirms with the
/// stored weak reference before trusting a hit.
fn key_of(binder: &SIBinder) -> usize {
    Arc::as_ptr(binder.as_arc()) as *const () as usize
}
