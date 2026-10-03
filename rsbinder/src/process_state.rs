// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Process-wide kernel binder state: the driver fd and receive mapping,
//! the handle → proxy cache, the published-native sidecar table, and the
//! binder thread pool.
//!
//! # Receive mapping
//!
//! The mapping is `PROT_READ`, owned for the whole `ProcessState` lifetime,
//! and the only access *through `MemoryMap::ptr`* is the single
//! `munmap(ptr, size)` in `ProcessState::drop` — which is why `MemoryMap` is
//! `Send + Sync`. The region is the kernel-delivered transaction-buffer area
//! and *is* read as Rust data elsewhere (`thread_state` reads inbound
//! `BR_TRANSACTION`/`BR_REPLY` buffers out of it), but that access goes
//! through the kernel pointers in the transaction, not `ptr`, and is
//! synchronized by the driver's buffer-lifetime protocol (a buffer stays
//! valid until `BC_FREE_BUFFER`). The `secctx` of a `BR_TRANSACTION_SEC_CTX`
//! is not read that way: the kernel does not tell the receiver how long it
//! is, so `thread_state` copies it with `process_vm_readv` on its own pid,
//! never with a load, no further than the end of `mapped_range` (which reads
//! `ptr` only as an address). Because the mapping lives for the singleton's
//! whole lifetime (an init-race loser never serviced a transaction), no such
//! read is outstanding at drop.
//!
//! The size floor is one page: the granularity `mmap(2)` works in and the
//! smallest mapping the driver serves a buffer from (measured: a 4096-byte
//! mapping carries a ~3.9 KB call). The floor also keeps a
//! `default_mmap_size()` that degenerated to 0 out of `mmap(len = 0)`. The
//! ceiling is the driver's silent `SZ_4M` clamp.
//!
//! `normalized_mmap_size` returns the size rounded up to a page, as `mmap(2)`
//! rounds it. That rounded value is what `mmap_size` reports and what the
//! entry layer compares a later request against; carrying the raw request
//! would make the same size compare unequal to itself.
//!
//! # Proxy cache slow path
//!
//! `strong_proxy_for_handle_stability` misses its read-lock fast path, then
//! runs three phases so no IPC happens with `handle_to_proxy` locked:
//!
//! - **P1** — short write-lock window. Decides the sub-case and, for case
//!   (a), issues `BC_INCREFS` + `flush_commands` so the cache
//!   pin is live in the kernel before any IPC enters. The write lock is
//!   taken even though P1 only reads, so two concurrent slow paths cannot
//!   both observe "absent" and produce two case-(a) commits with distinct
//!   generations while the first lives; the second's P3 returns the first's proxy (re-check
//!   (c)) or, while a `WIBinder` holds the first's pin, revives on it. Pinning under the lock
//!   also means P3's `BC_ACQUIRE` never races a freed `binder_ref` slot. `flush_commands` inside
//!   the lock is sound because it is a write-only ioctl (`talk_with_driver(false)`,
//!   `read_size = 0`): no `BR_*` — `BR_DEAD_BINDER` included — is
//!   dispatched, and re-entrant `send_obituary_for_handle` paths only
//!   originate from `BR_DEAD_BINDER`.
//! - **P2** — lock released. IPC (`ping_binder` for handle 0 on sdk >= 30,
//!   `query_interface` for case (a)) runs without the lock, so a re-entrant
//!   `BR_DEAD_BINDER` → `send_obituary_for_handle` on the same thread can
//!   take it without deadlocking against `std::sync::RwLock`'s
//!   non-reentrant write lock.
//! - **P3** — write lock re-acquired. Re-checks case (c) and the cross-thread
//!   race, and commits the entry.
//!
//! Sub-cases decided in P1:
//!
//! - **(a)** entry absent, or its pin already in `Drop` (AOSP `attemptIncWeak` fails). P1 pins.
//! - **(b)** entry present with no live proxy but a live pin. P1 upgrades the pin and holds it
//!   to P3 for its `BC_ACQUIRE`; P2 skips `query_interface`
//!   (the descriptor is immutable for the `binder_ref` slot's lifetime), and
//!   P3 resurrects under the pin's generation (AOSP `force_set`).
//! - **(c)** another thread inserted or upgraded the entry between the
//!   fast-path miss and P1; the live entry is returned.
//!
//! Every exit that does not commit this thread's case-(a) pin drops it with
//! no lock held, and its `Drop` releases it (module doc "Proxy cache entry");
//! the slow path has no separate undo step.
//!
//! The P2 lock release matters because the catch-all arm of
//! `wait_for_response` dispatches `BR_DEAD_BINDER` to `execute_command`, which
//! calls `send_obituary_for_handle` and takes the same write lock.
//!
//! P3 covers a race in which another thread T2 ran a complete case (a) during this thread's P2
//! IPC and then dropped its `Arc` while a `WIBinder` kept its pin: the plan is `CaseA`, yet the
//! entry names T2's live pin, which P3 adopts, releasing this thread's pin. A `CaseB` whose
//! entry an obituary removed re-runs from P1 as case (a); its `query_interface` gets `DeadObject`.
//!
//! # Proxy cache entry
//!
//! A case-(a) `BC_INCREFS` is a `HandlePin` (AOSP `BpBinder`'s weak lifetime) held by the entry's
//! proxies and `WIBinder`s; its last drop expunges the entry, then `pin_ledger` sends `BC_DECREFS`
//! once no clear of the handle's death subscription is in flight.
//!
//! `generation` comes from the process-wide monotonic `next_generation`,
//! taken once per case-(a) pin (u64, so wrap-around is not a concern).
//! A proxy `WIBinder` records the generation seen at `SIBinder::downgrade`, so
//! its `PartialEq` identity `(handle, generation)` is stable across case-(b)
//! re-lookups (a fresh `Arc<ProxyHandle>` is a new allocation) and a recycled
//! handle id naming a different `binder_node` is distinguishable. Case (b)
//! keeps the pin's generation — same kernel slot, and a fresh wire-delivered
//! strong ref makes it transactable again; only case (a) allocates one.
//!
//! A case-(b) re-lookup through `strong_proxy_for_handle_stability` is driven
//! by a fresh wire delivery of the handle (servicemanager `checkService`, or an
//! incoming transaction carrying it), which gives the new `BC_ACQUIRE` a
//! kernel strong count it can transact on. `WIBinder::upgrade()` differs: it
//! is purely weak and never re-`BC_ACQUIRE`s a strong-0 handle. When its own
//! proxy is gone it returns the entry's live proxy, found by
//! `live_proxy_for_pin` only while the entry names the `WIBinder`'s pin — AOSP
//! `wp::promote()` after `getStrongProxyForHandle` revived the same `BpBinder`
//! with `force_set` (`ProcessState.cpp:398`). The read-lock fast path is not
//! reused: it does not check the pin, and after an obituary a case (a) puts a
//! new generation in the entry, which this `WIBinder` does not name. The pin is
//! compared before the upgrade, so no other pin's proxy is upgraded and then
//! dropped under the read lock, where its pin's `Drop` would deadlock on the
//! write lock.
//!
//! # Proxy counting
//!
//! `crate::proxy_count` counts pins, not `Arc<ProxyHandle>`s: AOSP counts a
//! `BpBinder` from `BpBinder::create` to `~BpBinder` (`BpBinder.cpp:180-232`,
//! `:796-830`), and a pin is that lifetime. A handle a `WIBinder` alone keeps
//! stays counted, and a case-(b) revival is not counted again.
//!
//! `HandlePin::count_once` runs at the pin's first commit, in P3 after the
//! `BC_ACQUIRE`, under P3's `CallbackDeferGuard` (`proxy_count` module doc
//! "Callback deferral"), and records the uid it charged in `counted`;
//! `HandlePin::drop` posts the matching `on_proxy_drop` from it, so the uid is
//! the one at creation (AOSP `mTrackedUid`) and toggling per-uid tracking
//! while the pin lives cannot desync the map. The uid is
//! `thread_state::get_calling_uid` (AOSP `IPCThreadState::getCallingUid()`):
//! the sender of the `BR_TRANSACTION` being handled, or this process's own
//! `getuid()` when none is on the stack. Counting at P1's pinning would charge
//! pins that are never committed — a P2 `query_interface` failure, or P3
//! adopting another thread's pin — so no proxy anyone received; AOSP builds
//! and returns its `BpBinder` under one lock and has no such pin. The cost is
//! that a pin is uncounted during its P2 IPC. A case (a) replacing a pin in
//! its `Drop` counts 2 until that `Drop` posts, as AOSP does while a new
//! `BpBinder` replaces one in its destructor.
//!
//! # Death links
//!
//! `death` holds one slot per handle listing the proxies whose recipient
//! list is non-empty (the `death` module doc has why). The first link
//! requests the registration, flushed under the lock; the last unlink, drop
//! or obituary queues the clear; a link while the clear is in flight defers
//! its request to `death_clear_done`. The lock nests outside `pin_ledger`,
//! like the freeze lock, and is never held across user code. Only the
//! registry decides when a clear goes out; `pin_ledger` counts it even when
//! its write fails, since the subscription may still be live.
//!
//! # Freeze notifications
//!
//! `freeze` holds one slot per handle (the `freeze` module doc has the
//! kernel rules and why the slot is per handle, not per proxy). Under its
//! lock a caller decides a transition and writes the command it implies.
//! `BC_REQUEST_FREEZE_NOTIFICATION` is also flushed under the lock: once the
//! slot reads `Registered`, another thread may write a clear, and the kernel
//! refuses a clear that reaches it before the request. A clear may stay
//! queued (a drop on a looper leaves it for that looper's next ioctl): a
//! request is deferred until its clear-done, which needs the clear in the
//! kernel first.
//! The lock nests outside `pin_ledger` and is never held across user code
//! or a cache lookup. `deliver_frozen` takes its targets from the slot's
//! entries, not from the cache (an obituary removes the cache entry while the
//! proxy may live on), upgrades them after releasing the lock, and drops
//! them after releasing it again, since a proxy's drop takes it.
//!
//! Support is read from the binderfs `features/freeze_notification` file:
//! first next to the opened driver node (Linux mounts binderfs anywhere;
//! resolved at init, so a relative `driver_name` ignores a later cwd change),
//! then AOSP's fixed `/dev/binderfs/features/`
//! (`ProcessState.cpp:537-550`). The first file found decides; none means
//! unsupported, and the API returns `InvalidOperation` without a command.
//!
//! # Published natives
//!
//! `flat_binder_object.binder` carries a process-monotonic u64 id (from
//! `next_native_id`), not a pointer into the object. The kernel echoes the id
//! in `BR_INCREFS` / `BR_ACQUIRE` / `BR_RELEASE` / `BR_DECREFS` /
//! `BR_ATTEMPT_ACQUIRE` / `BR_TRANSACTION` (`target.ptr`), and a round-trip
//! `BINDER_TYPE_BINDER` read carries it too; `published_natives` resolves it
//! to the live `Arc` through `binder_pin.as_arc()`. A pointer encoding would
//! let a weak-ref handler (`BR_DECREFS`) run after `Inner<T>` had been dropped
//! (a use-after-free); Android's two-allocation `weakref_type*` + `BBinder*`
//! design addresses the same shape.
//!
//! A `PublishedNative` is created on the first `From<&SIBinder>`
//! (`BINDER_TYPE_BINDER`), held while parcel-side (`publish_count`) or
//! kernel-side (`kernel_refs`) refs are outstanding, and removed when both are
//! zero and `pending_reservations` is zero. While it exists, `binder_pin` keeps
//! `Inner<T>` alive and holds `RefCounter.strong` / `RefCounter.weak` at the
//! "alive" level (>= 1) so `attempt_inc_*` succeeds: `SIBinder::from_arc`'s
//! `inc_strong` sets that at creation and `SIBinder::Drop`'s
//! `dec_strong(None)` releases it at removal. `publish_count` follows
//! `FlatBinderObject::acquire` / `release`, called from
//! `Parcel::write_object`, `Parcel::append_from` and `Parcel::release_objects`.
//! `kernel_refs` rises on `BR_INCREFS` / `BR_ACQUIRE` and falls on
//! `BR_RELEASE` / `BR_DECREFS`, deferred through `pending_*_derefs` and
//! processed FIFO.
//!
//! `pending_reservations` counts `publish_native` dedup hits whose matching
//! `acquire` (`incref_publish`) has not landed yet. A dedup returns an existing
//! id, but only the `acquire` right after it bumps `publish_count`; without the
//! reservation a concurrent `decref_publish` / `deref_native_kernel` that
//! drives the counters to zero in that gap would remove the entry, and the
//! pending `acquire` would ship an id the kernel can no longer resolve. It is
//! bumped under the write lock on dedup and consumed by the next
//! `incref_publish`. The reservation is keyed by id, not by the specific
//! dedup, so an `append_from`-clone `acquire` on the same id can consume it.
//! That closes the common window (a `decref`/`deref` racing a dedup) but not
//! the one where a clone's whole `acquire`..`release` lifetime nests inside a
//! single dedup's `publish_native`..`acquire` gap; that gap is two adjacent
//! statements with no blocking call. Closing it fully would need an "is-dedup"
//! flag threaded into `FlatBinderObject::acquire`, a `binder_object.rs`
//! protocol change.
//!
//! `publish_native` takes one write lock for dedup and insert and dedups by
//! `Arc::ptr_eq` against `binder_pin`, so publishing one `Arc` twice returns
//! the same id, as Android allocates one `binder_node` per `weakref_type*`
//! however often it is sent. A dedup hit only bumps `pending_reservations`. A
//! fresh insert drives `RefCounter.strong` 0→1 through `SIBinder::from_arc`
//! and `RefCounter.weak` 0→1 through an explicit `inc_weak`; that table-held
//! +1 keeps both counts above zero, so user-side increments and decrements
//! never reach the count→0 closure path. `publish_count` starts at 0 and the
//! `Parcel::write_object` → `FlatBinderObject::acquire` right after brings
//! it to 1. The only leak path is a `Parcel::write_aligned_data` failure between
//! `From<&SIBinder>` returning and that `acquire()`: a panic (typically OOM),
//! `Err(BadValue)` when the write would end past `i32::MAX`, or
//! `Err(PermissionDenied)` when it would overlap a recorded object.
//!
//! `incref_publish` returns `false` for an unknown id. Every `acquire` follows
//! a `From<&SIBinder>` that just inserted the entry, or is an `append_from`
//! clone of a buffer that already holds one, so the caller treats `false` as
//! a bug: `debug_assert!` in debug builds, `log::error!` and skip in release.
//! `ref_native_kernel` returns `None` for an unknown id — a kernel invariant
//! violation for `BR_INCREFS` / `BR_ACQUIRE`, an expected race for
//! `BR_ATTEMPT_ACQUIRE`.
//!
//! Removal is two-phase. `decref_publish` / `deref_native_kernel` mutate the
//! counters under the write lock and release it; `remove_entry_if_zero` then
//! re-takes the lock, re-checks that `publish_count`, `kernel_refs` and
//! `pending_reservations` are all zero, removes the entry, and drops
//! `binder_pin` after releasing the lock. The drop calls `dec_strong(None)`,
//! which may run user destructor code (`Inner<T>::drop`) that calls back into
//! `ProcessState`, so holding the write lock there would deadlock. If a
//! concurrent `BR_INCREFS` / `From<&SIBinder>` raised a counter between the
//! phases, the re-check aborts the removal.
//!
//! # Obituary teardown
//!
//! Phase 1, `send_obituary_for_handle`, removes the cache entry under the
//! write lock (an rsbinder addition: a later delivery then gets `DeadObject` as case (a)) and
//! notifies death recipients; the pin stays with its holders. Phase 2, `finish_obituary`, flushes
//! the `BC_DEAD_BINDER_DONE` that `execute_command`'s `BR_DEAD_BINDER` arm queued after phase 1.
//!
//! Phase 1 can run on the thread that issued the originating transaction,
//! including one inside `strong_proxy_for_handle_stability`; its
//! `handle_to_proxy.write()` would deadlock if the slow path held the lock
//! across IPC, which is why P2 runs unlocked. It must be called with no
//! `THREAD_STATE` or `BINDER_DEREFS` borrow held: `send_obituary` runs user
//! `DeathRecipient::binder_died` callbacks, which can issue nested binder
//! calls (R1; see the `thread_state` module doc).
//!
//! # Published-native `kernel_refs` drift
//!
//! `deref_native_kernel` decrements `kernel_refs` with `saturating_sub`,
//! clamping at 0 under a cross-thread race where `BR_DECREFS` is processed
//! before its matching `BR_INCREFS`. The kernel queues both to
//! `proc->todo` (process-wide FIFO), so distinct binder threads can pop the
//! pair in FIFO order but dispatch out of order if the `BR_INCREFS` thread
//! is preempted before reaching `ref_native_kernel`. The late `BR_INCREFS`
//! then bumps `kernel_refs` from 0 to 1 — the decrement that should have
//! followed was already spent.
//!
//! Each occurrence on an id adds one to `kernel_refs`' over-count against
//! the kernel's true ref count. The accumulation is **unbounded** over the
//! binder's lifetime if races recur, leaving the entry stranded with
//! `kernel_refs >= 1` after the kernel has fully released; the bound is one
//! stranded entry per long-lived published binder that ever raced. This is
//! accepted over a `debug_assert` panic: the race is a property of kernel
//! scheduling, not of this bookkeeping, so panicking would fail CI on a
//! legitimate interleaving. A signed counter with a dual-direction removal
//! trigger could bound the drift but introduces premature-removal hazards
//! in multi-pair scenarios; left as a follow-up.
//!
//! # Context manager security context
//!
//! AOSP always registers the context manager with
//! `FLAT_BINDER_FLAG_TXN_SECURITY_CTX`; `selinux_available` decides whether
//! `become_context_manager` does. The flag makes the kernel deliver
//! `BR_TRANSACTION_SEC_CTX` to the context manager — without it
//! `get_calling_sid()` is always `None` there. But `binder_transaction` then
//! calls `security_secid_to_secctx()` for every transaction to that node and
//! fails the whole transaction with `BR_FAILED_REPLY` when that errors, which
//! it does on a kernel without SELinux: every call into the service manager
//! would fail. Android always has SELinux; on Linux a mounted selinuxfs is the
//! signal, the same check libselinux's `is_selinux_enabled()` makes.
//!
//! The same check decides whether `thread_state` keeps a delivered context at
//! all. The kernel copies only the context's `len` bytes, and of the LSMs that
//! produce one only SELinux counts the NUL in `len`; Smack and AppArmor do
//! not, so the bytes after their label belong to other data in the buffer.
//!
//! # Tests
//!
//! - `test_strong_proxy_under_same_thread_dead_binder_no_deadlock`: the
//!   cfg(test) `slow_path_p2` hook calls `send_obituary_for_handle` on the
//!   same thread right after P1 releases the lock and before P2's IPC. Driving
//!   a real obituary would need a service crashing mid-transaction, and the
//!   lock semantics under test do not depend on the driver. The test is
//!   wall-clock bounded, so a deadlock fails as a CI timeout instead of a
//!   hang. The `fired` flag asserts the hook ran: the singleton `ProcessState`
//!   is shared with parallel tests, and a sibling holding an `Arc` for handle 0
//!   keeps the cache `Weak` upgradeable, so P1 would return at case (c)
//!   without firing the hook (a vacuous pass).

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs::File;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{self, Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::Duration;

use crate::death::{DeathCmd, DeathRegistry};
use crate::freeze::{ClearDone, FreezeCmd, FreezeRegistry};
use crate::{binder::*, error::*, proxy::*, sys::binder, thread_state};

/// A handle's shared `BC_INCREFS`; `Drop` write-locks `handle_to_proxy`, so never drop under it.
pub(crate) struct HandlePin {
    handle: u32,
    generation: u64,
    /// Set by the pin's first commit; module doc "Proxy counting".
    counted: OnceLock<CountedPin>,
}

/// What `on_proxy_create` charged, for the matching `on_proxy_drop` (AOSP `mTrackedUid`).
struct CountedPin {
    uid: u32,
    by_uid: bool,
}

impl HandlePin {
    /// Caller has issued and flushed the `BC_INCREFS` this pin releases.
    fn new(handle: u32, generation: u64) -> Arc<Self> {
        Arc::new(Self {
            handle,
            generation,
            counted: OnceLock::new(),
        })
    }

    /// AOSP `BpBinder::create`'s count, once per pin; P3 calls it under its `CallbackDeferGuard`.
    fn count_once(&self) {
        self.counted.get_or_init(|| {
            // Outside a transaction this is this process's own uid, as in AOSP.
            let uid = thread_state::get_calling_uid();
            CountedPin {
                uid,
                by_uid: crate::proxy_count::on_proxy_create(uid),
            }
        });
    }

    /// Test-only: a pin with no `BC_INCREFS` behind it; the caller must never let it drop.
    #[cfg(test)]
    pub(crate) fn synthetic(handle: u32, generation: u64) -> Arc<Self> {
        Self::new(handle, generation)
    }

    pub(crate) fn handle(&self) -> u32 {
        self.handle
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

impl Drop for HandlePin {
    /// AOSP `~BpBinder`: count, then `expungeHandle` and `decWeakHandle` (`BpBinder.cpp:796-834`).
    fn drop(&mut self) {
        if let Some(counted) = self.counted.get() {
            crate::proxy_count::on_proxy_drop(counted.uid, counted.by_uid);
        }
        let Some(this) = ProcessState::instance().get() else {
            return;
        };
        {
            let mut handle_to_proxy = this
                .handle_to_proxy
                .write()
                .expect("Handle to proxy lock poisoned");
            expunge_locked(&mut handle_to_proxy, self.handle, self);
        }
        let count = {
            let mut ledger = this.pin_ledger_lock();
            ledger.hold(self.handle);
            ledger.take_releasable(self.handle)
        };
        release_pins(self.handle, count);
    }
}

/// P1's case decision, consumed by P2/P3; see module doc "Proxy cache slow path".
enum SlowPathPlan {
    /// Case (a): this thread's fresh pin; dropping it unused releases its `BC_INCREFS`.
    CaseA { pin: Arc<HandlePin> },
    /// Case (b): the entry's pin, held through P3 so its `BC_INCREFS` stays; P2 makes no query.
    CaseB { pin: Arc<HandlePin> },
}

/// Outcome of P1: a live entry (slow path done) or a [`SlowPathPlan`] for P2/P3.
enum SlowPathDecision {
    /// Case (c): another thread inserted or upgraded the entry before P1 took the write lock.
    Cached(SIBinder),
    /// Sub-cases (a)/(b) — proceed to P2 (IPC) and P3 (commit).
    NeedIpc(SlowPathPlan),
}

/// P2 output; each case carries its pin and descriptor into P3.
enum SlowPathReady {
    /// Case (a): descriptor freshly obtained by P2's `query_interface`.
    CaseA {
        pin: Arc<HandlePin>,
        descriptor: String,
    },
    /// Case (b): only held, not read; P3 commits on the entry's live pin, this one or newer.
    CaseB { _pin: Arc<HandlePin> },
}

/// Restores the thread's [`CallRestriction`] on drop, so P2's internal IPC cannot leak it.
struct RestoreCallRestriction(CallRestriction);

impl Drop for RestoreCallRestriction {
    fn drop(&mut self) {
        thread_state::set_call_restriction(self.0);
    }
}

// Fired at P2 entry, lock released, so a test can re-enter `send_obituary_for_handle` there.
#[cfg(test)]
type SlowPathP2TestHook = Box<dyn FnMut(u32)>;

#[cfg(test)]
thread_local! {
    static SLOW_PATH_P2_TEST_HOOK: std::cell::RefCell<Option<SlowPathP2TestHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_slow_path_p2_test_hook(hook: Option<SlowPathP2TestHook>) {
    SLOW_PATH_P2_TEST_HOOK.with(|h| *h.borrow_mut() = hook);
}

#[cfg(test)]
fn slow_path_p2_test_hook(handle: u32) {
    // Take the closure out first so a re-entrant hook can't double-borrow the RefCell.
    let hook = SLOW_PATH_P2_TEST_HOOK.with(|h| h.borrow_mut().take());
    if let Some(mut hook) = hook {
        hook(handle);
        SLOW_PATH_P2_TEST_HOOK.with(|h| *h.borrow_mut() = Some(hook));
    }
}

/// P3 `BC_ACQUIRE` on a live `pin` + insert; caller holds the write lock (module doc).
fn commit_new_acquired(
    handle_to_proxy: &mut HashMap<u32, CacheEntry>,
    pin: &Arc<HandlePin>,
    descriptor: String,
    stability: Stability,
) -> Result<SIBinder> {
    let arc = ProxyHandle::new_acquired(pin, descriptor.clone(), stability)?;
    pin.count_once();
    handle_to_proxy.insert(
        pin.handle(),
        CacheEntry {
            weak: Arc::downgrade(&arc),
            pin: Arc::downgrade(pin),
            descriptor,
        },
    );
    Ok(SIBinder::from_arc(arc as Arc<dyn IBinder>))
}

/// Per-handle proxy cache entry; see module doc "Proxy cache entry".
pub(crate) struct CacheEntry {
    pub(crate) weak: sync::Weak<ProxyHandle>,
    /// Upgrading it under the lock is AOSP `attemptIncWeak`; never drop the result there.
    pub(crate) pin: sync::Weak<HandlePin>,
    pub(crate) descriptor: String,
}

/// Removes `handle`'s entry iff it still names `pin`; `true` if removed.
fn expunge_locked(
    handle_to_proxy: &mut HashMap<u32, CacheEntry>,
    handle: u32,
    pin: *const HandlePin,
) -> bool {
    // A newer pin may have replaced it, as AOSP `expungeHandle` checks `e->binder == binder`.
    let ours = handle_to_proxy
        .get(&handle)
        .is_some_and(|entry| std::ptr::eq(entry.pin.as_ptr(), pin));
    if ours {
        handle_to_proxy.remove(&handle);
    }
    ours
}

/// Per-handle dropped pins, held while death- or freeze-notification clears are in flight.
#[derive(Default)]
struct PinLedger(HashMap<u32, LedgerSlot>);

/// Which notification a clear belongs to; the counts are kept apart so a done matches its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClearKind {
    Death,
    Freeze,
}

#[derive(Default)]
struct LedgerSlot {
    /// `BC_CLEAR_DEATH_NOTIFICATION`s queued whose `BR_CLEAR_DEATH_NOTIFICATION_DONE` is pending.
    clears_in_flight: u32,
    /// The same for `BC_CLEAR_FREEZE_NOTIFICATION`: the kernel drops a registration with its ref.
    freeze_clears_in_flight: u32,
    /// Dropped `HandlePin`s whose `BC_DECREFS` has not been sent.
    held_pins: u32,
}

impl LedgerSlot {
    fn in_flight(&mut self, kind: ClearKind) -> &mut u32 {
        match kind {
            ClearKind::Death => &mut self.clears_in_flight,
            ClearKind::Freeze => &mut self.freeze_clears_in_flight,
        }
    }
}

impl PinLedger {
    fn note_clear(&mut self, handle: u32, kind: ClearKind) {
        let count = self.0.entry(handle).or_default().in_flight(kind);
        *count = count.saturating_add(1);
    }

    fn hold(&mut self, handle: u32) {
        let slot = self.0.entry(handle).or_default();
        slot.held_pins = slot.held_pins.saturating_add(1);
    }

    /// Pins to release now: every held one, once no clear is in flight.
    fn take_releasable(&mut self, handle: u32) -> u32 {
        match self.0.get(&handle) {
            Some(slot) if slot.clears_in_flight == 0 && slot.freeze_clears_in_flight == 0 => {
                self.0.remove(&handle).map_or(0, |slot| slot.held_pins)
            }
            _ => 0,
        }
    }

    fn clear_done(&mut self, handle: u32, kind: ClearKind) -> u32 {
        match self.0.get_mut(&handle).map(|slot| slot.in_flight(kind)) {
            Some(count) if *count > 0 => *count -= 1,
            _ => log::warn!("{kind:?} clear done for handle {handle} with no clear in flight"),
        }
        self.take_releasable(handle)
    }
}

/// `BC_DECREFS` for `count` pins; call with no lock held (module doc "Proxy cache entry").
fn release_pins(handle: u32, count: u32) {
    for _ in 0..count {
        if let Err(err) = thread_state::dec_weak_handle(handle) {
            log::error!("BC_DECREFS for handle {handle} failed: {err:?}; its binder_ref leaks");
        }
    }
}

/// The opened driver's binderfs `features` dir, resolved at init so a later cwd change is moot.
fn features_dir(driver_name: &Path) -> Option<PathBuf> {
    let driver = std::fs::canonicalize(driver_name).ok()?;
    driver.parent().map(|dir| dir.join("features"))
}

/// Reads a binderfs `features/<name>` flag; module doc "Freeze notifications".
fn driver_feature_enabled(features_dir: Option<&Path>, name: &str) -> bool {
    // The driver's own binderfs first (Linux mounts vary), then AOSP's fixed path.
    let own = features_dir.map(|dir| dir.join(name));
    let aosp = Path::new("/dev/binderfs/features").join(name);
    own.into_iter()
        .chain(std::iter::once(aosp))
        .find_map(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|value| value.trim() == "1")
}

/// What the binder driver recorded for a process since its last freeze,
/// from [`ProcessState::process_freeze_info`]. AOSP
/// `IPCThreadState::getProcessFreezeInfo`, decoded from
/// `binder_frozen_status_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProcessFreezeInfo {
    /// A synchronous call reached the process while it was frozen (bit 0 of `sync_recv`).
    pub sync_received: bool,
    /// Transactions are still pending in the process (bit 1 of `sync_recv`).
    pub transactions_pending: bool,
    /// A oneway call reached the process while it was frozen (`async_recv`).
    pub async_received: bool,
}

/// Sidecar entry for a published native, keyed by a u64 id; see module doc "Published natives".
pub(crate) struct PublishedNative {
    /// Holds `RefCounter.strong` >= 1 and keeps `Inner<T>` alive while the entry exists.
    pub(crate) binder_pin: SIBinder,
    /// Live `BINDER_TYPE_BINDER` objects for this id across this process's parcel buffers.
    pub(crate) publish_count: u32,
    /// Kernel refs: +1 per `BR_INCREFS`/`BR_ACQUIRE`, -1 per deferred `BR_RELEASE`/`BR_DECREFS`.
    pub(crate) kernel_refs: u32,
    /// Dedup hits whose `incref_publish` is pending; removal needs 0 (module doc).
    pub(crate) pending_reservations: u32,
}

/// How this process treats **blocking** (non-oneway) outgoing binder
/// calls. AOSP `IPCThreadState::CallRestriction`. Set with
/// [`ProcessState::set_call_restriction`] or
/// [`ServeOptions::call_restriction`](crate::ServeOptions).
///
/// rsbinder's own first-sight interface lookup for a new handle (and the
/// handle-0 ping on SDK >= 30) runs with the restriction lifted, so it
/// neither logs nor panics and can block on that peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CallRestriction {
    /// All calls are permitted (the default).
    None,
    /// Log when a blocking call is made.
    ErrorIfNotOneway,
    /// Panic on a blocking call. A `catch_unwind` around a handler turns
    /// the panic into an error reply; AOSP aborts the process instead.
    FatalIfNotOneway,
}

/// The binder thread-pool ceiling [`ProcessState::init_default`] asks for,
/// matching AOSP's `DEFAULT_MAX_BINDER_THREADS`
/// (`frameworks/native/libs/binder/ProcessState.cpp:49`).
///
/// Public because it is the *only* spelling of "the default": `0` means a
/// literal zero to [`ProcessState::init`], not "pick something for me".
pub const DEFAULT_MAX_BINDER_THREADS: u32 = 15;
const DEFAULT_ENABLE_ONEWAY_SPAM_DETECTION: u32 = 1;

/// The largest receive mapping the binder driver honors: it clamps the
/// mapped area to `SZ_4M` (`drivers/android/binder_alloc.c`) without
/// telling the caller. [`ProcessState::init_with_mmap_size`] refuses
/// anything larger rather than let a process believe it asked for 8 MB
/// and got 4.
pub const MAX_BINDER_MMAP_SIZE: usize = 4 * 1024 * 1024;

struct MemoryMap {
    ptr: *mut c_void,
    size: usize,
}
// SAFETY: only Drop's `munmap` goes through `ptr`; see module doc "Receive mapping".
unsafe impl Sync for MemoryMap {}
unsafe impl Send for MemoryMap {}

pub struct ProcessState {
    max_threads: u32,
    driver_name: PathBuf,
    driver: Arc<File>,
    mmap: RwLock<MemoryMap>,
    context_manager: RwLock<Option<SIBinder>>,
    handle_to_proxy: RwLock<HashMap<u32, CacheEntry>>,
    /// Source of `HandlePin::generation`, bumped once per case-(a) pin.
    next_generation: AtomicU64,
    /// Leaf lock; never held across a command. See module doc "Proxy cache entry".
    pin_ledger: Mutex<PinLedger>,
    /// Death-registration slots by handle; module doc "Death links".
    death: Mutex<DeathRegistry>,
    /// Freeze-notification slots by handle; module doc "Freeze notifications".
    freeze: Mutex<FreezeRegistry<sync::Weak<ProxyHandle>>>,
    /// The driver's binderfs `features` dir, from [`features_dir`] at init.
    features_dir: Option<PathBuf>,
    /// `features/freeze_notification`, read once on first use.
    freeze_notification: OnceLock<bool>,
    /// Published natives by the u64 id in `flat_binder_object.binder`; see `PublishedNative`.
    published_natives: RwLock<HashMap<u64, PublishedNative>>,
    /// Monotonic u64 id allocator for `published_natives`.
    next_native_id: AtomicU64,
    disable_background_scheduling: AtomicBool,
    call_restriction: RwLock<CallRestriction>,
    thread_pool_started: AtomicBool,
    thread_pool_seq: AtomicUsize,
    /// `BR_SPAWN_LOOPER` spawns, bumped asynchronously by workers; not tied to any one caller.
    kernel_started_threads: AtomicUsize,
    /// `start_thread_pool` spawns (`0 → 1` via its CAS), kept apart from kernel spawns for tests.
    main_thread_spawned: AtomicUsize,
    pub(crate) current_threads: AtomicUsize,
}

impl ProcessState {
    fn instance() -> &'static OnceLock<ProcessState> {
        static INSTANCE: OnceLock<ProcessState> = OnceLock::new();
        &INSTANCE
    }

    /// Get ProcessState instance.
    /// If ProcessState is not initialized, it will panic.
    /// If you want to initialize ProcessState, use init() or init_default().
    pub fn as_self() -> &'static ProcessState {
        Self::instance()
            .get()
            .expect("ProcessState is not initialized!")
    }

    /// Whether the kernel-binder `ProcessState` singleton has been
    /// initialized (`init`/`init_default` called). Read-only; the way for
    /// code that may run in a pure RPC process (one that never brought up
    /// kernel binder) to check before [`as_self`], which panics there.
    ///
    /// [`as_self`]: ProcessState::as_self
    pub fn is_initialized() -> bool {
        Self::instance().get().is_some()
    }

    /// Set the [`CallRestriction`] for blocking outgoing calls.
    ///
    /// Each thread copies the process value when it first touches binder,
    /// so this applies to the calling thread and to threads that make their
    /// first binder call afterwards. Other threads that already made a
    /// binder call, including thread-pool threads already running, keep
    /// their value. Call it before [`start_thread_pool`](Self::start_thread_pool)
    /// and before other threads use binder. AOSP
    /// `ProcessState::setCallRestriction` aborts when the calling thread
    /// already has an `IPCThreadState`; rsbinder updates that thread instead.
    pub fn set_call_restriction(&self, call_restriction: CallRestriction) {
        {
            let mut self_call_restriction = self
                .call_restriction
                .write()
                .expect("Call restriction lock poisoned");
            *self_call_restriction = call_restriction;
        }
        // After the write guard drops: a first-time `ThreadState::new` reads this lock.
        thread_state::set_call_restriction(call_restriction);
    }

    pub(crate) fn call_restriction(&self) -> CallRestriction {
        *self
            .call_restriction
            .read()
            .expect("Call restriction lock poisoned")
    }

    /// Log what `max_threads` means; as in AOSP, it reaches `BINDER_SET_MAX_THREADS` unchanged.
    fn log_max_threads(max_threads: u32) {
        match max_threads {
            0 => log::info!(
                "binder max threads = 0: the kernel will never ask this process to spawn a \
                 binder thread, so only threads that call `join_thread_pool` serve incoming \
                 transactions (AOSP `setThreadPoolMaxThreadCount(0)`, what a service manager \
                 wants). Pass `DEFAULT_MAX_BINDER_THREADS` for the usual pool."
            ),
            n if n > DEFAULT_MAX_BINDER_THREADS => log::info!(
                "binder max threads = {n}, above the default {DEFAULT_MAX_BINDER_THREADS}; \
                 the pool may grow to {n} kernel-started threads"
            ),
            n => log::info!("binder max threads = {n}"),
        }
    }

    /// Range-check a receive-mapping size and page-round it; see module doc "Receive mapping".
    pub(crate) fn normalized_mmap_size(mmap_size: usize) -> Result<usize> {
        let page = rustix::param::page_size();
        // The page floor also backs `vm_size > 0` in `inner_init`'s SAFETY comment.
        if mmap_size < page || mmap_size > MAX_BINDER_MMAP_SIZE {
            log::error!(
                "binder mmap size {mmap_size} is outside [{page}, {MAX_BINDER_MMAP_SIZE}]; \
                 the driver clamps to 4 MB without saying so, which is why a larger \
                 request is refused here rather than silently shrunk"
            );
            return Err(StatusCode::BadValue);
        }
        Ok(mmap_size.next_multiple_of(page))
    }

    fn inner_init(
        driver_name: &str,
        max_threads: u32,
        mmap_size: usize,
    ) -> std::result::Result<ProcessState, Box<dyn std::error::Error>> {
        Self::log_max_threads(max_threads);

        let vm_size = Self::normalized_mmap_size(mmap_size)?;

        let driver_name = PathBuf::from(driver_name);

        let driver = open_driver(&driver_name, max_threads)?;
        let features_dir = features_dir(&driver_name);

        // SAFETY: live binder fd, `vm_size > 0`, null addr, read-only; unmapped once in Drop.
        let mmap = unsafe {
            let vm_start = rustix::mm::mmap(
                std::ptr::null_mut(),
                vm_size,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::PRIVATE | rustix::mm::MapFlags::NORESERVE,
                &driver,
                0,
            )?;

            (vm_start, vm_size)
        };

        Ok(ProcessState {
            max_threads,
            driver_name,
            driver: driver.into(),
            mmap: RwLock::new(MemoryMap {
                ptr: mmap.0,
                size: mmap.1,
            }),
            context_manager: RwLock::new(None),
            handle_to_proxy: RwLock::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
            pin_ledger: Mutex::new(PinLedger::default()),
            death: Mutex::new(DeathRegistry::default()),
            freeze: Mutex::new(FreezeRegistry::default()),
            features_dir,
            freeze_notification: OnceLock::new(),
            published_natives: RwLock::new(HashMap::new()),
            next_native_id: AtomicU64::new(1),
            disable_background_scheduling: AtomicBool::new(false),
            call_restriction: RwLock::new(CallRestriction::None),
            thread_pool_started: AtomicBool::new(false),
            thread_pool_seq: AtomicUsize::new(1),
            kernel_started_threads: AtomicUsize::new(0),
            main_thread_spawned: AtomicUsize::new(0),
            current_threads: AtomicUsize::new(0),
        })
    }

    /// Initialize `ProcessState` on `driver_name`, asking the kernel for a
    /// binder thread pool of at most `max_threads`.
    ///
    /// `max_threads` is passed to `BINDER_SET_MAX_THREADS` **as written**,
    /// matching AOSP's `setThreadPoolMaxThreadCount`. In particular `0`
    /// means zero: the kernel never asks this process to spawn a binder
    /// thread, so only threads that call
    /// [`join_thread_pool`](Self::join_thread_pool) serve incoming
    /// transactions. That is what a single-threaded service manager wants
    /// (AOSP's `servicemanager` asks for exactly that); it is *not* a way to
    /// spell "the default", which is
    /// [`DEFAULT_MAX_BINDER_THREADS`] — or [`init_default`](Self::init_default),
    /// which also picks the default binder path.
    ///
    /// The pool only ever grows if [`start_thread_pool`](Self::start_thread_pool)
    /// was called, whatever `max_threads` says: the flag it sets gates
    /// kernel-driven spawning too.
    ///
    /// First call wins — a later call with different arguments returns the
    /// existing state unchanged.
    pub fn init(
        driver_name: &str,
        max_threads: u32,
    ) -> std::result::Result<&'static ProcessState, Box<dyn std::error::Error>> {
        Self::init_with_mmap_size(driver_name, max_threads, Self::default_mmap_size())
    }

    /// [`init`](Self::init), plus the size of the buffer area this
    /// process maps to **receive** transactions.
    ///
    /// The widely quoted "1 MB binder limit" is this mapping, not
    /// anything in the protocol: the driver allocates an incoming
    /// transaction's buffer out of the *destination* process's mapping,
    /// so a transaction too large for it comes back to the sender as
    /// [`StatusCode::FailedTransaction`]. Raising it here lets this
    /// process accept larger calls from any peer, an AOSP `libbinder`
    /// one included — the wire is unchanged, which is why there is
    /// nothing to negotiate and nothing for the sender to set.
    ///
    /// Constraints, the driver's except where noted:
    ///
    /// - The size must be between one page and [`MAX_BINDER_MMAP_SIZE`];
    ///   outside that, [`StatusCode::BadValue`]. The driver clamps to
    ///   4 MB silently, so a larger request is refused here instead. The
    ///   floor is the page: the driver sets none of its own but serves
    ///   nothing from less than one page, and `mmap(2)` cannot map less
    ///   than that either. A one-page mapping is legal and carries a
    ///   call of a few KB; it is the caller's to choose.
    /// - It is rounded up to a page boundary, the granularity `mmap(2)`
    ///   works in. [`mmap_size`](Self::mmap_size) reports the rounded
    ///   value.
    /// - **Oneway transactions may use only half of it.** The driver
    ///   reserves the other half so an async flood cannot starve
    ///   synchronous calls.
    /// - Only the address range is reserved up front; pages are faulted
    ///   in as transactions use them, so a 4 MB mapping does not cost
    ///   4 MB of memory.
    ///
    /// Process-wide and set once, like every other `init` argument: the
    /// first call wins and a later one with a different size returns the
    /// existing state unchanged (the entry layer,
    /// [`serve`](crate::serve) / [`Client`](crate::Client), reports that
    /// mismatch as [`StatusCode::BadValue`] rather than passing it over).
    /// Call it before [`start_thread_pool`](Self::start_thread_pool) —
    /// once a pooled thread is serving, the mapping it serves out of is
    /// already fixed.
    pub fn init_with_mmap_size(
        driver_name: &str,
        max_threads: u32,
        mmap_size: usize,
    ) -> std::result::Result<&'static ProcessState, Box<dyn std::error::Error>> {
        let cell = Self::instance();
        if let Some(existing) = cell.get() {
            return Ok(existing);
        }
        // Built outside the cell so a failed init isn't cached (`get_or_try_init` is unstable).
        let instance = Self::inner_init(driver_name, max_threads, mmap_size)?;
        Ok(cell.get_or_init(|| instance))
    }

    /// The receive-mapping size [`init`](Self::init) uses: 1 MB less two
    /// pages, the same expression as AOSP's `BINDER_VM_SIZE`
    /// (`ProcessState.cpp`). Page size is a runtime value, so this is a
    /// function rather than a constant.
    pub fn default_mmap_size() -> usize {
        // Saturating: a page over 512 KB yields 0, which `init_with_mmap_size` refuses.
        (1024usize * 1024).saturating_sub(rustix::param::page_size() * 2)
    }

    /// Initialize `ProcessState` on the default binder path with
    /// [`DEFAULT_MAX_BINDER_THREADS`].
    ///
    /// The path is `DEFAULT_BINDER_PATH` (`/dev/binderfs/binder`), falling
    /// back to `LEGACY_BINDER_PATH` when that does not exist.
    pub fn init_default() -> std::result::Result<&'static ProcessState, Box<dyn std::error::Error>>
    {
        Self::init(Self::default_driver_path(), DEFAULT_MAX_BINDER_THREADS)
    }

    /// The driver path [`init_default`](Self::init_default) would use.
    pub(crate) fn default_driver_path() -> &'static str {
        if Path::new(crate::DEFAULT_BINDER_PATH).exists() {
            crate::DEFAULT_BINDER_PATH
        } else {
            crate::LEGACY_BINDER_PATH
        }
    }

    /// Register `binder` as this process's binder context manager
    /// (`BINDER_SET_CONTEXT_MGR`). First-wins: a second call is a no-op — the
    /// process can only be the context manager for one object, and the kernel
    /// registers the *process*, not a specific binder — so a second call with
    /// a different binder is logged and the passed binder dropped rather than
    /// silently believed to have taken effect.
    pub fn become_context_manager(
        &self,
        binder: SIBinder,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut context_manager = self
            .context_manager
            .write()
            .expect("Context manager lock poisoned");

        if context_manager.is_some() {
            log::warn!(
                "become_context_manager called again; keeping the first registration (no-op)"
            );
            return Ok(());
        }

        // ACCEPTS_FDS stays set, unlike AOSP: `rsb_hub` answers DUMP_TRANSACTION (carries an fd).
        let mut flags = binder::FLAT_BINDER_FLAG_ACCEPTS_FDS;
        // TXN_SECURITY_CTX only where the kernel can honour it — see `selinux_available`.
        if selinux_available() {
            flags |= binder::FLAT_BINDER_FLAG_TXN_SECURITY_CTX;
        }
        let obj = crate::binder_object::FlatBinderObject::new_binder_with_flags(flags).to_uapi();

        if binder::set_context_mgr_ext(&self.driver, obj).is_err() {
            if let Err(e) = binder::set_context_mgr(&self.driver, 0) {
                return Err(format!("Binder ioctl to become context manager failed: {e}").into());
            }
        }
        *context_manager = Some(binder);

        Ok(())
    }

    pub(crate) fn context_manager(&self) -> Option<SIBinder> {
        self.context_manager
            .read()
            .expect("Context manager lock poisoned")
            .clone()
    }

    /// Get binder service manager.
    pub fn context_object(&self) -> Result<SIBinder> {
        self.strong_proxy_for_handle(0)
    }

    /// Get binder from handle.
    /// If the binder is not cached, it will create a new binder.
    pub fn strong_proxy_for_handle(&self, handle: u32) -> Result<SIBinder> {
        self.strong_proxy_for_handle_stability(handle, Default::default())
    }

    pub(crate) fn strong_proxy_for_handle_stability(
        &self,
        handle: u32,
        stability: Stability,
    ) -> Result<SIBinder> {
        // Read-lock fast path: pure Arc::clone, no kernel command.
        if let Some(arc) = self
            .handle_to_proxy
            .read()
            .expect("Handle to proxy lock poisoned")
            .get(&handle)
            .and_then(|e| e.weak.upgrade())
        {
            return Ok(SIBinder::from_arc(arc));
        }

        // Slow path in three lock-decoupled phases: see module doc "Proxy cache slow path".
        loop {
            let plan = match self.slow_path_p1(handle)? {
                SlowPathDecision::Cached(arc) => return Ok(arc),
                SlowPathDecision::NeedIpc(plan) => plan,
            };
            let ready = self.slow_path_p2(handle, plan)?;
            if let Some(binder) = self.slow_path_p3(handle, stability, ready)? {
                return Ok(binder);
            }
        }
    }

    /// P1, under the write lock: decide the sub-case; case (a) pins (`BC_INCREFS` + flush).
    fn slow_path_p1(&self, handle: u32) -> Result<SlowPathDecision> {
        // Write lock though P1 only reads: it serializes the BC_INCREFS issue point.
        let handle_to_proxy = self
            .handle_to_proxy
            .write()
            .expect("Handle to proxy lock poisoned");

        if let Some(entry) = handle_to_proxy.get(&handle) {
            // Case (c): another thread inserted/upgraded since the read-fast-path miss.
            if let Some(arc) = entry.weak.upgrade() {
                return Ok(SlowPathDecision::Cached(SIBinder::from_arc(arc)));
            }
            // Case (b): a `WIBinder` keeps the pin; it backs P3's BC_ACQUIRE (`attemptIncWeak`).
            if let Some(pin) = entry.pin.upgrade() {
                return Ok(SlowPathDecision::NeedIpc(SlowPathPlan::CaseB { pin }));
            }
            // The pin is in its `Drop`, which leaves a replacing entry alone: case (a).
        }

        // Case (a): pin under the lock; see module doc "Proxy cache slow path" for why.
        thread_state::inc_weak_handle(handle)?;
        if let Err(err) = thread_state::flush_commands() {
            log::warn!(
                "BC_INCREFS for handle {handle} failed at flush: {err:?}; \
                 handle is no longer valid in the kernel"
            );
            return Err(StatusCode::DeadObject);
        }
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        Ok(SlowPathDecision::NeedIpc(SlowPathPlan::CaseA {
            pin: HandlePin::new(handle, generation),
        }))
    }

    /// P2, lock released: `ping_binder(0)` (sdk >= 30), then CaseA's `query_interface`.
    fn slow_path_p2(&self, handle: u32, plan: SlowPathPlan) -> Result<SlowPathReady> {
        // Test-only: the same-thread obituary test re-enters the cache here, lock released.
        #[cfg(test)]
        slow_path_p2_test_hook(handle);

        // P2's ping and `query_interface` are internal IPC, so the caller's restriction is lifted.
        let _restore = RestoreCallRestriction(thread_state::call_restriction());
        thread_state::set_call_restriction(CallRestriction::None);
        // An error drops `plan` here, unlocked; a case-(a) pin's `Drop` then releases it.
        if handle == 0 && crate::sdk_at_least(30) {
            thread_state::ping_binder(handle)?;
        }
        match plan {
            SlowPathPlan::CaseA { pin } => Ok(SlowPathReady::CaseA {
                descriptor: thread_state::query_interface(handle)?,
                pin,
            }),
            SlowPathPlan::CaseB { pin } => Ok(SlowPathReady::CaseB { _pin: pin }),
        }
    }

    /// P3: re-check races from P2 and commit; `None` re-runs P1.
    fn slow_path_p3(
        &self,
        handle: u32,
        stability: Stability,
        ready: SlowPathReady,
    ) -> Result<Option<SIBinder>> {
        // Declared first so it drops after the lock: watermark callbacks may re-enter the cache.
        let _proxy_count_defer = crate::proxy_count::CallbackDeferGuard::new();
        // Declared before the guard (and `ready` is a parameter) so every pin drops after the lock.
        let entry_pin: Option<Arc<HandlePin>>;
        let mut handle_to_proxy = self
            .handle_to_proxy
            .write()
            .expect("Handle to proxy lock poisoned");

        let entry = handle_to_proxy.get(&handle);
        // Re-check (c): a concurrent slow path completed during our P2; ours is redundant.
        if let Some(arc) = entry.and_then(|e| e.weak.upgrade()) {
            return Ok(Some(SIBinder::from_arc(arc)));
        }
        let entry_descriptor = entry.map(|e| e.descriptor.clone());
        entry_pin = entry.and_then(|e| e.pin.upgrade());

        let (pin, descriptor) = match (&ready, &entry_pin, entry_descriptor) {
            // Case (b), or another thread's commit during P2: revive the entry on its live pin.
            (_, Some(pin), Some(descriptor)) => (pin, descriptor),
            // Standard case (a), replacing an entry whose pin is in its `Drop`, if any.
            (SlowPathReady::CaseA { pin, descriptor }, _, _) => (pin, descriptor.clone()),
            // An obituary removed the entry, or replaced it with a dying pin: redo P1.
            (SlowPathReady::CaseB { .. }, _, _) => return Ok(None),
        };
        commit_new_acquired(&mut handle_to_proxy, pin, descriptor, stability).map(Some)
    }

    /// The live proxy of the entry that still names `pin`; module doc "Proxy cache entry".
    pub(crate) fn live_proxy_for_pin(pin: &Arc<HandlePin>) -> Option<Arc<ProxyHandle>> {
        // Pin compared before upgrading, so no other pin's proxy is upgraded and dropped here.
        Self::instance()
            .get()?
            .handle_to_proxy
            .read()
            .expect("Handle to proxy lock poisoned")
            .get(&pin.handle())
            .filter(|entry| std::ptr::eq(entry.pin.as_ptr(), Arc::as_ptr(pin)))
            .and_then(|entry| entry.weak.upgrade())
    }

    /// Test-only: the generation of `handle`'s entry while its pin lives.
    #[cfg(test)]
    pub(crate) fn cache_generation_for(&self, handle: u32) -> Option<u64> {
        // The upgraded pin drops after the lock (module doc "Proxy cache entry").
        let pin = self
            .handle_to_proxy
            .read()
            .expect("Handle to proxy lock poisoned")
            .get(&handle)
            .and_then(|e| e.pin.upgrade());
        pin.map(|pin| pin.generation())
    }

    /// Obituary phase 1; R1: call with no `THREAD_STATE`/`BINDER_DEREFS` borrow (module doc).
    pub(crate) fn send_obituary_for_handle(&self, handle: u32) -> Result<()> {
        // Upgrade and remove under one lock: a P3 commit in between would lose its obituary.
        let removed = {
            let mut handle_to_proxy = self
                .handle_to_proxy
                .write()
                .expect("Handle to proxy lock poisoned");
            // Recipients live only on a live proxy; the pin stays with its holders (module doc).
            handle_to_proxy
                .remove(&handle)
                .map(|entry| entry.weak.upgrade())
        };
        let existed = removed.is_some();
        let arc = removed.flatten();
        let who = arc.as_ref().map(|arc| {
            let sibinder = SIBinder::from_arc(arc.clone() as Arc<dyn IBinder>);
            SIBinder::downgrade(&sibinder)
        });

        // Runs user callbacks, so no lock held; idempotent, so a racing double obituary is fine.
        match (arc, who) {
            (Some(arc), Some(who)) => arc.send_obituary(&who)?,
            _ if existed => {
                log::trace!("Object for handle {handle} already destroyed at obituary time");
            }
            _ => log::trace!("Handle {handle} was not in cache during obituary"),
        }

        Ok(())
    }

    /// Obituary phase 2: flush `BC_DEAD_BINDER_DONE` and `send_obituary`'s clear; see module doc.
    pub(crate) fn finish_obituary(&self) -> Result<()> {
        thread_state::flush_commands()
    }

    fn pin_ledger_lock(&self) -> sync::MutexGuard<'_, PinLedger> {
        self.pin_ledger.lock().expect("Pin ledger lock poisoned")
    }

    /// Counts a `BC_CLEAR_DEATH_NOTIFICATION` about to go out; `handle`'s pins wait for its done.
    pub(crate) fn note_death_clear(handle: u32) {
        if let Some(this) = Self::instance().get() {
            this.pin_ledger_lock().note_clear(handle, ClearKind::Death);
        }
    }

    /// `BR_CLEAR_DEATH_NOTIFICATION_DONE`; R1: call with no `THREAD_STATE` borrow held.
    pub(crate) fn death_clear_done(&self, handle: u32) {
        {
            let mut registry = self.death_lock();
            // A link made during the clear asked for this request (module doc "Death links").
            if registry.clear_done(handle) == ClearDone::Rerequest {
                if let Err(err) = thread_state::send_death_request(handle) {
                    registry.abort_request(handle);
                    log::error!(
                        "BC_REQUEST_DEATH_NOTIFICATION for handle {handle} failed: {err:?}; \
                         its recipients will not hear of its death"
                    );
                }
            }
        }
        let count = self.pin_ledger_lock().clear_done(handle, ClearKind::Death);
        release_pins(handle, count);
    }

    fn death_lock(&self) -> sync::MutexGuard<'_, DeathRegistry> {
        self.death.lock().expect("Death registry lock poisoned")
    }

    /// `owner`'s recipient list became non-empty; `Err` means nothing was registered.
    pub(crate) fn link_death(&self, handle: u32, owner: usize) -> Result<()> {
        let mut registry = self.death_lock();
        if registry.link(handle, owner) == Some(DeathCmd::Request) {
            // Flushed under the lock: a clear written after it must not overtake it.
            thread_state::send_death_request(handle)
                .inspect_err(|_| registry.abort_request(handle))?;
        }
        Ok(())
    }

    /// `owner`'s recipient list became empty; queues the clear if no other proxy is linked.
    pub(crate) fn unlink_death(&self, handle: u32, owner: usize) -> Result<()> {
        let mut registry = self.death_lock();
        match registry.unlink(handle, owner) {
            Some(DeathCmd::Clear) => thread_state::clear_death_notification(handle),
            _ => Ok(()),
        }
    }

    /// Counts a `BC_CLEAR_FREEZE_NOTIFICATION` about to go out, as [`Self::note_death_clear`].
    pub(crate) fn note_freeze_clear(handle: u32) {
        if let Some(this) = Self::instance().get() {
            this.pin_ledger_lock().note_clear(handle, ClearKind::Freeze);
        }
    }

    /// Leaf lock but for the freeze commands written under it (module doc "Freeze notifications").
    fn freeze_lock(&self) -> sync::MutexGuard<'_, FreezeRegistry<sync::Weak<ProxyHandle>>> {
        self.freeze.lock().expect("Freeze registry lock poisoned")
    }

    /// Whether the driver reports freeze notifications; module doc "Freeze notifications".
    pub(crate) fn freeze_notification_supported(&self) -> bool {
        *self.freeze_notification.get_or_init(|| {
            driver_feature_enabled(self.features_dir.as_deref(), "freeze_notification")
        })
    }

    /// Registers `callback` for the proxy at address `owner`; R1: no `THREAD_STATE` borrow held.
    pub(crate) fn add_frozen_callback(
        &self,
        handle: u32,
        owner: usize,
        proxy: sync::Weak<ProxyHandle>,
        callback: sync::Weak<dyn FrozenStateChangeCallback>,
    ) -> Result<()> {
        {
            let mut registry = self.freeze_lock();
            if registry.add(handle, owner, proxy, callback) == Some(FreezeCmd::Request) {
                self.request_freeze_locked(&mut registry, handle)?;
            }
        }
        // Hands a state the kernel already reported to the new callback.
        self.deliver_frozen(handle);
        Ok(())
    }

    /// Flushes the request under the registry lock, or forgets the slot.
    fn request_freeze_locked(
        &self,
        registry: &mut FreezeRegistry<sync::Weak<ProxyHandle>>,
        handle: u32,
    ) -> Result<()> {
        thread_state::send_freeze_request(handle).inspect_err(|_| registry.abort_request(handle))
    }

    /// Removes `owner`'s first registration of `callback`; `NameNotFound` if there is none.
    pub(crate) fn remove_frozen_callback(
        &self,
        handle: u32,
        owner: usize,
        callback: &sync::Weak<dyn FrozenStateChangeCallback>,
    ) -> Result<()> {
        let mut registry = self.freeze_lock();
        if registry.remove(handle, owner, callback)? == Some(FreezeCmd::Clear) {
            Self::write_freeze_clear(handle);
            // AOSP flushes eagerly (`RELEASE_LIBBINDER_FREEZE_USE_FLUSH_EAGERLY`).
            if let Err(err) = thread_state::flush_commands_if_alive() {
                log::error!(
                    "BC_CLEAR_FREEZE_NOTIFICATION for handle {handle} not flushed: {err:?}"
                );
            }
        }
        Ok(())
    }

    /// `ProxyHandle::drop` of a proxy that registered callbacks; on a looper the clear waits.
    pub(crate) fn drop_frozen_owner(&self, handle: u32, owner: usize) {
        if self.freeze_lock().drop_owner(handle, owner) == Some(FreezeCmd::Clear) {
            Self::write_freeze_clear(handle);
        }
    }

    /// Queues the clear; a failure stays counted in the ledger, so the pins outlive it anyway.
    fn write_freeze_clear(handle: u32) {
        if let Err(err) = thread_state::clear_freeze_notification(handle) {
            log::error!("BC_CLEAR_FREEZE_NOTIFICATION for handle {handle} failed: {err:?}");
        }
    }

    /// `BR_FROZEN_BINDER` for `handle`; R1: call with no `THREAD_STATE` borrow held.
    pub(crate) fn frozen_state_changed(&self, handle: u32, is_frozen: bool) {
        if self.freeze_lock().state_changed(handle, is_frozen) {
            self.deliver_frozen(handle);
        }
    }

    /// `BR_CLEAR_FREEZE_NOTIFICATION_DONE`; R1: call with no `THREAD_STATE` borrow held.
    pub(crate) fn freeze_clear_done(&self, handle: u32) {
        let outcome = {
            let mut registry = self.freeze_lock();
            let outcome = registry.clear_done(handle);
            // AOSP re-requests here too (`onFrozenStateChangeListenerRemoved`).
            if outcome == ClearDone::Rerequest {
                if let Err(err) = self.request_freeze_locked(&mut registry, handle) {
                    log::error!(
                        "BC_REQUEST_FREEZE_NOTIFICATION for handle {handle} failed: {err:?}; \
                         its callbacks are dropped"
                    );
                }
            }
            outcome
        };
        if outcome == ClearDone::Unexpected {
            log::warn!("BR_CLEAR_FREEZE_NOTIFICATION_DONE for handle {handle} with no clear");
        }
        let count = self.pin_ledger_lock().clear_done(handle, ClearKind::Freeze);
        release_pins(handle, count);
    }

    /// Runs `handle`'s undelivered callbacks, one thread at a time; see `freeze` module doc.
    fn deliver_frozen(&self, handle: u32) {
        loop {
            let owners = self.freeze_lock().pending_owners(handle);
            let mut delivered = false;
            // Upgraded outside the registry lock: dropping one may run `ProxyHandle::drop`.
            for proxy in owners.iter().filter_map(sync::Weak::upgrade) {
                let owner = Arc::as_ptr(&proxy) as usize;
                let batch = self.freeze_lock().begin_batch(handle, owner);
                let Some((token, state, callbacks)) = batch else {
                    continue;
                };
                delivered = true;
                // Held through the callbacks: its last drop removes them (AOSP `onLastStrongRef`).
                let binder = SIBinder::from_arc(proxy as Arc<dyn IBinder>);
                let who = SIBinder::downgrade(&binder);
                for callback in &callbacks {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        callback.on_state_changed(&who, state);
                    }));
                    if let Err(payload) = result {
                        let msg = payload
                            .downcast_ref::<&'static str>()
                            .copied()
                            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                            .unwrap_or("<non-string panic payload>");
                        log::error!(
                            "FrozenStateChangeCallback panicked for handle {handle:X}: {msg}"
                        );
                    }
                }
                self.freeze_lock().end_batch(handle, token);
                // `binder` drops here, after the guard: its drop may take the registry lock.
            }
            if !delivered {
                return;
            }
        }
    }

    /// Freeze or thaw the binder state of process `pid`. AOSP `IPCThreadState::freeze`.
    ///
    /// This sets the kernel binder driver's per-process freeze state; it does
    /// not stop the process's threads, which is the cgroup freezer's job
    /// (Android's cached-app freezer does both). While frozen, the driver
    /// fails synchronous calls into the process with
    /// [`StatusCode::FailedTransaction`] (`BR_FROZEN_REPLY`), queues oneway
    /// calls until it is thawed, and notifies every
    /// [`FrozenStateChangeCallback`] registered on its binders.
    ///
    /// Freezing waits up to `timeout` for the process's in-flight
    /// transactions to finish. If synchronous ones are still pending, the
    /// freeze is rolled back and [`StatusCode::WouldBlock`] (`EAGAIN`) is
    /// returned; call again to retry, as AOSP documents. A zero `timeout`
    /// checks once without waiting. Thawing never waits. `timeout` is
    /// rounded down to milliseconds and capped at `u32::MAX` of them.
    ///
    /// The driver makes no permission check of its own and applies the
    /// call to every binder context the process opened. On Android, SELinux
    /// grants this ioctl to `system_server` alone: every other domain is
    /// limited to the `unpriv_binder_ioctls` allowlist (`system/sepolicy`
    /// `private/domain.te`, `private/system_server.te`). Observing freezes
    /// through [`FrozenStateChangeCallback`] needs no such grant.
    ///
    /// # Errors
    ///
    /// - [`StatusCode::BadValue`] for a `pid` that is not positive or has
    ///   no binder state (it never opened a binder device).
    /// - [`StatusCode::WouldBlock`] as described above.
    /// - `StatusCode::Errno(-EACCES)` on Android outside `system_server`.
    pub fn freeze_process(&self, pid: i32, enable: bool, timeout: Duration) -> Result<()> {
        let pid = u32::try_from(pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or(StatusCode::BadValue)?;
        let info = binder::binder_freeze_info {
            pid,
            enable: enable.into(),
            timeout_ms: u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX),
        };
        binder::freeze(&*self.driver, info).map_err(StatusCode::from)
    }

    /// What the binder driver recorded for process `pid` since its last
    /// freeze. AOSP `IPCThreadState::getProcessFreezeInfo`.
    ///
    /// # Errors
    ///
    /// - [`StatusCode::BadValue`] for a `pid` that is not positive or has no
    ///   binder state.
    /// - `StatusCode::Errno(-EACCES)` on Android outside `system_server`,
    ///   which SELinux grants this ioctl alone, as for
    ///   [`freeze_process`](Self::freeze_process).
    pub fn process_freeze_info(&self, pid: i32) -> Result<ProcessFreezeInfo> {
        let pid = u32::try_from(pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or(StatusCode::BadValue)?;
        let mut info = binder::binder_frozen_status_info {
            pid,
            sync_recv: 0,
            async_recv: 0,
        };
        binder::get_frozen_info(&*self.driver, &mut info).map_err(StatusCode::from)?;
        Ok(ProcessFreezeInfo {
            sync_received: info.sync_recv & 1 != 0,
            transactions_pending: info.sync_recv & 2 != 0,
            async_received: info.async_recv != 0,
        })
    }

    /// Publish a native (dedup by `Arc::ptr_eq`) and return its id; see module doc.
    pub(crate) fn publish_native(&self, arc: Arc<dyn IBinder>) -> u64 {
        // One write lock for dedup + insert: a read-then-write split would duplicate entries.
        let mut map = self
            .published_natives
            .write()
            .expect("Published natives lock poisoned");
        for (existing_id, entry) in map.iter_mut() {
            if Arc::ptr_eq(entry.binder_pin.as_arc(), &arc) {
                // Hold the entry until the matching `acquire`; a wrap to 0 would defeat that.
                entry.pending_reservations = entry.pending_reservations.saturating_add(1);
                return *existing_id;
            }
        }
        // Drive RefCounter.strong 0→1 via SIBinder::from_arc → inc_strong.
        let binder_pin = SIBinder::from_arc(Arc::clone(&arc));
        // native::inc_weak ignores dummy_wi; dropping it never touches RefCounter.weak.
        let dummy_wi = SIBinder::downgrade(&binder_pin);
        arc.inc_weak(&dummy_wi)
            .expect("inc_weak on Arc<dyn IBinder> must not fail");
        let id = self.next_native_id.fetch_add(1, Ordering::Relaxed);
        map.insert(
            id,
            PublishedNative {
                binder_pin,
                publish_count: 0,
                kernel_refs: 0,
                pending_reservations: 0,
            },
        );
        id
    }

    /// `acquire` arm for `BINDER_TYPE_BINDER`; `false` (a caller bug) for an unknown id.
    pub(crate) fn incref_publish(&self, id: u64) -> bool {
        let mut map = self
            .published_natives
            .write()
            .expect("Published natives lock poisoned");
        match map.get_mut(&id) {
            Some(entry) => {
                entry.publish_count += 1;
                // Consume a reservation; best-effort, keyed by id (see `pending_reservations`).
                entry.pending_reservations = entry.pending_reservations.saturating_sub(1);
                true
            }
            None => false,
        }
    }

    /// `release` arm for `BINDER_TYPE_BINDER`; removes the entry at zero, `false` if unknown.
    pub(crate) fn decref_publish(&self, id: u64) -> bool {
        let trigger_remove = {
            let mut map = self
                .published_natives
                .write()
                .expect("Published natives lock poisoned");
            match map.get_mut(&id) {
                Some(entry) => {
                    // Release builds clamp an unpaired release; debug builds assert on it.
                    debug_assert!(
                        entry.publish_count > 0,
                        "decref_publish on id {id} with publish_count == 0 \
                         (unpaired release; check From<&SIBinder> ↔ \
                         FlatBinderObject::release pairing)"
                    );
                    entry.publish_count = entry.publish_count.saturating_sub(1);
                    entry.publish_count == 0
                        && entry.kernel_refs == 0
                        && entry.pending_reservations == 0
                }
                None => return false,
            }
        };
        if trigger_remove {
            self.remove_entry_if_zero(id);
        }
        true
    }

    /// `BR_INCREFS`/`BR_ACQUIRE`/`BR_ATTEMPT_ACQUIRE`: bump `kernel_refs`; `None` if unknown.
    pub(crate) fn ref_native_kernel(&self, id: u64) -> Option<Arc<dyn IBinder>> {
        let mut map = self
            .published_natives
            .write()
            .expect("Published natives lock poisoned");
        let entry = map.get_mut(&id)?;
        entry.kernel_refs += 1;
        Some(Arc::clone(entry.binder_pin.as_arc()))
    }

    /// Deferred `BR_RELEASE`/`BR_DECREFS`: decrement `kernel_refs`, removing the entry at zero.
    pub(crate) fn deref_native_kernel(&self, id: u64) -> Option<Arc<dyn IBinder>> {
        let (arc, trigger_remove) = {
            let mut map = self
                .published_natives
                .write()
                .expect("Published natives lock poisoned");
            let entry = map.get_mut(&id)?;
            // Clamps a BR_DECREFS-before-BR_INCREFS race; see module doc "`kernel_refs` drift".
            entry.kernel_refs = entry.kernel_refs.saturating_sub(1);
            let arc = Arc::clone(entry.binder_pin.as_arc());
            let trigger = entry.publish_count == 0
                && entry.kernel_refs == 0
                && entry.pending_reservations == 0;
            (arc, trigger)
        };
        if trigger_remove {
            self.remove_entry_if_zero(id);
        }
        Some(arc)
    }

    /// Read-only lookup for `BR_TRANSACTION` and a round-trip `BINDER_TYPE_BINDER` read.
    pub(crate) fn lookup_native(&self, id: u64) -> Option<Arc<dyn IBinder>> {
        let map = self
            .published_natives
            .read()
            .expect("Published natives lock poisoned");
        map.get(&id).map(|e| Arc::clone(e.binder_pin.as_arc()))
    }

    /// Removal phase 2: re-check the counters, remove, drop unlocked; see module doc.
    fn remove_entry_if_zero(&self, id: u64) {
        let entry = {
            let mut map = self
                .published_natives
                .write()
                .expect("Published natives lock poisoned");
            let Entry::Occupied(slot) = map.entry(id) else {
                return;
            };
            let e = slot.get();
            if e.publish_count != 0 || e.kernel_refs != 0 || e.pending_reservations != 0 {
                return;
            }
            slot.remove()
        };
        // dec_weak (no side effect) before the drop that may run `Inner<T>::drop`; no BR_* left.
        let arc_for_weak = Arc::clone(entry.binder_pin.as_arc());
        if let Err(e) = arc_for_weak.dec_weak() {
            // Loud like `publish_native`'s `inc_weak`: silence hides a RefCounter.weak underflow.
            log::error!("unpublish_native: dec_weak failed for id {id}: {e:?}");
        }
        drop(entry.binder_pin);
    }

    pub fn disable_background_scheduling(&self, disable: bool) {
        self.disable_background_scheduling
            .store(disable, Ordering::Relaxed);
    }

    pub fn background_scheduling_disabled(&self) -> bool {
        self.disable_background_scheduling.load(Ordering::Relaxed)
    }

    pub fn driver(&self) -> Arc<File> {
        self.driver.clone()
    }

    /// Init-time driver path; `serve`/`Client` compare it to detect a conflicting re-init.
    pub(crate) fn driver_name(&self) -> &std::path::Path {
        &self.driver_name
    }

    /// Init-time `max_threads` (`0` = no kernel-driven spawns); see [`Self::driver_name`].
    pub(crate) fn max_threads(&self) -> u32 {
        self.max_threads
    }

    /// Whether [`start_thread_pool`](Self::start_thread_pool) ran (AOSP `mThreadPoolStarted`).
    pub(crate) fn thread_pool_started(&self) -> bool {
        self.thread_pool_started.load(Ordering::Acquire)
    }

    /// The size of the receive mapping this process actually has, in
    /// bytes — the value passed to
    /// [`init_with_mmap_size`](Self::init_with_mmap_size) rounded up to a
    /// page, or [`default_mmap_size`](Self::default_mmap_size) when the
    /// process was initialized by [`init`](Self::init) /
    /// [`init_default`](Self::init_default).
    pub fn mmap_size(&self) -> usize {
        self.mmap.read().unwrap_or_else(|e| e.into_inner()).size
    }

    /// Addresses of the receive mapping; bounds the `calling_sid` copy ("Receive mapping").
    pub(crate) fn mapped_range(&self) -> std::ops::Range<usize> {
        let mmap = self.mmap.read().unwrap_or_else(|e| e.into_inner());
        let start = mmap.ptr as usize;
        start..start + mmap.size
    }

    /// Start the binder thread pool: spawn one worker now and **enable
    /// kernel-driven spawning** for the rest of the process's life.
    ///
    /// This flips an internal flag that gates *all* dynamic worker creation —
    /// including the kernel's own `BR_SPAWN_LOOPER` requests under load. Until
    /// it is called, binder commands are served only on threads that manually
    /// [`join_thread_pool`](Self::join_thread_pool), and the kernel cannot add
    /// more, so the process is effectively single-threaded.
    ///
    /// Call it from **any process that receives an inbound transaction**: a
    /// service handling incoming calls, or a client that receives a callback, a
    /// notification, an event-driven [`crate::hub::wait_for_service`]
    /// registration, or a [`crate::DeathRecipient`]. Only a fire-and-forget
    /// client that makes synchronous outbound calls and receives nothing back
    /// can safely skip it. Idempotent — the first call wins, later calls are
    /// no-ops.
    ///
    /// # Panics
    ///
    /// Panics if the process state has not been initialized — call
    /// [`ProcessState::init`](Self::init) or
    /// [`init_default`](Self::init_default) first.
    ///
    /// Calling it after `init(.., 0)` warns: the pool is enabled but pinned
    /// at the single worker spawned here, because a `max_threads` of zero
    /// tells the kernel never to ask for more.
    ///
    /// Call this before any thread joins the pool. A `BR_SPAWN_LOOPER` that
    /// arrives before this call is dropped, as in AOSP `spawnPooledThread`
    /// (`frameworks/native/libs/binder/ProcessState.cpp`, the TODO about a
    /// late `startThreadPool`). The kernel counts that request in
    /// `requested_threads` and decrements it only on `BC_REGISTER_LOOPER`, so
    /// once one is dropped it never requests another pooled thread, even after
    /// this call. The sequence is `init_default()`, then `join_thread_pool()`
    /// receiving a transaction, then a late `start_thread_pool()`. This
    /// function itself is CAS-idempotent.
    pub fn start_thread_pool() {
        let this = Self::as_self();
        if this
            .thread_pool_started
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            // Mirror of AOSP `checkExpectingThreadPoolStart` (pool sized but never started).
            if this.max_threads == 0 {
                log::warn!(
                    "start_thread_pool() with max_threads = 0: one worker is spawned, but the \
                     kernel will never ask for another, so the pool cannot grow under load. \
                     Pass DEFAULT_MAX_BINDER_THREADS (or any non-zero ceiling) to \
                     ProcessState::init if this process serves incoming transactions."
                );
            }
            this.spawn_pooled_thread(true);
        }
    }

    fn make_binder_thread_name(&self) -> String {
        let seq = self.thread_pool_seq.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let driver_name = self
            .driver_name
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_owned())
            .unwrap_or("BINDER".to_owned());
        format!("{driver_name}:{pid}_{seq:X}")
    }

    pub(crate) fn spawn_pooled_thread(&self, is_main: bool) {
        if self.thread_pool_started.load(Ordering::Relaxed) {
            let name = self.make_binder_thread_name();
            log::info!("Spawning new pooled thread, name={name}");
            match thread::Builder::new().name(name).spawn(move || {
                // The `JoinHandle` is dropped, so a looper's exit error is only visible here.
                if let Err(e) = thread_state::join_thread_pool(is_main) {
                    log::error!("pooled binder thread exited with {e}");
                }
            }) {
                Ok(_) => {
                    // Count only real spawns, per origin (see `main_thread_spawned` docs).
                    if is_main {
                        self.main_thread_spawned.fetch_add(1, Ordering::SeqCst);
                    } else {
                        self.kernel_started_threads.fetch_add(1, Ordering::SeqCst);
                    }
                }
                Err(e) => {
                    log::error!("failed to spawn pooled binder thread: {e}");
                }
            }
        }
        // Dropped before the pool starts, as in AOSP; see `start_thread_pool` rustdoc.
    }

    pub fn strong_ref_count_for_node(&self, node: &ProxyHandle) -> Result<usize> {
        let mut info = binder::binder_node_info_for_ref {
            handle: node.handle(),
            strong_count: 0,
            weak_count: 0,
            reserved1: 0,
            reserved2: 0,
            reserved3: 0,
        };

        binder::get_node_info_for_ref(&self.driver, &mut info).inspect_err(|&e| {
            log::error!("Binder ioctl(BINDER_GET_NODE_INFO_FOR_REF) failed: {e:?}");
        })?;
        Ok(info.strong_count as usize)
    }

    pub fn join_thread_pool() -> Result<()> {
        thread_state::join_thread_pool(true)
    }
}

fn open_driver(
    driver: &Path,
    max_threads: u32,
) -> std::result::Result<File, Box<dyn std::error::Error>> {
    let fd = File::options()
        .read(true)
        .write(true)
        .open(driver)
        .map_err(|e| format!("Opening '{}' failed: {}\n", driver.to_string_lossy(), e))?;

    let mut vers = binder::binder_version {
        protocol_version: 0,
    };

    binder::version(&fd, &mut vers)
        .map_err(|e| format!("Binder ioctl to obtain version failed: {e}"))?;
    log::info!("Binder driver protocol version: {}", vers.protocol_version);

    if vers.protocol_version != binder::BINDER_CURRENT_PROTOCOL_VERSION as i32 {
        return Err(format!(
            "Binder driver protocol({}) does not match user space protocol({})!",
            vers.protocol_version,
            binder::BINDER_CURRENT_PROTOCOL_VERSION
        )
        .into());
    }

    binder::set_max_threads(&fd, max_threads)
        .map_err(|e| format!("Binder ioctl to set max threads failed: {e}"))?;
    log::info!("Binder driver max threads set to {max_threads}");

    let enable = DEFAULT_ENABLE_ONEWAY_SPAM_DETECTION;
    if let Err(e) = binder::enable_oneway_spam_detection(&fd, enable) {
        log::warn!("Binder ioctl to enable oneway spam detection failed: {e}")
    }

    Ok(fd)
}

/// Kernel can attach SELinux contexts; see module doc "Context manager security context".
pub(crate) fn selinux_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        cfg!(target_os = "android") || std::path::Path::new("/sys/fs/selinux/enforce").exists()
    })
}

impl Drop for ProcessState {
    fn drop(self: &mut ProcessState) {
        // No panic here (init-race losers drop too); a poisoned POD `ptr`/`size` is still valid.
        let mmap = self.mmap.read().unwrap_or_else(|e| e.into_inner());
        // SAFETY: `inner_init`'s own live mapping, unmapped once here; nothing outlives `self`.
        unsafe {
            if let Err(e) = rustix::mm::munmap(mmap.ptr, mmap.size) {
                log::error!("ProcessState::drop: munmap failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AOSP `DEFAULT_MAX_BINDER_THREADS` (`ProcessState.cpp:49`): a default `init` may exceed.
    #[test]
    fn the_default_thread_ceiling_matches_aosp() {
        assert_eq!(DEFAULT_MAX_BINDER_THREADS, 15);
    }

    /// A removed entry's pin is released at once with no clear in flight, else at the last done.
    #[test]
    fn pin_ledger_releases_after_the_last_clear_completes() {
        let mut ledger = PinLedger::default();

        // Never linked: the drop's pin goes out immediately.
        ledger.hold(7);
        assert_eq!(ledger.take_releasable(7), 1);
        assert!(ledger.0.is_empty(), "a released handle leaves no slot");

        // Linked: the drop's clear holds the pin until its done.
        ledger.note_clear(7, ClearKind::Death);
        ledger.hold(7);
        assert_eq!(ledger.take_releasable(7), 0);
        // A second clear (an earlier unlink) still in flight keeps it held.
        ledger.note_clear(7, ClearKind::Death);
        assert_eq!(ledger.clear_done(7, ClearKind::Death), 0);
        assert_eq!(ledger.clear_done(7, ClearKind::Death), 1);
        assert!(ledger.0.is_empty());

        // Handles are independent; a stray done releases nothing it does not hold.
        ledger.note_clear(1, ClearKind::Death);
        ledger.hold(1);
        ledger.hold(2);
        assert_eq!(ledger.take_releasable(2), 1);
        assert_eq!(ledger.clear_done(3, ClearKind::Death), 0);
        assert_eq!(ledger.clear_done(1, ClearKind::Death), 1);
        assert!(ledger.0.is_empty());
    }

    /// The flag next to the opened driver node decides, before AOSP's fixed path.
    #[test]
    fn driver_feature_flag_is_read_next_to_the_driver_node() {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "rsbinder-features-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let features = root.join("features");
        std::fs::create_dir_all(&features).expect("temp dir");
        let driver = root.join("binder");
        std::fs::write(&driver, b"").expect("driver stand-in");

        std::fs::write(features.join("on"), b"1\n").expect("flag");
        std::fs::write(features.join("off"), b"0\n").expect("flag");
        let dir = features_dir(&driver);
        assert_eq!(dir, Some(features.canonicalize().expect("canonical")));
        assert!(driver_feature_enabled(dir.as_deref(), "on"));
        // A local `0` decides even if the AOSP path says otherwise.
        assert!(!driver_feature_enabled(dir.as_deref(), "off"));
        // Missing locally: the answer is AOSP's fixed path, whatever this host has there.
        let aosp = std::fs::read_to_string("/dev/binderfs/features/freeze_notification")
            .is_ok_and(|value| value.trim() == "1");
        assert_eq!(
            driver_feature_enabled(dir.as_deref(), "freeze_notification"),
            aosp
        );
        assert_eq!(driver_feature_enabled(None, "freeze_notification"), aosp);

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    /// A freeze clear holds the pin like a death clear; one kind's done never frees the other's.
    #[test]
    fn pin_ledger_holds_pins_for_freeze_clears_too() {
        let mut ledger = PinLedger::default();
        ledger.note_clear(4, ClearKind::Freeze);
        ledger.note_clear(4, ClearKind::Death);
        ledger.hold(4);
        assert_eq!(ledger.take_releasable(4), 0);
        assert_eq!(
            ledger.clear_done(4, ClearKind::Death),
            0,
            "freeze still in flight"
        );
        // A done of the wrong kind is a stray: it must not count down the freeze clear.
        assert_eq!(ledger.clear_done(4, ClearKind::Death), 0);
        assert_eq!(ledger.clear_done(4, ClearKind::Freeze), 1);
        assert!(ledger.0.is_empty());
    }

    /// A pin's drop removes only an entry naming that pin (AOSP `expungeHandle`'s check).
    #[test]
    fn expunge_removes_only_the_dropping_pins_entry() {
        // Only addresses are compared, so no `HandlePin` (and no `BC_DECREFS`) is needed.
        let pin: sync::Weak<HandlePin> = sync::Weak::new();
        let ours = pin.as_ptr();
        let entry = || CacheEntry {
            weak: sync::Weak::new(),
            pin: pin.clone(),
            descriptor: String::new(),
        };
        let other = std::ptr::NonNull::<HandlePin>::dangling()
            .as_ptr()
            .cast_const();
        assert!(!std::ptr::eq(ours, other));

        let mut map = HashMap::new();
        assert!(
            !expunge_locked(&mut map, 5, ours),
            "absent: nothing to remove"
        );

        map.insert(5, entry());
        assert!(
            !expunge_locked(&mut map, 5, other),
            "a newer pin's entry: keep it"
        );
        assert!(map.contains_key(&5));

        assert!(expunge_locked(&mut map, 5, ours));
        assert!(!map.contains_key(&5));
    }

    /// Plan 10-1 AC-1.1; driver-free, since the size is decided before the driver opens.
    #[test]
    fn a_receive_mapping_size_is_range_checked_then_page_rounded() {
        let page = rustix::param::page_size();

        // Rounding the default must be identity, or `init` would disagree with its argument.
        let default = ProcessState::default_mmap_size();
        assert_eq!(default, (1024 * 1024) - page * 2);
        assert_eq!(ProcessState::normalized_mmap_size(default), Ok(default));

        // Both ends are refused, not shrunk; the floor also keeps a 0 default out of `mmap`.
        assert_eq!(
            ProcessState::normalized_mmap_size(0),
            Err(StatusCode::BadValue)
        );
        assert_eq!(
            ProcessState::normalized_mmap_size(page - 1),
            Err(StatusCode::BadValue)
        );
        // One page is legal: the driver serves a buffer from it.
        assert_eq!(ProcessState::normalized_mmap_size(page), Ok(page));
        assert_eq!(
            ProcessState::normalized_mmap_size(MAX_BINDER_MMAP_SIZE),
            Ok(MAX_BINDER_MMAP_SIZE)
        );
        assert_eq!(
            ProcessState::normalized_mmap_size(MAX_BINDER_MMAP_SIZE + 1),
            Err(StatusCode::BadValue)
        );

        // Partial pages round up; the ceiling is a page multiple, so rounding never passes it.
        assert_eq!(ProcessState::normalized_mmap_size(page + 1), Ok(page * 2));
        assert_eq!(
            ProcessState::normalized_mmap_size(MAX_BINDER_MMAP_SIZE - 1),
            Ok(MAX_BINDER_MMAP_SIZE)
        );
    }

    /// Shared init checks; not `#[serial]`: callers already hold the `binder` serial lock.
    fn assert_process_state_initialized() {
        let process = ProcessState::init_default().expect("init_default");
        // `init_default` passes the default explicitly; no sentinel is rewritten into it.
        assert_eq!(process.max_threads, DEFAULT_MAX_BINDER_THREADS);
        assert_eq!(
            process.driver_name,
            PathBuf::from(crate::DEFAULT_BINDER_PATH)
        );
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_process_state() {
        assert_process_state_initialized();
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_process_state_context_object() {
        let process = ProcessState::init_default().expect("init_default");
        assert!(process.context_object().is_ok());
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_process_state_strong_proxy_for_handle() {
        let process = ProcessState::init_default().expect("init_default");
        assert!(process.strong_proxy_for_handle(0).is_ok());
    }

    /// N threads on uncached handle 0 converge on one entry and one `Arc` (P3 race table).
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_concurrent_strong_proxy_same_handle_returns_same_arc() {
        let _ = ProcessState::init_default();
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| ProcessState::as_self().strong_proxy_for_handle(0)))
            .collect();
        let arcs: Vec<SIBinder> = handles
            .into_iter()
            .map(|h| {
                h.join()
                    .expect("thread panic")
                    .expect("strong_proxy failed")
            })
            .collect();
        let first = &arcs[0];
        for a in &arcs[1..] {
            assert_eq!(
                first, a,
                "concurrent slow-path winners must share a single Arc"
            );
        }
        // Exactly one cache entry for this handle.
        let map = ProcessState::as_self()
            .handle_to_proxy
            .read()
            .expect("Handle to proxy lock poisoned");
        assert!(
            map.contains_key(&0),
            "case (a) winner must have installed an entry for handle 0"
        );
    }

    /// Case (b) revives one `Arc` and the generation while a `WIBinder` lives; then case (a).
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_concurrent_strong_proxy_case_b_resurrection() {
        let _ = ProcessState::init_default();
        let lookup_from_8_threads = || -> SIBinder {
            let handles: Vec<_> = (0..8)
                .map(|_| std::thread::spawn(|| ProcessState::as_self().strong_proxy_for_handle(0)))
                .collect();
            let arcs: Vec<SIBinder> = handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .expect("thread panic")
                        .expect("strong_proxy failed")
                })
                .collect();
            for a in &arcs[1..] {
                assert_eq!(&arcs[0], a, "concurrent lookups must produce a single Arc");
            }
            arcs[0].clone()
        };

        // The count follows the pin, as AOSP's follows the BpBinder (module doc "Proxy counting").
        let uid = thread_state::get_calling_uid();
        let was_by_uid = crate::proxy_count::is_count_by_uid_enabled();
        crate::proxy_count::enable_count_by_uid(true);
        let count = || {
            (
                crate::proxy_count::get_binder_proxy_count(),
                crate::proxy_count::get_binder_proxy_count_for_uid(uid),
            )
        };
        let (base, base_uid) = count();

        let initial = ProcessState::as_self()
            .strong_proxy_for_handle(0)
            .expect("initial strong_proxy failed");
        let initial_gen = ProcessState::as_self()
            .cache_generation_for(0)
            .expect("entry must exist for handle 0");
        assert_eq!(
            count(),
            (base + 1, base_uid + 1),
            "a committed pin counts once"
        );
        let weak = SIBinder::downgrade(&initial);
        // A clean process holds no other handle-0 `Arc`, so this is the last proxy.
        drop(initial);
        assert!(
            weak.upgrade().is_err(),
            "WIBinder::upgrade never resurrects"
        );
        assert_eq!(
            count(),
            (base + 1, base_uid + 1),
            "a pin only a WIBinder keeps stays counted, as a BpBinder a wp keeps"
        );
        assert_eq!(
            ProcessState::as_self().cache_generation_for(0),
            Some(initial_gen),
            "a live WIBinder keeps the entry, as a wp<BpBinder> keeps its cache slot"
        );

        let resurrected = lookup_from_8_threads();
        assert_eq!(
            ProcessState::as_self().cache_generation_for(0),
            Some(initial_gen),
            "case (b) resurrection must preserve the entry's generation"
        );
        assert!(
            weak == resurrected,
            "the WIBinder names the resurrected proxy"
        );
        assert_eq!(
            weak.upgrade().as_ref(),
            Ok(&resurrected),
            "the WIBinder upgrades to the proxy revived on its pin, as AOSP promote() after force_set"
        );
        assert_eq!(
            count(),
            (base + 1, base_uid + 1),
            "a case (b) revival is not counted again"
        );

        drop(resurrected);
        drop(weak);
        assert_eq!(
            ProcessState::as_self().cache_generation_for(0),
            None,
            "the last proxy and WIBinder gone, the pin's drop removes the entry"
        );
        assert_eq!(
            count(),
            (base, base_uid),
            "the pin's drop posts the decrement"
        );
        let rebuilt = lookup_from_8_threads();
        let rebuilt_gen = ProcessState::as_self()
            .cache_generation_for(0)
            .expect("the lookups must have installed an entry for handle 0");
        assert_ne!(
            rebuilt_gen, initial_gen,
            "a fresh case (a) takes a new generation"
        );
        assert_eq!(
            count(),
            (base + 1, base_uid + 1),
            "8 racing lookups count one pin, uncommitted spares none"
        );
        drop(rebuilt);
        assert_eq!(count(), (base, base_uid));
        crate::proxy_count::enable_count_by_uid(was_by_uid);
    }

    /// A same-thread obituary between P1 and P2 takes the lock, no deadlock; module doc "Tests".
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_strong_proxy_under_same_thread_dead_binder_no_deadlock() {
        let process = ProcessState::init_default().expect("init_default");

        // Seed and drop handle 0; the lookup then runs P1-P3 as case (a).
        let seed = process
            .strong_proxy_for_handle(0)
            .expect("seed strong_proxy_for_handle(0) must succeed");
        drop(seed);

        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_w = std::sync::Arc::clone(&fired);

        let (tx, rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            // The hook is thread-local: install it on the thread that runs the lookup.
            super::set_slow_path_p2_test_hook(Some(Box::new(move |handle| {
                fired_w.store(true, std::sync::atomic::Ordering::SeqCst);
                ProcessState::as_self()
                    .send_obituary_for_handle(handle)
                    .expect("send_obituary_for_handle from P2 hook must not fail");
            })));
            let r = ProcessState::as_self().strong_proxy_for_handle(0);
            super::set_slow_path_p2_test_hook(None);
            tx.send(r).expect("result channel must not drop");
        });

        // Wallclock bound: a lock-structure regression fails here as a timeout, not a hang.
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("strong_proxy_for_handle must complete within 5s — deadlock regression");
        join.join().expect("worker thread must not panic");

        assert!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            "P2 hook never fired — P1 short-circuited at case (c), most likely \
             because a parallel test held an Arc for handle 0 and kept the \
             cache Weak upgradeable. Test passed vacuously."
        );

        // Guards the deadlock only; an obituary on a live handle 0 may still fail the lookup.
        match result {
            Ok(_arc) => {}
            Err(StatusCode::DeadObject) => {}
            Err(other) => panic!("unexpected slow-path result: {other:?}"),
        }
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_process_state_disable_background_scheduling() {
        let process = ProcessState::init_default().expect("init_default");
        process.disable_background_scheduling(true);
        assert!(process.background_scheduling_disabled());
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn set_call_restriction_reaches_a_thread_that_already_used_binder() {
        let process = ProcessState::init_default().expect("init_default");
        let prev = process.call_restriction();
        // Creates this thread's `THREAD_STATE`, which copies the process value.
        assert_eq!(thread_state::call_restriction(), prev);

        process.set_call_restriction(CallRestriction::ErrorIfNotOneway);
        assert_eq!(
            process.call_restriction(),
            CallRestriction::ErrorIfNotOneway
        );
        assert_eq!(
            thread_state::call_restriction(),
            CallRestriction::ErrorIfNotOneway
        );

        process.set_call_restriction(prev);
        assert_eq!(thread_state::call_restriction(), prev);
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_process_state_start_thread_pool() {
        // `main_thread_spawned`, not `kernel_started_threads`: leftover loopers can't race it.
        assert_process_state_initialized();
        let process = ProcessState::as_self();
        let was_started = process.thread_pool_started.load(Ordering::SeqCst);
        let before = process.main_thread_spawned.load(Ordering::SeqCst);
        ProcessState::start_thread_pool();
        assert!(process.thread_pool_started.load(Ordering::SeqCst));
        let after = process.main_thread_spawned.load(Ordering::SeqCst);
        if was_started {
            assert_eq!(after, before);
        } else {
            assert_eq!(after, before + 1);
        }
    }

    /// Driver-free native `IBinder` with no-op ref counts, for the `published_natives` tests.
    struct MockNative;

    impl IBinder for MockNative {
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
        fn as_transactable(&self) -> Option<&dyn crate::Transactable> {
            None
        }
        fn descriptor(&self) -> &str {
            "rsbinder.test.MockNative"
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

    /// Only `binder_pin` holds the `Arc`; the entry lives until both counters reach 0.
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_native_uaf_window_closed() {
        let process = ProcessState::init_default().expect("init_default");
        let arc: Arc<dyn IBinder> = Arc::new(MockNative);

        let id = process.publish_native(Arc::clone(&arc));
        assert!(
            process.incref_publish(id),
            "incref on freshly published id must succeed"
        );

        // Only the table's binder_pin keeps the inner Arc alive from here on.
        drop(arc);

        // BR_INCREFS / BR_ACQUIRE: kernel_refs goes 0→1→2.
        assert!(process.ref_native_kernel(id).is_some());
        assert!(process.ref_native_kernel(id).is_some());
        // BR_RELEASE: kernel_refs 2→1; entry still alive (publish_count=1).
        assert!(process.deref_native_kernel(id).is_some());
        assert!(
            process.lookup_native(id).is_some(),
            "entry must remain while publish_count > 0"
        );

        // Parcel::release_objects → decref_publish: publish_count 1→0; kernel_refs still 1.
        assert!(process.decref_publish(id));
        assert!(
            process.lookup_native(id).is_some(),
            "entry must remain while kernel_refs > 0"
        );

        // BR_DECREFS: kernel_refs 1→0. Both zero → entry removed.
        assert!(process.deref_native_kernel(id).is_some());
        assert!(
            process.lookup_native(id).is_none(),
            "entry must be removed after both counts hit zero"
        );

        // Subsequent unknown-id ops are graceful.
        assert!(!process.incref_publish(id));
        assert!(!process.decref_publish(id));
        assert!(process.ref_native_kernel(id).is_none());
        assert!(process.deref_native_kernel(id).is_none());
    }

    /// Publishing one `Arc` twice dedups to one id, alive until the last `release`.
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_native_dedup_same_arc() {
        let process = ProcessState::init_default().expect("init_default");
        let arc: Arc<dyn IBinder> = Arc::new(MockNative);

        let id1 = process.publish_native(Arc::clone(&arc));
        let id2 = process.publish_native(Arc::clone(&arc));
        assert_eq!(id1, id2, "publishing the same Arc twice must dedup");

        // Two parcel slots share the id: two `release`s are needed before the entry drops.
        assert!(process.incref_publish(id1));
        assert!(process.incref_publish(id1));

        assert!(process.decref_publish(id1));
        assert!(
            process.lookup_native(id1).is_some(),
            "entry must remain while one parcel slot still holds a ref"
        );

        assert!(process.decref_publish(id1));
        assert!(
            process.lookup_native(id1).is_none(),
            "entry must be removed after the last release fires"
        );

        drop(arc);
    }

    /// Without `pending_reservations` the `decref` removes the entry and the later `incref` fails.
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_native_dedup_reserves_against_concurrent_removal() {
        let process = ProcessState::init_default().expect("init_default");
        let arc: Arc<dyn IBinder> = Arc::new(MockNative);

        // Thread A publishes and serializes (acquire): publish_count = 1.
        let id = process.publish_native(Arc::clone(&arc));
        assert!(process.incref_publish(id));

        // Thread B publishes the same Arc (dedup); its acquire is still pending.
        let id_b = process.publish_native(Arc::clone(&arc));
        assert_eq!(id, id_b);

        // A's release -> publish_count = 0; B's pending dedup acquire must keep the entry.
        assert!(process.decref_publish(id));
        assert!(
            process.lookup_native(id).is_some(),
            "dedup reservation must keep the entry alive across the acquire gap"
        );

        // Thread B's acquire finally lands, consuming the reservation.
        assert!(process.incref_publish(id));
        // Thread B's release -> now genuinely zero -> removed.
        assert!(process.decref_publish(id));
        assert!(process.lookup_native(id).is_none());

        drop(arc);
    }

    /// Distinct `Arc`s of a unit struct get distinct ids: `Arc::ptr_eq` keys on allocation.
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_native_distinct_arcs_get_distinct_ids() {
        let process = ProcessState::init_default().expect("init_default");
        let arc_a: Arc<dyn IBinder> = Arc::new(MockNative);
        let arc_b: Arc<dyn IBinder> = Arc::new(MockNative);
        assert!(!Arc::ptr_eq(&arc_a, &arc_b));

        let id_a = process.publish_native(Arc::clone(&arc_a));
        let id_b = process.publish_native(Arc::clone(&arc_b));
        assert_ne!(id_a, id_b);

        // Cleanup.
        for id in [id_a, id_b] {
            assert!(process.incref_publish(id));
            assert!(process.decref_publish(id));
            assert!(process.lookup_native(id).is_none());
        }
    }

    /// `lookup_native` (`BR_TRANSACTION`/`deserialize_option` round trip) leaves counts unchanged.
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_native_lookup_does_not_change_counts() {
        let process = ProcessState::init_default().expect("init_default");
        let arc: Arc<dyn IBinder> = Arc::new(MockNative);
        let id = process.publish_native(Arc::clone(&arc));

        assert!(process.incref_publish(id)); // publish_count = 1
        assert!(process.ref_native_kernel(id).is_some()); // kernel_refs = 1

        // Look up multiple times — must not affect either counter.
        for _ in 0..5 {
            assert!(process.lookup_native(id).is_some());
        }

        // Decrement both: entry must be removed exactly once.
        assert!(process.decref_publish(id));
        assert!(
            process.lookup_native(id).is_some(),
            "lookup must not have decremented kernel_refs"
        );
        assert!(process.deref_native_kernel(id).is_some());
        assert!(process.lookup_native(id).is_none());

        drop(arc);
    }
}
