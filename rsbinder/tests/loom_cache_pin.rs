// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Loom re-implementation (not the production code) of the proxy cache's kernel ref protocol.
//!
//! This file is **gated on `cfg(loom)`** and is empty in normal builds.
//! Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p rsbinder --test loom_cache_pin --release
//! ```

#![cfg(loom)]

use loom::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use loom::sync::{Arc, Mutex, RwLock};
use std::collections::HashMap;

/// Mock `binder_ref` table: handle → (strong, weak).
#[derive(Default)]
struct MockKernel {
    refs: Mutex<HashMap<u32, (u32, u32)>>,
    bc_acquire_count: AtomicU32,
    bc_release_count: AtomicU32,
    bc_increfs_count: AtomicU32,
    bc_decrefs_count: AtomicU32,
    /// `BC_ACQUIRE` on a `(0, 0)` slot, or a `BC_RELEASE`/`BC_DECREFS` underflow.
    violations: AtomicU32,
}

impl MockKernel {
    fn bc_acquire(&self, h: u32) {
        self.bc_acquire_count.fetch_add(1, Ordering::Relaxed);
        let mut refs = self.refs.lock().unwrap();
        let entry = refs.entry(h).or_insert((0, 0));
        if *entry == (0, 0) {
            self.violations.fetch_add(1, Ordering::Relaxed);
        }
        entry.0 += 1;
    }

    fn bc_release(&self, h: u32) {
        self.bc_release_count.fetch_add(1, Ordering::Relaxed);
        let mut refs = self.refs.lock().unwrap();
        match refs.get_mut(&h) {
            Some(entry) if entry.0 > 0 => entry.0 -= 1,
            _ => {
                self.violations.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn bc_increfs(&self, h: u32) {
        self.bc_increfs_count.fetch_add(1, Ordering::Relaxed);
        self.refs.lock().unwrap().entry(h).or_insert((0, 0)).1 += 1;
    }

    fn bc_decrefs(&self, h: u32) {
        self.bc_decrefs_count.fetch_add(1, Ordering::Relaxed);
        let mut refs = self.refs.lock().unwrap();
        match refs.get_mut(&h) {
            Some(entry) if entry.1 > 0 => entry.1 -= 1,
            _ => {
                self.violations.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn ref_state(&self, h: u32) -> (u32, u32) {
        self.refs.lock().unwrap().get(&h).copied().unwrap_or((0, 0))
    }

    fn count(counter: &AtomicU32) -> u32 {
        counter.load(Ordering::Relaxed)
    }
}

/// An `Arc` strong count; loom 0.7 has no `Weak` to upgrade.
struct Refs(AtomicU32);

impl Refs {
    fn one() -> Self {
        Self(AtomicU32::new(1))
    }

    /// `Weak::upgrade`: increment unless already 0.
    fn try_upgrade(&self) -> bool {
        let mut n = self.0.load(Ordering::Relaxed);
        while n != 0 {
            match self
                .0
                .compare_exchange(n, n + 1, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(current) => n = current,
            }
        }
        false
    }

    /// `Arc::clone` from a reference the caller holds.
    fn add(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// `true` when this dropped the last reference.
    fn release(&self) -> bool {
        self.0.fetch_sub(1, Ordering::AcqRel) == 1
    }
}

struct Pin {
    handle: u32,
    generation: u32,
    refs: Refs,
    /// `HandlePin::counted`: set by the pin's first commit.
    counted: AtomicBool,
}

struct Proxy {
    pin: Arc<Pin>,
    refs: Refs,
}

/// `CacheEntry`; the `Arc`s only keep memory, liveness is each `refs`.
struct Entry {
    proxy: Arc<Proxy>,
    pin: Arc<Pin>,
}

struct Process {
    cache: RwLock<HashMap<u32, Entry>>,
    kernel: MockKernel,
    next_generation: AtomicU32,
    /// `proxy_count::PROXY_COUNT`: committed pins not yet released.
    proxy_count: AtomicU32,
}

/// A user `SIBinder` of a proxy.
struct StrongRef {
    process: Arc<Process>,
    proxy: Arc<Proxy>,
}

/// A pin reference: a proxy `WIBinder`, or a pin the slow path holds.
struct PinRef {
    process: Arc<Process>,
    pin: Arc<Pin>,
}

enum Plan {
    CaseA {
        pin: PinRef,
    },
    /// Only held, as production's `SlowPathReady::CaseB`.
    CaseB {
        _pin: PinRef,
    },
}

impl Process {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            cache: RwLock::new(HashMap::new()),
            kernel: MockKernel::default(),
            next_generation: AtomicU32::new(1),
            proxy_count: AtomicU32::new(0),
        })
    }

    /// `HandlePin::drop` when this was the last holder.
    fn release_pin(&self, pin: &Arc<Pin>) {
        if !pin.refs.release() {
            return;
        }
        if pin.counted.load(Ordering::Relaxed) {
            let before = self.proxy_count.fetch_sub(1, Ordering::Relaxed);
            assert!(before >= 1, "on_proxy_drop without its on_proxy_create");
        }
        {
            let mut cache = self.cache.write().unwrap();
            if cache
                .get(&pin.handle)
                .is_some_and(|e| Arc::ptr_eq(&e.pin, pin))
            {
                cache.remove(&pin.handle);
            }
        }
        self.kernel.bc_decrefs(pin.handle);
    }
}

/// Methods on the shared process (loom's `Arc` is not a method receiver).
trait SharedProcess {
    fn pin_ref(&self, pin: &Arc<Pin>) -> PinRef;
    fn strong(&self, proxy: &Arc<Proxy>) -> StrongRef;
    fn lookup(&self, handle: u32) -> StrongRef;
}

impl SharedProcess for Arc<Process> {
    /// Wraps a reference to `pin` the caller has already counted in `refs`.
    fn pin_ref(&self, pin: &Arc<Pin>) -> PinRef {
        PinRef {
            process: Arc::clone(self),
            pin: Arc::clone(pin),
        }
    }

    /// Wraps a reference to `proxy` the caller has already counted in `refs`.
    fn strong(&self, proxy: &Arc<Proxy>) -> StrongRef {
        StrongRef {
            process: Arc::clone(self),
            proxy: Arc::clone(proxy),
        }
    }

    /// `strong_proxy_for_handle_stability`.
    fn lookup(&self, handle: u32) -> StrongRef {
        {
            let cache = self.cache.read().unwrap();
            if let Some(e) = cache.get(&handle) {
                if e.proxy.refs.try_upgrade() {
                    return self.strong(&e.proxy);
                }
            }
        }
        loop {
            // P1. A plan's pin is moved out, never dropped under the lock.
            let plan = {
                let cache = self.cache.write().unwrap();
                match cache.get(&handle) {
                    Some(e) if e.proxy.refs.try_upgrade() => return self.strong(&e.proxy),
                    Some(e) if e.pin.refs.try_upgrade() => Plan::CaseB {
                        _pin: self.pin_ref(&e.pin),
                    },
                    _ => {
                        self.kernel.bc_increfs(handle);
                        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
                        let pin = Arc::new(Pin {
                            handle,
                            generation,
                            refs: Refs::one(),
                            counted: AtomicBool::new(false),
                        });
                        Plan::CaseA {
                            pin: self.pin_ref(&pin),
                        }
                    }
                }
            };
            // P2 holds no lock; P3 below. `plan` and `entry_pin` drop after the guard.
            let entry_pin: Option<PinRef>;
            let mut cache = self.cache.write().unwrap();
            if let Some(e) = cache.get(&handle) {
                if e.proxy.refs.try_upgrade() {
                    return self.strong(&e.proxy);
                }
            }
            entry_pin = match cache.get(&handle) {
                Some(e) if e.pin.refs.try_upgrade() => Some(self.pin_ref(&e.pin)),
                _ => None,
            };
            let pin = match (&entry_pin, &plan) {
                (Some(entry), _) => Arc::clone(&entry.pin),
                (None, Plan::CaseA { pin: ours }) => Arc::clone(&ours.pin),
                (None, Plan::CaseB { .. }) => {
                    drop(cache);
                    continue;
                }
            };
            self.kernel.bc_acquire(handle);
            // `HandlePin::count_once`: the first commit counts, a case (b) revival does not.
            if !pin.counted.swap(true, Ordering::Relaxed) {
                self.proxy_count.fetch_add(1, Ordering::Relaxed);
            }
            pin.refs.add();
            let proxy = Arc::new(Proxy {
                pin: Arc::clone(&pin),
                refs: Refs::one(),
            });
            cache.insert(
                handle,
                Entry {
                    proxy: Arc::clone(&proxy),
                    pin,
                },
            );
            drop(cache);
            return self.strong(&proxy);
        }
    }
}

impl StrongRef {
    fn generation(&self) -> u32 {
        self.proxy.pin.generation
    }

    /// `SIBinder::downgrade`: the `WIBinder` holds the pin.
    fn downgrade(&self) -> PinRef {
        self.proxy.pin.refs.add();
        self.process.pin_ref(&self.proxy.pin)
    }
}

impl Drop for StrongRef {
    /// `ProxyHandle::drop` (`BC_RELEASE`), then its pin field's drop.
    fn drop(&mut self) {
        if self.proxy.refs.release() {
            self.process.kernel.bc_release(self.proxy.pin.handle);
            self.process.release_pin(&self.proxy.pin);
        }
    }
}

impl PinRef {
    /// `WIBinder::upgrade`'s `live_proxy_for_pin`; its first step is a plain `Weak::upgrade`.
    fn upgrade(&self) -> Option<StrongRef> {
        let cache = self.process.cache.read().unwrap();
        let entry = cache
            .get(&self.pin.handle)
            .filter(|e| Arc::ptr_eq(&e.pin, &self.pin))?;
        // Upgraded only on our pin and moved out, so no proxy drops under the read lock.
        if entry.proxy.refs.try_upgrade() {
            Some(self.process.strong(&entry.proxy))
        } else {
            None
        }
    }
}

impl Drop for PinRef {
    fn drop(&mut self) {
        self.process.release_pin(&self.pin);
    }
}

/// `loom::model` with a preemption bound of 4 unless `LOOM_MAX_PREEMPTIONS` sets one.
fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound.get_or_insert(4);
    builder.check(f);
}

/// Settled-state checks once every proxy and `WIBinder` is gone.
fn assert_released(process: &Process, handle: u32) {
    let kernel = &process.kernel;
    assert_eq!(MockKernel::count(&kernel.violations), 0, "I1 violation");
    assert_eq!(kernel.ref_state(handle), (0, 0), "the last holder releases");
    assert!(
        process.cache.read().unwrap().is_empty(),
        "the last holder's drop removes the entry"
    );
    assert_eq!(
        MockKernel::count(&kernel.bc_increfs_count),
        MockKernel::count(&kernel.bc_decrefs_count),
        "every pin is released once"
    );
    assert_eq!(
        MockKernel::count(&kernel.bc_acquire_count),
        MockKernel::count(&kernel.bc_release_count),
        "every proxy is released once"
    );
    assert_eq!(
        process.proxy_count.load(Ordering::Relaxed),
        0,
        "every counted pin posts its drop"
    );
}

/// Two threads each look up then drop, racing each other's last proxy and pin drops.
#[test]
fn concurrent_lookup_and_drop_releases_on_last_holder() {
    const HANDLE: u32 = 42;

    model(|| {
        let process = Process::new();
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let process = Arc::clone(&process);
                loom::thread::spawn(move || drop(process.lookup(HANDLE)))
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }

        assert_released(&process, HANDLE);
        let increfs = MockKernel::count(&process.kernel.bc_increfs_count);
        // A second pin: both P1s ran before a commit (the spare is released), or after a release.
        assert!((1..=2).contains(&increfs), "got increfs={increfs}");
        // One BC_ACQUIRE when the second lookup shared the first's live proxy (case (c)).
        let acquire = MockKernel::count(&process.kernel.bc_acquire_count);
        assert!((1..=2).contains(&acquire), "got acquire={acquire}");
    });
}

/// A `WIBinder` keeps the pin for case (b); with none left, the next lookup pins afresh.
#[test]
fn weak_ref_keeps_the_pin_for_case_b() {
    const HANDLE: u32 = 7;

    model(|| {
        let process = Process::new();
        let kernel = &process.kernel;

        // No holder left: the last drop releases the pin and the entry.
        let first = process.lookup(HANDLE);
        let first_generation = first.generation();
        drop(first);
        assert_eq!(kernel.ref_state(HANDLE), (0, 0));
        assert_eq!(MockKernel::count(&kernel.bc_decrefs_count), 1);

        // Case (a) again: a fresh BC_INCREFS under a new generation.
        let second = process.lookup(HANDLE);
        let kept_generation = second.generation();
        assert_ne!(kept_generation, first_generation);
        assert_eq!(MockKernel::count(&kernel.bc_increfs_count), 2);

        // A held WIBinder keeps the pin and the entry: case (b), same generation.
        let weak = second.downgrade();
        drop(second);
        assert_eq!(kernel.ref_state(HANDLE), (0, 1));
        assert_eq!(
            process.proxy_count.load(Ordering::Relaxed),
            1,
            "the WIBinder's pin counts"
        );
        let third = process.lookup(HANDLE);
        assert_eq!(third.generation(), kept_generation);
        assert_eq!(MockKernel::count(&kernel.bc_increfs_count), 2);
        assert_eq!(
            process.proxy_count.load(Ordering::Relaxed),
            1,
            "revival counts nothing"
        );
        drop(third);

        // A lookup racing the WIBinder's drop: case (b) if it upgrades the pin first.
        let racer = {
            let process = Arc::clone(&process);
            loom::thread::spawn(move || process.lookup(HANDLE).generation())
        };
        drop(weak);
        let raced_generation = racer.join().unwrap();

        assert_released(&process, HANDLE);
        let increfs = MockKernel::count(&kernel.bc_increfs_count);
        if raced_generation == kept_generation {
            assert_eq!(increfs, 2, "case (b) reuses the live pin");
        } else {
            assert_eq!(increfs, 3, "case (a) after the pin's release pins afresh");
        }
    });
}

/// A `WIBinder`'s upgrade races the lookup that revives its entry and that proxy's last drop.
#[test]
fn weak_upgrade_finds_the_revived_proxy() {
    const HANDLE: u32 = 9;

    model(|| {
        let process = Process::new();

        let first = process.lookup(HANDLE);
        let kept_generation = first.generation();
        let weak = first.downgrade();
        drop(first);
        assert!(weak.upgrade().is_none(), "no proxy is left to upgrade to");

        // The reviver drops its proxy when it returns, racing the upgrade below.
        let reviver = {
            let process = Arc::clone(&process);
            loom::thread::spawn(move || process.lookup(HANDLE).generation())
        };
        let upgraded = weak.upgrade().map(|strong| strong.generation());
        let revived_generation = reviver.join().unwrap();

        assert_eq!(
            revived_generation, kept_generation,
            "the WIBinder keeps the pin, so the lookup is case (b)"
        );
        if let Some(generation) = upgraded {
            assert_eq!(generation, kept_generation, "only a proxy on its own pin");
        }
        drop(weak);
        assert_released(&process, HANDLE);
    });
}
