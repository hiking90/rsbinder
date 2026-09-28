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
//! `BR_TRANSACTION`/`BR_REPLY` buffers and `calling_sid` out of it), but that
//! access goes through the kernel pointers in the transaction, not `ptr`, and
//! is synchronized by the driver's buffer-lifetime protocol (a buffer stays
//! valid until `BC_FREE_BUFFER`). Because the mapping lives for the
//! singleton's whole lifetime (an init-race loser never serviced a
//! transaction), no such read is outstanding at drop.
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
//!   (a) (entry absent), issues `BC_INCREFS` + `flush_commands` so the cache
//!   pin is live in the kernel before any IPC enters. The write lock is
//!   taken even though P1 only reads, so two concurrent slow paths cannot
//!   both observe "absent" and produce two case-(a) commits with distinct
//!   generations; a second one collapses onto P3's `(CaseA, Some(_))` race
//!   arm instead. Pinning under the lock also means P3's `BC_ACQUIRE` never
//!   races a freed `binder_ref` slot. `flush_commands` inside the lock is
//!   sound because it is a write-only ioctl (`talk_with_driver(false)`,
//!   `read_size = 0`): no `BR_*` — `BR_DEAD_BINDER` included — is
//!   dispatched, and re-entrant `send_obituary_for_handle` paths only
//!   originate from `BR_DEAD_BINDER`.
//! - **P2** — lock released. IPC (`ping_binder` for handle 0 on sdk >= 30,
//!   `query_interface` for case (a)) runs without the lock, so a re-entrant
//!   `BR_DEAD_BINDER` → `send_obituary_for_handle` on the same thread can
//!   take it without deadlocking against `std::sync::RwLock`'s
//!   non-reentrant write lock.
//! - **P3** — write lock re-acquired. Re-checks case (c) and the case
//!   (a)→(b) cross-thread race, undoes any spare pin, and commits the entry.
//!
//! Sub-cases decided in P1:
//!
//! - **(a)** entry absent. P1 pins, and this thread owns the pin until P3
//!   moves it into the new entry or undoes it (`undo_case_a_pin`).
//! - **(b)** entry present but its `weak` dangles. The entry keeps owning its
//!   pin; P1 snapshots descriptor and generation, P2 skips `query_interface`
//!   (the descriptor is immutable for the `binder_ref` slot's lifetime), and
//!   P3 resurrects under the same generation if the snapshot still matches.
//! - **(c)** another thread inserted or upgraded the entry between the
//!   fast-path miss and P1; the live entry is returned.
//!
//! `commit_new_acquired` undoes the pin on a `new_acquired` failure only when
//! this thread owns it (`owns_case_a_pin`): case (b) and the cross-thread
//! `(CaseA, Some(_))` race pass `false`, because the existing entry owns that
//! pin. The pin undo itself is best-effort: if its `BC_DECREFS` or flush
//! fails, the failure is logged and the pin leaks until obituary or process
//! teardown, since returning the secondary error would mask the original one.
//!
//! The P2 lock release matters because the catch-all arm of
//! `wait_for_response` dispatches `BR_DEAD_BINDER` to `execute_command`, which
//! calls `send_obituary_for_handle` and takes the same write lock.
//!
//! P3 covers a race in which another thread T2 ran a complete case (a) during
//! this thread's P2 IPC and then dropped its `Arc`: the plan is `CaseA`, yet
//! the slot is again "present + dangling weak", so there is one spare
//! `BC_INCREFS` pin (this thread's) on top of T2's entry-owned pin. P3 undoes
//! the spare pin and adopts T2's descriptor and generation, restoring one pin
//! per entry. The companion `(CaseB, None)` arm — the entry vanished mid-flight
//! through an obituary — returns `DeadObject` rather than sending `BC_ACQUIRE`
//! against a freed `binder_ref` slot; it is the one window where the
//! precondition "`BC_ACQUIRE` requires a live pin" could otherwise break.
//!
//! P3 race resolution (P2 output × cache state at P3):
//!
//! | ready  | cached at P3 | action                                              |
//! |--------|--------------|-----------------------------------------------------|
//! | (any)  | live entry   | drop this thread's work; if CaseA, undo its pin     |
//! | CaseA  | None         | standard commit; new generation                     |
//! | CaseA  | Some(_)      | undo this thread's pin; commit with cached desc/gen |
//! | CaseB  | None         | DeadObject (cache pin gone — BC_ACQUIRE unsafe)     |
//! | CaseB  | Some, gen=   | resurrect under same generation                     |
//! | CaseB  | Some, gen≠   | adopt new entry's desc/gen                          |
//!
//! # Proxy cache entry
//!
//! A `CacheEntry`'s `weak` lets the process build a fresh `Arc<ProxyHandle>`
//! after the previous one dropped, reusing the cached `descriptor` instead of
//! issuing a new `INTERFACE_TRANSACTION`. The kernel weak ref (`BC_INCREFS`)
//! that keeps `binder_ref(handle)` alive while the user-side strong count is 0
//! is not a field: the entry's presence in `handle_to_proxy` owns it. The pin
//! is acquired exactly once on case-(a) insertion and released exactly once on
//! obituary teardown.
//!
//! `generation` comes from the process-wide monotonic `next_generation`,
//! bumped once per case-(a) insertion (u64, so wrap-around is not a concern).
//! A proxy `WIBinder` records the generation seen at `SIBinder::downgrade`, so
//! its `PartialEq` identity `(handle, generation)` is stable across case-(b)
//! re-lookups (a fresh `Arc<ProxyHandle>` is a new allocation) and a recycled
//! handle id naming a different `binder_node` is distinguishable. Case (b)
//! keeps the entry's generation — same kernel slot, and a fresh wire-delivered
//! strong ref makes it transactable again; only case (a) allocates one.
//!
//! A case-(b) re-lookup through `strong_proxy_for_handle_stability` is driven
//! by a fresh wire delivery of the handle (servicemanager `checkService`, or an
//! incoming transaction carrying it), which gives the new `BC_ACQUIRE` a
//! kernel strong count it can transact on. `WIBinder::upgrade()` differs: it
//! is purely weak and never re-`BC_ACQUIRE`s a strong-0 handle.
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
//! `flat_binder_object::acquire` / `release`, called from
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
//! flag threaded into `flat_binder_object::acquire`, a `binder_object.rs`
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
//! `Parcel::write_object` → `flat_binder_object::acquire` right after brings
//! it to 1. The only leak path is a `Parcel::write_aligned` failure between
//! `From<&SIBinder>` returning and that `acquire()`: a panic (typically OOM),
//! or `Err(BadValue)` when the write would end past `i32::MAX`.
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
//! write lock and notifies death recipients. Phase 2, `release_obituary_pin`,
//! releases the cache pin with `BC_DECREFS`; `thread_state::execute_command`'s
//! `BR_DEAD_BINDER` arm calls it after queueing `BC_DEAD_BINDER_DONE` in this
//! thread's out-parcel.
//!
//! Phase 1 can run on the thread that issued the originating transaction,
//! including one inside `strong_proxy_for_handle_stability`; its
//! `handle_to_proxy.write()` would deadlock if the slow path held the lock
//! across IPC, which is why P2 runs unlocked. It must be called with no
//! `THREAD_STATE` or `BINDER_DEREFS` borrow held: `send_obituary` runs user
//! `DeathRecipient::binder_died` callbacks, which can issue nested binder
//! calls (R1; see the `thread_state` module doc).
//!
//! Phase 2's first `flush_commands()` commits `BC_DEAD_BINDER_DONE` and any
//! `BC_RELEASE` queued on this thread before `BC_DECREFS` reaches the kernel.
//! It does not drain other threads' out-parcels: a `BC_RELEASE` from a `Drop`
//! on another thread can still arrive after the `BC_DECREFS`, and the kernel
//! rejects it with `-EINVAL` and a dmesg line. Closing that window would take
//! cross-thread synchronization of every out-parcel flush, which is not done.
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

use std::collections::HashMap;
use std::fs::File;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{self, Arc, OnceLock, RwLock};
use std::thread;

use crate::{binder::*, error::*, proxy::*, sys::binder, thread_state};

/// Best-effort undo of a case (a) pin; see module doc "Proxy cache slow path".
fn undo_case_a_pin(handle: u32) {
    if let Err(err) = thread_state::dec_weak_handle(handle) {
        log::warn!(
            "Best-effort BC_DECREFS for handle {handle} failed during \
             case (a) cleanup: {err:?}; kernel binder_ref pin may leak \
             until obituary"
        );
        return;
    }
    if let Err(err) = thread_state::flush_commands() {
        log::warn!(
            "Best-effort flush after BC_DECREFS for handle {handle} \
             failed during case (a) cleanup: {err:?}; kernel binder_ref \
             pin may leak until obituary"
        );
    }
}

/// P1's case decision, consumed by P2/P3; see module doc "Proxy cache slow path".
enum SlowPathPlan {
    /// Case (a): this thread owns the P1 pin until P3 moves it into the entry or undoes it.
    CaseA,
    /// Case (b): the entry owns the pin; P2 reuses this snapshot instead of `query_interface`.
    CaseB { descriptor: String, generation: u64 },
}

/// Outcome of P1: a live entry (slow path done) or a [`SlowPathPlan`] for P2/P3.
enum SlowPathDecision {
    /// Case (c): another thread inserted or upgraded the entry before P1 took the write lock.
    Cached(SIBinder),
    /// Sub-cases (a)/(b) — proceed to P2 (IPC) and P3 (commit).
    NeedIpc(SlowPathPlan),
}

/// P2 output; each case carries its descriptor, so P3 never `expect`s an `Option<String>`.
enum SlowPathReady {
    /// Case (a): descriptor freshly obtained by P2's `query_interface`.
    CaseA { descriptor: String },
    /// Case (b): descriptor and generation snapshotted by P1; P2 made no IPC.
    CaseB { descriptor: String, generation: u64 },
}

/// Restores the thread's [`CallRestriction`] on drop, so P2's `ping_binder(0)` cannot leak it.
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

/// P3 `BC_ACQUIRE` + insert, caller holds the write lock; `owns_case_a_pin`: see module doc.
fn commit_new_acquired(
    handle_to_proxy: &mut HashMap<u32, CacheEntry>,
    handle: u32,
    descriptor: String,
    generation: u64,
    stability: Stability,
    owns_case_a_pin: bool,
) -> Result<SIBinder> {
    let arc = match ProxyHandle::new_acquired(handle, generation, descriptor.clone(), stability) {
        Ok(arc) => arc,
        Err(err) => {
            if owns_case_a_pin {
                undo_case_a_pin(handle);
            }
            return Err(err);
        }
    };
    handle_to_proxy.insert(
        handle,
        CacheEntry {
            weak: Arc::downgrade(&arc),
            descriptor,
            generation,
        },
    );
    Ok(SIBinder::from_arc(arc as Arc<dyn IBinder>))
}

/// Per-handle proxy cache entry; its presence owns the pin (module doc "Proxy cache entry").
pub(crate) struct CacheEntry {
    pub(crate) weak: sync::Weak<ProxyHandle>,
    pub(crate) descriptor: String,
    pub(crate) generation: u64,
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
    /// Source of `CacheEntry::generation`, bumped once per case-(a) insertion (fresh pin).
    next_generation: AtomicU64,
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

    pub fn set_call_restriction(&self, call_restriction: CallRestriction) {
        let mut self_call_restriction = self
            .call_restriction
            .write()
            .expect("Call restriction lock poisoned");
        *self_call_restriction = call_restriction;
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
        let obj = binder::flat_binder_object::new_binder_with_flags(flags);

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
        let plan = match self.slow_path_p1(handle)? {
            SlowPathDecision::Cached(arc) => return Ok(arc),
            SlowPathDecision::NeedIpc(plan) => plan,
        };
        let ready = self.slow_path_p2(handle, plan)?;
        self.slow_path_p3(handle, stability, ready)
    }

    /// P1, under the write lock: decide the sub-case; case (a) pins (`BC_INCREFS` + flush).
    fn slow_path_p1(&self, handle: u32) -> Result<SlowPathDecision> {
        // Write lock though P1 only reads: it serializes the BC_INCREFS issue point.
        let handle_to_proxy = self
            .handle_to_proxy
            .write()
            .expect("Handle to proxy lock poisoned");

        // Case (c): another thread inserted/upgraded since the read-fast-path miss.
        if let Some(arc) = handle_to_proxy.get(&handle).and_then(|e| e.weak.upgrade()) {
            return Ok(SlowPathDecision::Cached(SIBinder::from_arc(arc)));
        }

        // Case (b): weak dead; the first-insertion pin still backs P3's BC_ACQUIRE.
        if let Some(entry) = handle_to_proxy.get(&handle) {
            return Ok(SlowPathDecision::NeedIpc(SlowPathPlan::CaseB {
                descriptor: entry.descriptor.clone(),
                generation: entry.generation,
            }));
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
        Ok(SlowPathDecision::NeedIpc(SlowPathPlan::CaseA))
    }

    /// P2, lock released: `ping_binder(0)` (sdk >= 30), then CaseA's `query_interface`.
    fn slow_path_p2(&self, handle: u32, plan: SlowPathPlan) -> Result<SlowPathReady> {
        // Test-only: the same-thread obituary test re-enters the cache here, lock released.
        #[cfg(test)]
        slow_path_p2_test_hook(handle);

        if handle == 0 && crate::sdk_at_least(30) {
            // RAII restore: a ping failure can't leak CallRestriction::None into later calls.
            let _restore = RestoreCallRestriction(thread_state::call_restriction());
            thread_state::set_call_restriction(CallRestriction::None);
            if let Err(err) = thread_state::ping_binder(handle) {
                if matches!(plan, SlowPathPlan::CaseA) {
                    undo_case_a_pin(handle);
                }
                return Err(err);
            }
        }
        match plan {
            SlowPathPlan::CaseA => match thread_state::query_interface(handle) {
                Ok(descriptor) => Ok(SlowPathReady::CaseA { descriptor }),
                Err(err) => {
                    undo_case_a_pin(handle);
                    Err(err)
                }
            },
            SlowPathPlan::CaseB {
                descriptor,
                generation,
            } => Ok(SlowPathReady::CaseB {
                descriptor,
                generation,
            }),
        }
    }

    /// P3: re-check races from P2 and commit; see the table in module doc "Proxy cache slow path".
    fn slow_path_p3(
        &self,
        handle: u32,
        stability: Stability,
        ready: SlowPathReady,
    ) -> Result<SIBinder> {
        // Declared first so it drops after the lock: watermark callbacks may re-enter the cache.
        let _proxy_count_defer = crate::proxy_count::CallbackDeferGuard::new();
        let mut handle_to_proxy = self
            .handle_to_proxy
            .write()
            .expect("Handle to proxy lock poisoned");

        // Re-check (c): a concurrent slow path completed during our P2; ours is redundant.
        if let Some(arc) = handle_to_proxy.get(&handle).and_then(|e| e.weak.upgrade()) {
            if matches!(ready, SlowPathReady::CaseA { .. }) {
                undo_case_a_pin(handle);
            }
            return Ok(SIBinder::from_arc(arc));
        }

        let cached = handle_to_proxy
            .get(&handle)
            .map(|e| (e.descriptor.clone(), e.generation));

        match (ready, cached) {
            (SlowPathReady::CaseA { descriptor }, None) => {
                // Standard case (a): fresh entry, fresh generation.
                let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
                commit_new_acquired(
                    &mut handle_to_proxy,
                    handle,
                    descriptor,
                    generation,
                    stability,
                    true,
                )
            }
            (SlowPathReady::CaseA { .. }, Some((cached_desc, cached_gen))) => {
                // T2 ran a case (a) during our P2 (module doc, P3 race): undo our spare pin.
                undo_case_a_pin(handle);
                commit_new_acquired(
                    &mut handle_to_proxy,
                    handle,
                    cached_desc,
                    cached_gen,
                    stability,
                    false,
                )
            }
            (
                SlowPathReady::CaseB {
                    descriptor,
                    generation,
                },
                Some((_, cached_gen)),
            ) if cached_gen == generation => {
                // Same entry, same generation: the first-insertion pin is still active.
                commit_new_acquired(
                    &mut handle_to_proxy,
                    handle,
                    descriptor,
                    generation,
                    stability,
                    false,
                )
            }
            (SlowPathReady::CaseB { .. }, Some((cached_desc, cached_gen))) => {
                // An obituary plus a new case (a) replaced the entry during P2; follow it.
                commit_new_acquired(
                    &mut handle_to_proxy,
                    handle,
                    cached_desc,
                    cached_gen,
                    stability,
                    false,
                )
            }
            (SlowPathReady::CaseB { .. }, None) => {
                // Obituary may have freed the pin; callers re-resolve as after BR_DEAD_BINDER.
                Err(StatusCode::DeadObject)
            }
        }
    }

    /// Test-only: the cache entry's generation for `handle` (production reads the `ProxyHandle`).
    #[cfg(test)]
    pub(crate) fn cache_generation_for(&self, handle: u32) -> Option<u64> {
        self.handle_to_proxy
            .read()
            .expect("Handle to proxy lock poisoned")
            .get(&handle)
            .map(|e| e.generation)
    }

    /// Obituary phase 1; R1: call with no `THREAD_STATE`/`BINDER_DEREFS` borrow (module doc).
    pub(crate) fn send_obituary_for_handle(&self, handle: u32) -> Result<()> {
        // `downgrade` reads identity off the `ProxyHandle`; reading first only secures a live Arc.
        let arc = {
            let handle_to_proxy = self
                .handle_to_proxy
                .read()
                .expect("Handle to proxy lock poisoned");
            // Recipients exist only on a live proxy (`link_to_death` needs the Arc).
            handle_to_proxy
                .get(&handle)
                .and_then(|entry| entry.weak.upgrade())
        };
        let who = arc.as_ref().map(|arc| {
            let sibinder = SIBinder::from_arc(arc.clone() as Arc<dyn IBinder>);
            SIBinder::downgrade(&sibinder)
        });

        let existed = {
            let mut handle_to_proxy = self
                .handle_to_proxy
                .write()
                .expect("Handle to proxy lock poisoned");
            handle_to_proxy.remove(&handle).is_some()
        };

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

    /// Obituary phase 2: `BC_DECREFS` the pin after `BC_DEAD_BINDER_DONE`; see module doc.
    pub(crate) fn release_obituary_pin(&self, handle: u32) -> Result<()> {
        thread_state::flush_commands()?;
        thread_state::dec_weak_handle(handle)?;
        thread_state::flush_commands()?;
        Ok(())
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
                         flat_binder_object::release pairing)"
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
            let needs_remove = map
                .get(&id)
                .map(|e| e.publish_count == 0 && e.kernel_refs == 0 && e.pending_reservations == 0)
                .unwrap_or(false);
            if !needs_remove {
                return;
            }
            map.remove(&id).expect("just observed Some")
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

    /// The size of the receive mapping this process actually has, in
    /// bytes — the value passed to
    /// [`init_with_mmap_size`](Self::init_with_mmap_size) rounded up to a
    /// page, or [`default_mmap_size`](Self::default_mmap_size) when the
    /// process was initialized by [`init`](Self::init) /
    /// [`init_default`](Self::init_default).
    pub fn mmap_size(&self) -> usize {
        self.mmap.read().unwrap_or_else(|e| e.into_inner()).size
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
fn selinux_available() -> bool {
    cfg!(target_os = "android") || std::path::Path::new("/sys/fs/selinux/enforce").exists()
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

    /// N threads resurrect a dangling handle-0 entry via case (b): one `Arc`, generation kept.
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn test_concurrent_strong_proxy_case_b_resurrection() {
        let _ = ProcessState::init_default();
        // Force a cache entry to exist for handle 0.
        let initial = ProcessState::as_self()
            .strong_proxy_for_handle(0)
            .expect("initial strong_proxy failed");
        let initial_gen = ProcessState::as_self()
            .cache_generation_for(0)
            .expect("entry must exist for handle 0");
        // Drop all strong refs to make `weak` dangling.
        drop(initial);
        // Let other handle-0 Arc holders (e.g. `context_manager`) settle; a clean process has none.
        std::thread::yield_now();

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
                "concurrent case (b) resurrection must produce a single Arc"
            );
        }
        // Case (b) keeps the entry's generation; a fresh case (a) would allocate a new one.
        assert_eq!(
            ProcessState::as_self().cache_generation_for(0),
            Some(initial_gen),
            "case (b) resurrection must preserve the entry's generation"
        );
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

        // Seed and drop handle 0 so the lookup takes case (b), which issues no fresh BC_INCREFS.
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

        // Guards the deadlock only: (CaseB, None) gives DeadObject, a parallel resurrection Ok.
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
