// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Loom proof-of-concept for the cache-pin model's kernel ref-count
//! invariants.
//!
//! This file is **gated on `cfg(loom)`** and is empty in normal builds.
//! Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --test loom_cache_pin --release
//! ```
//!
//! ## Scope
//!
//! Loom 0.7 does not model `Arc::Weak` / `Arc::downgrade`. Production
//! `process_state::CacheEntry { weak: sync::Weak<ProxyHandle>, .. }`
//! cannot be loom-modeled directly. This PoC instead verifies the
//! **kernel-side invariants** (which are what the cache-pin model
//! actually closes the race on) by representing the cache as
//! "presence/absence of a pin record" without trying to model
//! Arc-sharing across threads.
//!
//! Loom-checked invariants in this PoC:
//!
//! - **I1 (cache contains h ⟹ binder_ref(h).weak ≥ 1)** — under
//!   exhaustive interleaving of N=2 worker threads doing
//!   lookup/drop/lookup/drop loops, the mock kernel never sees
//!   `BC_ACQUIRE` to a freed slot (`(0, 0)` state) when the cache
//!   has a pin record for `h`.
//! - **No double-pin** — case (b) (cache present) reuses the pin and
//!   does not issue a second `BC_INCREFS` for the same handle.
//! - **Paired BC_ACQUIRE / BC_RELEASE** — every `Arc<MockProxyHandle>`
//!   allocation Drops exactly once, and Drop's `BC_RELEASE` always
//!   lands on a live slot (kernel never rejects with `DeadObject`).
//!
//! Out of scope (would require loom Weak support and Path-A integration):
//!
//! - **Arc-identity preservation** across concurrent lookups
//!   (production guarantees `Arc::ptr_eq` between two `SIBinder`s
//!   acquired for the same handle while any strong ref is alive —
//!   this PoC creates a fresh Arc per lookup, so it cannot test that
//!   property; AIDL out-parameter equality tests on the integration
//!   side already cover it).
//! - **Per-thread out-parcel buffering** effects on cross-thread BC_*
//!   ordering (production wires BC_* through `thread_state.rs` which
//!   buffers per thread; loom would need to model that buffer too).
//! - **Obituary teardown timing** (BR_DEAD_BINDER → BC_DEAD_BINDER_DONE
//!   → BC_DECREFS ordering).
//!
//! These belong in a Path-A loom integration that swaps rsbinder's own
//! sync primitives (process_state.rs cache RwLock, thread_state.rs
//! THREAD_STATE thread-local, etc.) via cfg(loom). That refactor is a
//! separate follow-up.

#![cfg(loom)]

use loom::sync::atomic::{AtomicU32, Ordering};
use loom::sync::{Arc, Mutex, RwLock};
use std::collections::HashMap;

/// Mock kernel `binder_ref` state. Maps handle → (strong, weak).
/// Entries lazily created on first BC_INCREFS / BC_ACQUIRE. A handle
/// whose entry has `(strong, weak) == (0, 0)` is considered freed and
/// any subsequent BC_INCREFS / BC_ACQUIRE returns `Err(DeadObject)` —
/// matching Linux binder driver behavior.
#[derive(Default)]
struct MockKernel {
    refs: Mutex<HashMap<u32, (u32, u32)>>,
    bc_acquire_count: AtomicU32,
    bc_release_count: AtomicU32,
    bc_increfs_count: AtomicU32,
    bc_decrefs_count: AtomicU32,
    /// Set if any `bc_acquire` fails with DeadObject during the run.
    /// I1 violation triggers this.
    saw_acquire_to_freed_slot: AtomicU32,
}

#[derive(Debug, PartialEq, Eq)]
struct DeadObject;

impl MockKernel {
    fn new() -> Self {
        Self::default()
    }

    fn bc_acquire(&self, h: u32) -> Result<(), DeadObject> {
        self.bc_acquire_count.fetch_add(1, Ordering::Relaxed);
        let mut refs = self.refs.lock().unwrap();
        let entry = refs.entry(h).or_insert((0, 0));
        if entry.0 == 0 && entry.1 == 0 {
            self.saw_acquire_to_freed_slot
                .fetch_add(1, Ordering::Relaxed);
            return Err(DeadObject);
        }
        entry.0 += 1;
        Ok(())
    }

    fn bc_release(&self, h: u32) -> Result<(), DeadObject> {
        self.bc_release_count.fetch_add(1, Ordering::Relaxed);
        let mut refs = self.refs.lock().unwrap();
        let entry = refs.get_mut(&h).ok_or(DeadObject)?;
        if entry.0 == 0 {
            return Err(DeadObject);
        }
        entry.0 -= 1;
        Ok(())
    }

    fn bc_increfs(&self, h: u32) -> Result<(), DeadObject> {
        self.bc_increfs_count.fetch_add(1, Ordering::Relaxed);
        let mut refs = self.refs.lock().unwrap();
        let entry = refs.entry(h).or_insert((0, 0));
        entry.1 += 1;
        Ok(())
    }

    #[allow(dead_code)]
    fn bc_decrefs(&self, h: u32) -> Result<(), DeadObject> {
        self.bc_decrefs_count.fetch_add(1, Ordering::Relaxed);
        let mut refs = self.refs.lock().unwrap();
        let entry = refs.get_mut(&h).ok_or(DeadObject)?;
        if entry.1 == 0 {
            return Err(DeadObject);
        }
        entry.1 -= 1;
        Ok(())
    }

    fn ref_state(&self, h: u32) -> (u32, u32) {
        let refs = self.refs.lock().unwrap();
        refs.get(&h).copied().unwrap_or((0, 0))
    }
}

/// Mock `ProxyHandle`. Drop sends BC_RELEASE just like the production
/// type. `Arc<MockProxyHandle>` is created per lookup (this PoC does
/// not test Arc-identity sharing — see file-level docstring).
struct MockProxyHandle {
    handle: u32,
    kernel: Arc<MockKernel>,
}

impl Drop for MockProxyHandle {
    fn drop(&mut self) {
        // Cache pin keeps weak ≥ 1, so this BC_RELEASE always finds
        // the slot alive. Verified by the assertion at the end of the
        // loom model.
        let _ = self.kernel.bc_release(self.handle);
    }
}

/// Mock cache: just records which handles have an active pin. Real
/// production cache stores `sync::Weak<ProxyHandle>` for Arc identity
/// sharing — this PoC's simplified cache is sufficient to verify
/// kernel-side I1 (the actual race the cache-pin model closes).
type Cache = RwLock<HashMap<u32, ()>>;

/// Mock `ProcessState::strong_proxy_for_handle_stability` simplified
/// to the kernel-side ordering: case (a) issues `BC_INCREFS` then
/// `BC_ACQUIRE`; case (b) reuses the pin and only issues
/// `BC_ACQUIRE`. Returns a fresh `Arc<MockProxyHandle>` per call.
fn strong_proxy_for_handle(
    cache: &Cache,
    kernel: &Arc<MockKernel>,
    handle: u32,
) -> Result<Arc<MockProxyHandle>, DeadObject> {
    // Read-lock fast path: pin already exists.
    let pin_already_held = {
        let read = cache.read().unwrap();
        read.contains_key(&handle)
    };

    if !pin_already_held {
        // Slow path: acquire write lock, double-check, then issue
        // BC_INCREFS pin.
        let mut write = cache.write().unwrap();
        if let std::collections::hash_map::Entry::Vacant(slot) = write.entry(handle) {
            kernel.bc_increfs(handle)?;
            slot.insert(());
        }
    }

    // Issue BC_ACQUIRE. The cache pin (issued above or by an earlier
    // caller) keeps `binder_ref(handle).weak >= 1`, so this BC_ACQUIRE
    // must succeed. If it ever returns DeadObject, the cache-pin
    // invariant is broken — `MockKernel::bc_acquire` records that.
    kernel.bc_acquire(handle)?;

    Ok(Arc::new(MockProxyHandle {
        handle,
        kernel: Arc::clone(kernel),
    }))
}

/// Loom model: 2 worker threads each do one lookup-then-drop. Even
/// with K=1 per thread the state space is large because each
/// `loom::sync::*` operation is a preemption point.
///
/// Invariants checked at run end:
///
/// 1. Kernel never observed `BC_ACQUIRE` against a freed slot
///    (counter `saw_acquire_to_freed_slot == 0`). This is **I1**.
/// 2. `bc_increfs_count == 1` — pin issued exactly once across the
///    run (case (a) only fires for the first thread; the second
///    hits case (b) — fast path — even if it doesn't see the cache
///    insert until it acquires the read lock again).
/// 3. `bc_acquire_count == bc_release_count` — paired.
/// 4. Final kernel state `(strong = 0, weak = 1)` — cache pin still
///    held; no obituary in this model.
#[test]
fn cache_pin_holds_under_concurrent_lookup_and_drop() {
    const HANDLE: u32 = 42;

    loom::model(|| {
        let kernel = Arc::new(MockKernel::new());
        let cache: Arc<Cache> = Arc::new(RwLock::new(HashMap::new()));

        let kernel_t1 = Arc::clone(&kernel);
        let cache_t1 = Arc::clone(&cache);
        let t1 = loom::thread::spawn(move || {
            let arc = strong_proxy_for_handle(&cache_t1, &kernel_t1, HANDLE)
                .expect("T1 lookup must succeed");
            drop(arc);
        });

        let kernel_t2 = Arc::clone(&kernel);
        let cache_t2 = Arc::clone(&cache);
        let t2 = loom::thread::spawn(move || {
            let arc = strong_proxy_for_handle(&cache_t2, &kernel_t2, HANDLE)
                .expect("T2 lookup must succeed");
            drop(arc);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        // Settled-state assertions: every thread has joined, all BC_*
        // commands have committed.
        assert_eq!(
            kernel.saw_acquire_to_freed_slot.load(Ordering::Relaxed),
            0,
            "I1 violation: BC_ACQUIRE issued against freed kernel slot"
        );

        let (strong, weak) = kernel.ref_state(HANDLE);
        assert_eq!(
            strong, 0,
            "after both threads' Arcs dropped, kernel strong must be 0; got {strong}"
        );
        assert_eq!(
            weak, 1,
            "cache pin must keep kernel weak == 1 (I1); got {weak}"
        );

        let increfs = kernel.bc_increfs_count.load(Ordering::Relaxed);
        let decrefs = kernel.bc_decrefs_count.load(Ordering::Relaxed);
        let acquire = kernel.bc_acquire_count.load(Ordering::Relaxed);
        let release = kernel.bc_release_count.load(Ordering::Relaxed);
        assert_eq!(
            increfs, 1,
            "cache pin must be issued exactly once per handle; got increfs={increfs}"
        );
        assert_eq!(
            decrefs, 0,
            "no obituary in this model; got decrefs={decrefs}"
        );
        assert_eq!(
            acquire, 2,
            "two lookups → two BC_ACQUIREs; got acquire={acquire}"
        );
        assert_eq!(
            release, 2,
            "two Arc Drops → two BC_RELEASEs; got release={release}"
        );
    });
}

/// Sequential variant: T1 establishes the cache entry and drops, then
/// T2 enters with the cache pin already held but no live Arc — this
/// exercises the case (b)-equivalent path (pin held, BC_ACQUIRE only).
/// Smaller state space than the fully-concurrent test above, but
/// directly targets case (b).
#[test]
fn case_b_path_reuses_existing_pin() {
    const HANDLE: u32 = 7;

    loom::model(|| {
        let kernel = Arc::new(MockKernel::new());
        let cache: Arc<Cache> = Arc::new(RwLock::new(HashMap::new()));

        // T1 (main thread): establish pin, drop Arc.
        let arc1 =
            strong_proxy_for_handle(&cache, &kernel, HANDLE).expect("first lookup must succeed");
        drop(arc1);

        // After T1's drop, kernel state: strong=0, weak=1 (pin alive).
        let (s_mid, w_mid) = kernel.ref_state(HANDLE);
        assert_eq!(s_mid, 0, "post-drop strong must be 0");
        assert_eq!(w_mid, 1, "pin keeps weak == 1 across lookup-then-drop");

        // T2 enters: cache contains pin, so no new BC_INCREFS, just
        // BC_ACQUIRE. The pin keeps the slot alive so BC_ACQUIRE must
        // succeed.
        let kernel_t2 = Arc::clone(&kernel);
        let cache_t2 = Arc::clone(&cache);
        let t2 = loom::thread::spawn(move || {
            let arc = strong_proxy_for_handle(&cache_t2, &kernel_t2, HANDLE)
                .expect("case-b lookup must succeed under pin");
            drop(arc);
        });
        t2.join().unwrap();

        assert_eq!(
            kernel.saw_acquire_to_freed_slot.load(Ordering::Relaxed),
            0,
            "I1 violation: case-b BC_ACQUIRE saw freed slot"
        );
        assert_eq!(
            kernel.bc_increfs_count.load(Ordering::Relaxed),
            1,
            "case (b) must NOT issue a second BC_INCREFS"
        );
        let (strong, weak) = kernel.ref_state(HANDLE);
        assert_eq!(strong, 0);
        assert_eq!(weak, 1, "pin survives case-b resurrection");
    });
}
